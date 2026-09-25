# shellcheck shell=bash
# M33 (T16): preinitialize starts, verifies and parks each deployment in turn:
#   run_row.sh M33 [fixtures...]      default: sa-14 va-4 (normal budget)
#
# Expected: each deployment deployed stopped, then `preinitialize deployment`
# reaches `parked` without serving traffic (Ready is verified inside the operation,
# then parked); the first is parked before the second preinitializes; each parked
# instance keeps its processes alive with a parked reservation; a request to each
# wakes it on demand with the same processes; stop both with verified cleanup; delete.

row_main() {
  local rc=0 d host fixtures=("$@")
  [ ${#fixtures[@]} -gt 0 ] || fixtures=(sa-14 va-4)
  host=$(fixture_host "${fixtures[0]}")
  step before host_idle "$host" || return 1
  for d in "${fixtures[@]}"; do
    step "deploy-$d" deploy "$d" || return 1
    step "preinit-$d" timed "preinit-$d" preinit_dep "$d" || rc=1
    step "parked-$d" wait_state "$d" parked 1200 || { rc=1; step "errors-$d" engine_errors "$host"; }
    step "owned-$d-parked" keep_owned "$d" parked
    step "alive-$d" alive_check "$host" "$EVID/owned-$d-parked.json" || rc=1
    step "evidence-$d" evidence "$d" parked
    snap "parked-$d"
  done
  for d in "${fixtures[@]}"; do
    step "wake-$d" timed "wake-$d" infer "$d" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
    step "owned-$d-woken" keep_owned "$d" woken
    step "same-$d" same_identities "$EVID/owned-$d-parked.json" "$EVID/owned-$d-woken.json" || rc=1
  done
  snap woken
  for d in "${fixtures[@]}"; do step "stop-$d" stop_dep "$d" || rc=1; done
  for d in "${fixtures[@]}"; do step "stopped-$d" wait_state "$d" stopped 600 || rc=1; done
  sleep 3
  for d in "${fixtures[@]}"; do
    step "cleanup-$d" cleanup_check "$d" "$host" "$EVID/owned-$d-woken.json" "$EVID/accounting-$d-woken.json" "$d" || rc=1
    step "delete-$d" delete_dep "$d" || rc=1
  done
  return "$rc"
}

preinit_dep() { cli preinitialize deployment "$1" --output json; }
