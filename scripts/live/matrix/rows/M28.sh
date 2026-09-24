# shellcheck shell=bash
# M28 (T16, T20, T22): deep park then wake on request, per fixture:
#   run_row.sh M28 --tag s92-14 -- s92-14      (SGLang: memory-saver release, observed unmapped)
#   run_row.sh M29 --tag v92-14 -- v92-14      (vLLM: rows/M29.sh reuses this with sleep level 2)
#
# Setup: server and the fixture's host online (normal budget); no other deployment
# active on that host. Only after M08 passed live (owner decision P4).
#
# Expected:
#   ready   deploy --activate --wait; correct routed answer; I1.
#   park    `park deployment` settles `parked`; the same owned processes (pid, start
#           ticks, boot id) stay alive; the reservation drops to the parked phase;
#           MemAvailable returns at least half of the Ready drop; the park step
#           records its release evidence (vLLM sleep level 2; SGLang every kv_cache
#           and weights segment observed unmapped).
#   wake    a routed request to the parked route wakes it on demand (no operator
#           start): correct answer, same processes, Ready again, reservation back to
#           the ready phase, restore evidence (vLLM wake, reload_weights, KV wake,
#           prefix reset, fresh probe), I1 matches the golden from Ready.
#   stop    verified cleanup; the deployment is deleted.

PARK_MIN_RELEASE=${PARK_MIN_RELEASE:-0.5}

state_of() { # state_of <deployment>: observed_state now
  status_dep "$1" 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d); print(d.get("observed_state",""))'
}

park_wake_row() {
  local fix=$1 dep host engine rc=0
  dep=$fix
  host=$(fixture_host "$fix")
  engine=$(fixture_engine "$fix")
  echo "fixture $fix deployment $dep host $host engine $engine" | tee -a "$EVID/timeline.txt"
  step before host_idle "$host" || return 1
  step mem-before host_mem "$host" before

  step deploy deploy "$fix" --activate --wait || { step engine-errors engine_errors "$host"; return 1; }
  step owned-ready keep_owned "$dep" ready
  step mem-ready host_mem "$host" ready
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step i1-ready i1 "$dep" "$fix" "$dep" || rc=1
  snap ready

  step park timed park park_dep "$dep" || rc=1
  step parked wait_state "$dep" parked 600 || { rc=1; step park-errors engine_errors "$host"; }
  sleep 3
  step owned-parked keep_owned "$dep" parked
  step alive-parked alive_check "$host" "$EVID/owned-$dep-ready.json" || rc=1
  step same-parked same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-parked.json" || rc=1
  step mem-parked host_mem "$host" parked
  step released mem_release before ready parked "$PARK_MIN_RELEASE" || rc=1
  step evidence-parked evidence "$dep" parked
  step gpu-parked rsh "$host" "nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader"
  snap parked
  step state-parked-still state_of "$dep"

  # Wake on request: no operator start.
  step wake-infer timed wake infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step woken wait_state "$dep" ready 600 || rc=1
  step owned-woken keep_owned "$dep" woken
  step same-woken same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-woken.json" || rc=1
  step mem-woken host_mem "$host" woken
  step i1-woken i1 "$dep" "$fix" "$dep" || rc=1
  step evidence-woken evidence "$dep" woken
  step engine-control-log engine_log_lines "$host" 'POST /(sleep|wake_up|collective_rpc|reset_prefix_cache|release_memory_occupation|resume_memory_occupation)|release_memory|resume_memory|reload_weights|is_sleeping'
  snap woken

  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() { park_wake_row "${1:?fixture, e.g. s92-14}"; }
