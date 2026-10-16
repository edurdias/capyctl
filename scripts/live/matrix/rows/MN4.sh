# shellcheck shell=bash
# MN4 (design section 17.2, ADR 0028 §12; T16, T21, T34): five deep park and wake
# cycles of a two-rank group, per engine:
#   run_row.sh MN4 --tag vllm -- vllm
#   run_row.sh MN4 --tag sglang -- sglang
# MN4_CYCLES (default 5) sets the count. A wake is a routed request to the parked
# route (no operator start); MN4_WAKE=start wakes with `start deployment --wait`
# instead. Setup as MN1 (rows/MN1.sh).
#
# Expected, every cycle:
#   park    `park deployment` settles parked; on each host its member's reservation
#           drops to the parked phase (fewer bytes than at Ready) and MemAvailable
#           returns at least PARK_MIN_RELEASE (0.5) of that host's Ready drop; every
#           member process (pid, start ticks, boot id) stays alive on its host.
#   wake    the request is answered correctly; Ready again with the same processes,
#           both reservations back to the ready phase and bytes, MemAvailable down
#           again on both hosts.
#   canary  the wake canary's reference tokens (ADR 0028 §12: 8 tokens at
#           temperature 0, recorded at first readiness) are unchanged, no
#           group_wake_mismatch is recorded, and the evidence names them per cycle
#           (the owner reviews the exact tokens).
#   one collective per cycle: the server journal gains exactly one `parked` and one
#           `restored` entry per cycle (the head's one collective each way).
# Then stop with verified cleanup on both hosts; delete.
#
# SGLang runs the same cycles: ADR 0028 note of 2026-10-16 lifts the 0.1.3
# restart_only rule for SGLang groups, so the fixture (no residency) resolves deep.
# shellcheck source=scripts/live/matrix/grouplib.sh
. "$MATRIX_DIR/grouplib.sh"

MN4_CYCLES=${MN4_CYCLES:-5}
MN4_WAKE=${MN4_WAKE:-request}

# cycle_check <deployment> <n>: per-member residency of cycle n against Ready.
cycle_check() {
  dry && { log_cmd control-host "judge status-$1-ready/parked-$2/woken-$2.json: per-member reservation parked below ready, then ready again"; return 0; }
  python3 - "$EVID/status-$1-ready.json" "$EVID/status-$1-parked-$2.json" "$EVID/status-$1-woken-$2.json" <<'PY'
import json, sys
def members(path):
    d = json.load(open(path)); d = d.get("deployment", d)
    inst = (d.get("instances") or [{}])[0]
    return d.get("observed_state"), {m.get("node_rank"): m for m in inst.get("members", [])}
def res(m):
    r = (m or {}).get("reservation") or {}
    return r.get("phase"), int(r.get("bytes") or 0)
(_, ready), (ps, parked), (ws, woken) = (members(p) for p in sys.argv[1:])
findings, rows = [], {}
if ps != "parked":
    findings.append(f"parked status observed {ps}")
if ws != "ready":
    findings.append(f"woken status observed {ws}")
for rank in sorted(ready):
    r, p, w = res(ready[rank]), res(parked.get(rank)), res(woken.get(rank))
    rows[rank] = {"ready": r, "parked": p, "woken": w}
    if p[0] != "parked" or not p[1] < r[1]:
        findings.append(f"rank {rank} parked reservation {p} against ready {r}")
    if w != r:
        findings.append(f"rank {rank} woken reservation {w} against ready {r}")
print(json.dumps({"members": rows, "findings": findings}))
sys.exit(1 if findings else 0)
PY
}

# canary_check <deployment> <label>: the reference recorded at Ready (or by the
# first wake, when Ready could not record one) is unchanged at <label>, on the
# same plan, with no failure code.
canary_check() {
  dry && { log_cmd control-host "judge group-$1-ready.json against group-$1-$2.json: same plan generation and canary tokens, no failure_code"; return 0; }
  python3 - "$EVID/group-$1-ready.json" "$EVID/group-$1-$2.json" <<'PY'
import json, sys
def plan(path):
    plans = json.load(open(path))["plans"]
    return plans[-1] if plans else {}
a, b = (plan(p) for p in sys.argv[1:])
ref, now = (a.get("canary") or {}).get("tokens"), (b.get("canary") or {}).get("tokens")
findings = []
if a.get("generation") != b.get("generation"):
    findings.append(f"plan generation {a.get('generation')} -> {b.get('generation')}")
if now is None:
    findings.append("no canary reference recorded")
elif ref is not None and ref != now:
    findings.append("canary reference changed")
if b.get("failure_code"):
    findings.append(f"failure_code {b.get('failure_code')}")
print(json.dumps({"reference_at_ready": ref, "reference_now": now, "prompt": (b.get("canary") or {}).get("prompt"),
                  "findings": findings}))
sys.exit(1 if findings else 0)
PY
}

# collective_check <deployment> <n> <parked-base> <restored-base>: the journal has
# exactly n more `parked` and n more `restored` entries than at Ready.
collective_check() {
  local dep=$1 n=$2 p r
  dry && { log_cmd control-host "judge evidence-$dep-woken-$n.json: parked = base + $n, restored = base + $n"; return 0; }
  p=$(journal_count "$dep" "woken-$n" parked)
  r=$(journal_count "$dep" "woken-$n" restored)
  echo "journal parked $p (base $3) restored $r (base $4) after cycle $n"
  [ "$p" = $(($3 + n)) ] && [ "$r" = $(($4 + n)) ]
}

wake() { # wake <deployment>
  if [ "$MN4_WAKE" = start ]; then start_dep "$1" --wait; else
    infer "$1" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024; fi
}

row_main() {
  local engine=${1:?engine: vllm or sglang} dep host n rc=0 base_p=0 base_r=0
  case $engine in vllm | sglang) ;; *) echo "MN4 parks vllm or sglang groups (TensorFold has no deep park)"; return 1 ;; esac
  dep=$(group_fixture_name "$engine")
  echo "engine $engine deployment $dep cycles $MN4_CYCLES wake $MN4_WAKE" | tee -a "$EVID/timeline.txt"
  step preflight group_preflight "$engine" || return 1
  for host in "${GROUP_HOSTS[@]}"; do step "idle-$(host_short "$host")" host_idle "$host" || return 1; done
  step fixture group_fixture "$engine" || return 1
  step mem-before mem_both before

  step deploy timed deploy deploy "$dep" --activate --wait || {
    step status-failed group_status "$dep" failed
    for host in "${GROUP_HOSTS[@]}"; do step "errors-$(host_short "$host")" engine_errors "$host"; done
    return 1
  }
  step ready keep_group "$dep" ready
  step ready-check group_ready_check "$dep" ready || rc=1
  step mem-ready mem_both ready
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step evidence-ready evidence "$dep" ready
  if ! dry; then
    base_p=$(journal_count "$dep" ready parked); base_r=$(journal_count "$dep" ready restored)
  fi

  for ((n = 1; n <= MN4_CYCLES; n++)); do
    step "park-$n" timed "park-$n" park_dep "$dep" || rc=1
    step "parked-$n" wait_states "$dep" parked 600 || {
      rc=1
      for host in "${GROUP_HOSTS[@]}"; do step "park-errors-$n-$(host_short "$host")" engine_errors "$host"; done
      step "status-park-failed-$n" group_status "$dep" "park-failed-$n"
      break
    }
    sleep 3
    step "keep-parked-$n" keep_group "$dep" "parked-$n"
    step "alive-parked-$n" alive_check "$HOST_A" "$EVID/members-$dep-ready.json" || rc=1
    step "same-parked-$n" same_identities "$EVID/members-$dep-ready.json" "$EVID/members-$dep-parked-$n.json" || rc=1
    step "mem-parked-$n" mem_both "parked-$n"
    for host in a b; do
      step "released-$host-$n" mem_release "$host-before" "$host-ready" "$host-parked-$n" "$PARK_MIN_RELEASE" || rc=1
    done

    step "wake-$n" timed "wake-$n" wake "$dep" || rc=1
    step "woken-$n" wait_states "$dep" ready 600 || {
      rc=1
      for host in "${GROUP_HOSTS[@]}"; do step "wake-errors-$n-$(host_short "$host")" engine_errors "$host"; done
      step "status-wake-failed-$n" group_status "$dep" "wake-failed-$n"
      step "ledger-wake-failed-$n" group_ledger "$dep" "wake-failed-$n"
      break
    }
    step "keep-woken-$n" keep_group "$dep" "woken-$n"
    step "same-woken-$n" same_identities "$EVID/members-$dep-ready.json" "$EVID/members-$dep-woken-$n.json" || rc=1
    step "mem-woken-$n" mem_both "woken-$n"
    step "cycle-$n" cycle_check "$dep" "$n" || rc=1
    step "canary-$n" canary_check "$dep" "woken-$n" || rc=1
    step "evidence-woken-$n" evidence "$dep" "woken-$n"
    step "collective-$n" collective_check "$dep" "$n" "$base_p" "$base_r" || rc=1
  done
  step infer-after infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  for host in "${GROUP_HOSTS[@]}"; do
    step "control-log-$(host_short "$host")" engine_log_lines "$host" 'POST /(sleep|wake_up|collective_rpc|reset_prefix_cache|release_memory_occupation|resume_memory_occupation)|release_memory|resume_memory|reload_weights|is_sleeping'
  done

  group_stop_settle "$dep" stopped || rc=1
  step evidence evidence "$dep" stopped
  step journal journal_hosts "$dep" stopped
  sleep 3
  step cleanup group_cleanup_check "$dep" stopped ready || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
