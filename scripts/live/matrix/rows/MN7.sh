# shellcheck shell=bash
# MN7 (design section 17.2, ADR 0028 §11; T20, T34; SPEC §11): host B's agent
# service stops while the group is Ready, then returns:
#   run_row.sh MN7 --tag vllm -- vllm        (one engine; default vllm)
# The agent is stopped the way the harness stops a host role: SIGTERM to the
# `start host` process recorded in its pid file (roles.sh host-down b, through
# signal_owned.py), then roles.sh host-up b. No firewall or interface is touched.
# Stopping the agent leaves its engines running (SPEC §4.3). MN7_HOLD_S (default
# 120) is how long the row holds the outage after the member turned uncertain.
# Setup as MN1 (rows/MN1.sh). If the row fails while the agent is down, its trap
# starts the agent again before it deletes the group.
#
# Expected:
#   uncertain  once the server sees host B gone, status shows rank 1 `uncertain`
#              (group_member_uncertain) with its reservation still charged, and the
#              ledger still holds host B's member charge; after MN7_HOLD_S it is
#              still uncertain and charged (no timeout releases it).
#   settles    after the agent returns and reconnects, rank 1 leaves `uncertain` on
#              host B's own evidence: the group either serves again (a correct
#              answer) or settles its members; the row records which.
#   stop       stop with verified cleanup on both hosts; delete.
# shellcheck source=scripts/live/matrix/grouplib.sh
. "$MATRIX_DIR/grouplib.sh"

MN7_HOLD_S=${MN7_HOLD_S:-120}
MN7_AGENT_DOWN=0

# A failure while host B's agent is down: bring it back first, so the trap's
# `delete deployment --stop` can reach host B's member.
eval "mn7_$(declare -f cleanup_failed_row)"
cleanup_failed_row() {
  if [ "$MN7_AGENT_DOWN" = 1 ] && ! dry; then
    step trap-agent-up "$MATRIX_DIR/roles.sh" host-up b || true
    step trap-online "$MATRIX_DIR/roles.sh" wait-online 180 || true
    MN7_AGENT_DOWN=0
  fi
  mn7_cleanup_failed_row
}

hold_outage() { # hold_outage <seconds>
  dry && { log_cmd control-host "hold the outage ${1}s"; return 0; }
  sleep "$1"
}

# uncertain_check <deployment> <label>: rank 1 uncertain and still charged.
uncertain_check() {
  dry && { log_cmd control-host "judge status-$1-$2.json and group-$1-$2.json: rank 1 uncertain, reservation held, member charge held"; return 0; }
  python3 - "$EVID/status-$1-$2.json" "$EVID/group-$1-$2.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d = d.get("deployment", d)
g = json.load(open(sys.argv[2]))
members = {m.get("node_rank"): m for i in d.get("instances", []) for m in i.get("members", [])}
m = members.get(1) or {}
owners = [o for o in g["held"]["member_owners"] if o.endswith("/member:1")]
ledger = [x for x in g["members"] if x["rank"] == 1 and x["state"] != "settled"]
findings = []
if m.get("state") != "uncertain":
    findings.append(f"rank 1 state {m.get('state')}")
if not m.get("reservation"):
    findings.append("rank 1 reservation released")
if not owners:
    findings.append("rank 1 member charge released in the ledger")
print(json.dumps({"observed_state": d.get("observed_state"), "rank1": {k: m.get(k) for k in ("state", "reservation", "last_error", "processes")},
                  "rank0_state": (members.get(0) or {}).get("state"), "ledger_rank1": [x["state"] for x in ledger],
                  "charges": owners, "findings": findings}))
sys.exit(1 if findings else 0)
PY
}

row_main() {
  local engine=${1:-vllm} dep host state rc=0
  dep=$(group_fixture_name "$engine")
  echo "engine $engine deployment $dep agent $HOST_B hold ${MN7_HOLD_S}s" | tee -a "$EVID/timeline.txt"
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

  dry || echo "agent-down $(now_ms)" >>"$EVID/marks.txt"
  MN7_AGENT_DOWN=1
  step agent-down "$MATRIX_DIR/roles.sh" host-down b || return 1
  step offline host_offline_wait "$HOST_B" 180 || rc=1
  step uncertain timed uncertain wait_member "$dep" 1 uncertain 300 || rc=1
  step keep-uncertain keep_group "$dep" uncertain
  step uncertain-check uncertain_check "$dep" uncertain || rc=1
  # Recorded, not judged: whether the members' processes outlived the agent, and
  # what a request meets during the outage.
  step alive-uncertain alive_check "$HOST_A" "$EVID/members-$dep-ready.json"
  step infer-during infer "$dep" "What is 17+25? Answer with only the number." --max-tokens 64 --timeout 60
  step hold hold_outage "$MN7_HOLD_S"
  step keep-held keep_group "$dep" held
  step held-check uncertain_check "$dep" held || rc=1

  dry || echo "agent-up $(now_ms)" >>"$EVID/marks.txt"
  step agent-up "$MATRIX_DIR/roles.sh" host-up b || return 1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  MN7_AGENT_DOWN=0
  step returned timed returned wait_member "$dep" 1 '!uncertain' 600 || rc=1
  step keep-returned keep_group "$dep" returned
  step evidence-returned evidence "$dep" returned
  step journal-returned journal_hosts "$dep" returned

  state=ready
  dry || state=$(group_state "$dep")
  echo "after the agent returned: $dep $state" | tee -a "$EVID/timeline.txt"
  if [ "$state" = ready ]; then
    step infer-after infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
    group_stop_settle "$dep" stopped || rc=1
  else
    step settled wait_group_settled "$dep" 600 || {
      step stop-unsettled stop_dep "$dep"
      step settled-after-stop wait_group_settled "$dep" 300 || rc=1
    }
    step status-settled group_status "$dep" stopped
  fi
  step evidence evidence "$dep" stopped
  step journal journal_hosts "$dep" stopped
  sleep 3
  step cleanup group_cleanup_check "$dep" stopped ready || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
