# shellcheck shell=bash
# M42 (T33, T38; D8, G07): engines Ready on both hosts; SIGKILL the server and
# restart it:
#   run_row.sh M42 -- va-4 sb-4
#
# Expected: with the server down nothing touches the engines (identities alive);
# after restart every retained launch is re-attached by Inspect plus a fresh probe
# (same identities, no relaunch), no false Ready before it, both routes answer;
# then stop both with verified cleanup; delete.

. "$MATRIX_DIR/rows/M36.sh"

row_main() {
  local a=${1:-va-4} b=${2:-sb-4} ha hb rc=0
  ha=$(fixture_host "$a"); hb=$(fixture_host "$b")
  step before-a host_idle "$ha" || return 1
  step before-b host_idle "$hb" || return 1
  step deploy-a deploy "$a" --activate || return 1
  step deploy-b deploy "$b" --activate || return 1
  step ready-a wait_state "$a" ready 900 || return 1
  step ready-b wait_state "$b" ready 900 || return 1
  step owned-a keep_owned "$a" ready
  step owned-b keep_owned "$b" ready
  step infer-a infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step infer-b infer "$b" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  echo "server_kill $(now_ms)" >>"$EVID/marks.txt"
  step server-kill fault server KILL || return 1
  sleep 5
  step alive-a alive_check "$ha" "$EVID/owned-$a-ready.json" || rc=1
  step alive-b alive_check "$hb" "$EVID/owned-$b-ready.json" || rc=1
  echo "server_up $(now_ms)" >>"$EVID/marks.txt"
  step server-up "$MATRIX_DIR/roles.sh" server-up || return 1
  step poll-a dispatch_poll "$a" 15
  step online "$MATRIX_DIR/roles.sh" wait-online 120 || rc=1
  step ready-a2 wait_state "$a" ready 300 || rc=1
  step ready-b2 wait_state "$b" ready 300 || rc=1
  echo "reattached $(now_ms)" >>"$EVID/marks.txt"
  step owned-a2 keep_owned "$a" after
  step owned-b2 keep_owned "$b" after
  step same-a same_identities "$EVID/owned-$a-ready.json" "$EVID/owned-$a-after.json" || rc=1
  step same-b same_identities "$EVID/owned-$b-ready.json" "$EVID/owned-$b-after.json" || rc=1
  step infer-a2 infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step infer-b2 infer "$b" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step server-log bash -c "grep -aE 'adopt|re-?attach|reconcil|probe' '$LRD/server.log' | grep -viE 'key|secret|bearer|token' | tail -n 30"
  for d in "$a" "$b"; do step "stop-$d" stop_dep "$d" || rc=1; done
  for d in "$a" "$b"; do step "stopped-$d" wait_state "$d" stopped 600 || rc=1; done
  sleep 3
  step cleanup-a cleanup_check "$a" "$ha" "$EVID/owned-$a-ready.json" "$EVID/accounting-$a-ready.json" a || rc=1
  step cleanup-b cleanup_check "$b" "$hb" "$EVID/owned-$b-ready.json" "$EVID/accounting-$b-ready.json" b || rc=1
  step delete-a delete_dep "$a" || rc=1
  step delete-b delete_dep "$b" || rc=1
  return "$rc"
}
