# shellcheck shell=bash
# MN5 (design section 17.2, ADR 0028 §11; T20, T33, T34): SIGKILL of the worker's
# engine (rank 1, host B) while the group is Ready, per engine:
#   run_row.sh MN5 --tag vllm -- vllm
#   run_row.sh MN5 --tag sglang -- sglang
#   run_row.sh MN5 --tag tensorfold -- tensorfold
# MN6 kills the head's engine instead (rows/MN6.sh). The signal goes only to the
# member's recorded identity (pid, start ticks, boot id) from the server's ledger
# (fault.sh member, signal_owned.py); MN_FAULT_SIGNAL (default KILL) and
# MN_FAULT_ROLE (default api) choose what is sent to which recorded process.
# Setup as MN1 (rows/MN1.sh).
#
# Expected (the fixtures use `recovery: reconcile`, ADR 0028 §11):
#   stop     the plan that was Ready stops: its failed_rank is the killed rank with
#            group_member_failed, and every member of it settles on its own host
#            with the journal entries recorded by each host.
#   again    the group comes back on a new plan generation: under reconcile the
#            server relaunches it by itself once the old plan has settled (found
#            live 2026-10-15 with SGLang); a start is sent only when the
#            deployment rests failed or stopped. It answers, and stops with
#            verified cleanup on both hosts (every identity recorded at either
#            Ready gone, no engine, GPU process or rendezvous directory, the
#            rendezvous port and engine ports free, nothing held); delete.
# shellcheck source=scripts/live/matrix/grouplib.sh
. "$MATRIX_DIR/grouplib.sh"

MN_FAULT_SIGNAL=${MN_FAULT_SIGNAL:-KILL}
MN_FAULT_ROLE=${MN_FAULT_ROLE:-api}

# plan_settled <deployment> <generation>: that plan and every member of it are settled.
plan_settled() {
  python3 "$MATRIX_DIR/ledger.py" group --db "$SERVER_DB" --deployment "$1" 2>/dev/null | python3 -c 'import json,sys
gen=int(sys.argv[1])
try: d=json.load(sys.stdin)
except Exception: sys.exit(1)
plans=[p for p in d["plans"] if p.get("generation")==gen]
members=[m for m in d["members"] if m.get("generation")==gen]
ok=bool(plans) and all(p.get("state")=="settled" for p in plans) and bool(members) and all(m.get("state")=="settled" for m in members)
print(json.dumps({"generation": gen, "plans": [p.get("state") for p in plans], "members": {m.get("rank"): m.get("state") for m in members}}))
sys.exit(0 if ok else 1)' "$2"
}

# wait_plan_settled <deployment> <generation> <timeout_s>
wait_plan_settled() {
  local dep=$1 gen=$2 timeout=$3 i last=""
  dry && { log_cmd control-host "poll ledger.py group $dep until plan $gen and its members are settled (<= ${timeout}s)"; return 0; }
  for ((i = 0; i < timeout; i += 2)); do
    if last=$(plan_settled "$dep" "$gen"); then echo "plan $gen settled after ${i}s: $last"; return 0; fi
    sleep 2
  done
  echo "plan $gen of $dep not settled after ${timeout}s: $last" >&2
  return 1
}

# ready_generation <deployment> <label>: the last plan generation in group-<dep>-<label>.json.
ready_generation() {
  if dry; then echo 1; return 0; fi
  python3 -c 'import json,sys; print((json.load(open(sys.argv[1]))["plans"] or [{}])[-1].get("generation") or "")' "$EVID/group-$1-$2.json"
}

# failure_check <deployment> <label> <rank> <generation>: that plan failed at that rank.
failure_check() {
  dry && { log_cmd control-host "judge group-$1-$2.json: plan $4 failed_rank $3, group_member_failed"; return 0; }
  python3 - "$EVID/group-$1-$2.json" "$EVID/status-$1-$2.json" "$3" "$4" <<'PY'
import json, sys
gen = int(sys.argv[4])
plan = next((p for p in json.load(open(sys.argv[1]))["plans"] if p.get("generation") == gen), {})
d = json.load(open(sys.argv[2])); d = d.get("deployment", d)
rank = int(sys.argv[3])
findings = []
if plan.get("failed_rank") != rank:
    findings.append(f"failed_rank {plan.get('failed_rank')}, want {rank}")
if plan.get("failure_code") != "group_member_failed":
    findings.append(f"failure_code {plan.get('failure_code')}")
print(json.dumps({"observed_state": d.get("observed_state"), "plan_generation": gen,
                  "failed_rank": plan.get("failed_rank"), "failure_code": plan.get("failure_code"),
                  "findings": findings}))
sys.exit(1 if findings else 0)
PY
}

# new_plan_check <deployment> <before-label> <after-label>: a later plan generation.
new_plan_check() {
  dry && { log_cmd control-host "judge group-$1-$2.json against group-$1-$3.json: a later plan generation"; return 0; }
  python3 - "$EVID/group-$1-$2.json" "$EVID/group-$1-$3.json" <<'PY'
import json, sys
a, b = ((json.load(open(p))["plans"] or [{}])[-1].get("generation") for p in sys.argv[1:])
print(json.dumps({"plan_generation": [a, b], "new_plan": a is not None and b is not None and b > a}))
sys.exit(0 if a is not None and b is not None and b > a else 1)
PY
}

member_kill_row() { # member_kill_row <engine> <host code a|b>
  local engine=$1 target dep rank host rc=0
  target=$(short_host "$2")
  rank=1; [ "$target" = "$HOST_A" ] && rank=0
  dep=$(group_fixture_name "$engine")
  echo "engine $engine deployment $dep kill rank $rank on $target signal $MN_FAULT_SIGNAL role $MN_FAULT_ROLE" | tee -a "$EVID/timeline.txt"
  step preflight group_preflight "$engine" || return 1
  for host in "${GROUP_HOSTS[@]}"; do step "idle-$(host_short "$host")" host_idle "$host" || return 1; done
  step fixture group_fixture "$engine" || return 1

  step deploy timed deploy deploy "$dep" --activate --wait || {
    step status-failed group_status "$dep" failed
    for host in "${GROUP_HOSTS[@]}"; do step "errors-$(host_short "$host")" engine_errors "$host"; done
    return 1
  }
  step ready keep_group "$dep" ready
  step ready-check group_ready_check "$dep" ready || rc=1
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1

  local gen1
  gen1=$(ready_generation "$dep" ready)
  dry || echo "kill $(now_ms)" >>"$EVID/marks.txt"
  step kill fault member "$target" "$dep" "$MN_FAULT_SIGNAL" "$MN_FAULT_ROLE" || return 1
  # The system stop after an engine exit settles both members of the plan; an
  # ordinary stop is sent only if it has not within the bound, and the row records
  # that it was. A plan that never settles ends the row (the trap cleans up).
  if ! step settled timed settled wait_plan_settled "$dep" "$gen1" 600; then
    rc=1
    step status-unsettled group_status "$dep" unsettled
    step ledger-unsettled group_ledger "$dep" unsettled
    step stop-after-kill stop_dep "$dep"
    step settled-after-stop wait_plan_settled "$dep" "$gen1" 300 || {
      step evidence-unsettled evidence "$dep" unsettled
      return 1
    }
  fi
  step killed keep_group "$dep" killed
  step status-settled group_status "$dep" settled
  step ledger-settled group_ledger "$dep" settled
  step failure failure_check "$dep" settled "$rank" "$gen1" || rc=1
  step evidence evidence "$dep" settled
  step journal journal_hosts "$dep" settled

  case $(dry || group_state "$dep") in
    failed|stopped) step restart timed restart start_dep "$dep" --wait || rc=1 ;;
  esac
  step restarted timed restarted wait_states "$dep" ready 1800 || rc=1
  step ready-2 keep_group "$dep" ready-2
  step ready-check-2 group_ready_check "$dep" ready-2 || rc=1
  step new-plan new_plan_check "$dep" ready ready-2 || rc=1
  step infer-2 infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  group_stop_settle "$dep" stopped || rc=1
  sleep 3
  step cleanup-gen1 group_cleanup_check "$dep" gen1 ready || rc=1
  step cleanup-gen2 group_cleanup_check "$dep" gen2 ready-2 || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() { member_kill_row "${1:?engine: vllm, sglang or tensorfold}" b; }
