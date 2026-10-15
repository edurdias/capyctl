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
# SGLang (owner decision 2026-10-15, after the deep wake failed live twice): SGLang
# groups resolve restart_only and refuse deep, so `MN4 -- sglang` runs
# restart_only_row instead:
#   a  the fixture declares no residency: status reads restart_only (deployment
#      and each member); a park stops the group (parked or stopped, no engine
#      process on either host); a routed request brings it back Ready on a new
#      plan generation and is answered; stop with verified cleanup; delete.
#   b  the same fixture declaring `residency: deep` (variant sg-30-deep) is refused
#      at deploy `capability_missing:deep_park` with the restart_only hint, and no
#      deployment is left.
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

# residency_check <deployment> <label>: status-<dep>-<label>.json reads
# restart_only for the deployment and every member.
residency_check() {
  dry && return 0
  python3 - "$EVID/status-$1-$2.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d = d.get("deployment", d)
dep = (d.get("development_controls") or {}).get("residency")
members = [m.get("residency") for i in d.get("instances", []) for m in i.get("members", [])]
ok = dep == "restart_only" and members and all(m == "restart_only" for m in members)
print(json.dumps({"deployment_residency": dep, "member_residency": members, "restart_only": bool(ok)}))
sys.exit(0 if ok else 1)
PY
}

# plan_advanced <deployment> <before-label> <after-label>: a later plan generation.
plan_advanced() {
  dry && return 0
  python3 - "$EVID/group-$1-$2.json" "$EVID/group-$1-$3.json" <<'PY'
import json, sys
a, b = ((json.load(open(p))["plans"] or [{}])[-1].get("generation") for p in sys.argv[1:])
print(json.dumps({"plan_generation": [a, b], "new_plan": a is not None and b is not None and b > a}))
sys.exit(0 if a is not None and b is not None and b > a else 1)
PY
}

restart_only_row() {
  local dep host rc=0
  dep=$(group_fixture_name sglang)
  echo "engine sglang deployment $dep restart_only park, then a deep variant refused" | tee -a "$EVID/timeline.txt"
  step preflight group_preflight sglang || return 1
  for host in "${GROUP_HOSTS[@]}"; do step "idle-$(host_short "$host")" host_idle "$host" || return 1; done
  step fixture group_fixture sglang || return 1
  dry || cp "$RUNSTATE/fixtures/$dep.yaml" "$EVID/fixture-$dep.json"

  step deploy timed deploy deploy "$dep" --activate --wait || {
    step status-failed group_status "$dep" failed
    for host in "${GROUP_HOSTS[@]}"; do step "errors-$(host_short "$host")" engine_errors "$host"; done
    return 1
  }
  step ready keep_group "$dep" ready
  step ready-check group_ready_check "$dep" ready || rc=1
  step residency residency_check "$dep" ready || rc=1
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1

  step park timed park park_dep "$dep" || rc=1
  step parked timed parked wait_states "$dep" "parked stopped" 600 || rc=1
  step keep-parked keep_group "$dep" parked
  for host in "${GROUP_HOSTS[@]}"; do step "no-engine-$(host_short "$host")" host_idle "$host" || rc=1; done
  step mem-parked mem_both parked
  step evidence-parked evidence "$dep" parked

  step wake timed wake infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step woken wait_states "$dep" ready 600 || rc=1
  step keep-woken keep_group "$dep" woken
  step new-plan plan_advanced "$dep" ready woken || rc=1
  step infer-woken infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1

  group_stop_settle "$dep" stopped || rc=1
  step evidence evidence "$dep" stopped
  step journal journal_hosts "$dep" stopped
  sleep 3
  step cleanup group_cleanup_check "$dep" stopped woken || rc=1
  step delete delete_dep "$dep" || rc=1

  step fixture-deep group_fixture sglang --variant deep --document-json '{"residency": "deep"}' || return 1
  step deploy-deep-refused deep_refused "$dep" || rc=1
  return "$rc"
}

# deep_refused <deployment>: deploying <dep>-deep is refused capability_missing:deep_park
# naming restart_only, and leaves no deployment.
deep_refused() {
  local dep=$1 out rc=0
  out=$(FIXTURE_VARIANT=deep refused_with capability_missing:deep_park deploy "$dep" --activate) || rc=1
  printf '%s\n' "$out"
  dry && return 0
  case $out in *restart_only*) echo "hint names restart_only" ;; *) echo "UNEXPECTED: no restart_only hint"; rc=1 ;; esac
  if status_dep "$dep-deep" >/dev/null 2>&1; then echo "UNEXPECTED: $dep-deep exists"; rc=1; else echo "no deployment $dep-deep"; fi
  return "$rc"
}

row_main() {
  local engine=${1:?engine: vllm or sglang} dep host n rc=0 base_p=0 base_r=0
  case $engine in vllm) ;; sglang) restart_only_row; return ;; *) echo "MN4 parks vllm or sglang groups (TensorFold has no deep park)"; return 1 ;; esac
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
