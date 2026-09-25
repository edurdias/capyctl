# shellcheck shell=bash
# M32 (T23, T26): `max_parked` is enforced by stopping the least recently parked
# instance on that host, never ready work. Tight budget on host-a
# (`max_parked: 1`):
#   roles.sh host-doc host-a tight; roles.sh host-down host-a; roles.sh host-up host-a
#   run_row.sh M32
#
# Expected: A (va-4) Ready then parked; B (sa-4) Ready then parked, which exceeds
# max_parked 1 on the host, so A (least recently parked) is stopped with verified
# cleanup (identities gone, charge released) while B stays parked with its processes
# alive; a request to B wakes it on demand; a request to A starts it cold (a new
# generation), both then serving; stop both with verified cleanup; delete both.

row_main() {
  local host=$HOST_A a=va-4 b=sa-4 rc=0
  [ "${POLICY_a:-}" = tight ] || dry || { echo "$HOST_A is not on the tight policy (POLICY_a=${POLICY_a:-unset})"; return 1; }
  step before host_idle "$host" || return 1

  step deploy-a deploy "$a" --activate --wait || return 1
  step owned-a-ready keep_owned "$a" ready
  step infer-a infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step park-a park_dep "$a" || rc=1
  step parked-a wait_state "$a" parked 600 || rc=1
  step owned-a-parked keep_owned "$a" parked
  snap a-parked

  step deploy-b deploy "$b" --activate --wait || rc=1
  step owned-b-ready keep_owned "$b" ready
  step infer-b infer "$b" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step park-b park_dep "$b" || rc=1
  step parked-b wait_state "$b" parked 600 || rc=1
  # max_parked 1: A, the least recently parked, is reclaimed by an ordinary stop.
  step a-reclaimed wait_state "$a" stopped 600 || rc=1
  sleep 3
  step a-cleanup cleanup_check "$a" "$host" "$EVID/owned-$a-ready.json" "$EVID/accounting-$a-ready.json" a || rc=1
  step b-alive alive_check "$host" "$EVID/owned-$b-ready.json" || rc=1
  step owned-b-parked keep_owned "$b" parked
  step evidence-a evidence "$a" reclaimed
  step evidence-b evidence "$b" parked
  snap b-parked

  step wake-b timed wake-b infer "$b" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step owned-b-woken keep_owned "$b" woken
  step same-b same_identities "$EVID/owned-$b-ready.json" "$EVID/owned-$b-woken.json" || rc=1
  step start-a-cold timed cold-a infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step owned-a-gen2 keep_owned "$a" gen2
  snap both

  for d in "$a" "$b"; do step "stop-$d" stop_dep "$d" || rc=1; done
  for d in "$a" "$b"; do step "stopped-$d" wait_state "$d" stopped 600 || rc=1; done
  sleep 3
  step cleanup-a cleanup_check "$a" "$host" "$EVID/owned-$a-gen2.json" "$EVID/accounting-$a-gen2.json" a2 || rc=1
  step cleanup-b cleanup_check "$b" "$host" "$EVID/owned-$b-woken.json" "$EVID/accounting-$b-woken.json" b || rc=1
  step delete-a delete_dep "$a" || rc=1
  step delete-b delete_dep "$b" || rc=1
  return "$rc"
}
