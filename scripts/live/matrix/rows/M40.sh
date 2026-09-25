# shellcheck shell=bash
# M40 (T33; G06): kill (SIGKILL) and restart a host agent with a Ready engine:
#   run_row.sh M40 --tag sa-4 -- sa-4
#
# Expected: while the agent is down the route answers a retryable refusal quickly
# (503), never a hang or a 500 with Ready; after `host-up` the journal reconciles,
# no duplicate launch (identical owned identities), Ready only after a fresh probe,
# the route answers correctly; stop with verified cleanup; delete.

. "$MATRIX_DIR/rows/M36.sh"

row_main() {
  local fix=${1:-sa-4} dep host rc=0
  dep=$fix; host=$(fixture_host "$fix")
  step before host_idle "$host" || return 1
  step deploy deploy "$fix" --activate --wait || return 1
  step owned-ready keep_owned "$dep" ready
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  echo "agent_kill $(now_ms)" >>"$EVID/marks.txt"
  step agent-kill fault agent "$host" KILL || return 1
  step during-down infer "$dep" "What is 17+25? Answer with only the number." --max-tokens 16 --timeout 60
  step poll-down dispatch_poll "$dep" 8
  step alive-while-down alive_check "$host" "$EVID/owned-$dep-ready.json" || rc=1
  echo "agent_up $(now_ms)" >>"$EVID/marks.txt"
  step agent-up "$MATRIX_DIR/roles.sh" host-up "$host" || return 1
  step poll-up dispatch_poll "$dep" 20
  step online "$MATRIX_DIR/roles.sh" wait-online 120 || rc=1
  step ready-again wait_state "$dep" ready 300 || rc=1
  step owned-after keep_owned "$dep" after
  step same same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-after.json" || rc=1
  step infer-after infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step evidence evidence "$dep" after
  step host-log rsh "$host" "grep -aE 'adopt|reconcil|probe|re-?attach|journal' $RRD/host.log | grep -viE 'key|secret|bearer|token' | tail -n 30"
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
