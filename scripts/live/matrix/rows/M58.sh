# shellcheck shell=bash
# M58 + M60 standalone (2026-09-24 smoke): freeze the host-b host agent
# under a load that outlasts the freeze, on a two-instance spread deployment.
# q4 answers are short, so M54's 128-request burst ended before the SIGSTOP;
# here 2400 requests at 16 concurrent keep traffic flowing through the 20 s
# freeze. Selections are counted per window (before, frozen, after); the
# instances keep their processes (no replay, no restart). See rows/M54.sh for
# the M58/M60 expectations.
#   run_row.sh M58 -- va-4
# shellcheck source=scripts/live/matrix/rows/M54.sh
. "$MATRIX_DIR/rows/M54.sh"
row_main() {
  local fix=${1:-va-4} dep rc=0 l0 l1 l2 l3
  dep=$fix-m58
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant variant "$fix" m58 --route "$REP_ROUTE" \
    --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  FIXTURE_VARIANT=m58 step deploy deploy "$fix" --activate || return 1
  step ready both_ready "$dep" 1200 || rc=1
  step owned keep_owned "$dep" ready
  sel_mark; l0=$SERVER_LOG_LINE
  host_poll_start m58
  bg_load m58 --nonstream 2400 --concurrency 16 --max-tokens 64
  sleep 6
  l1=$(wc -l <"$LRD/server.log")
  echo "agent_stop $(now_ms) server.log $l1" >>"$EVID/marks.txt"
  step m58-sigstop fault agent "$HOST_B" STOP || rc=1
  sleep 20
  step m58-accounting keep_owned "$dep" agent-stopped
  l2=$(wc -l <"$LRD/server.log")
  echo "agent_cont $(now_ms) server.log $l2" >>"$EVID/marks.txt"
  step m60-sigcont fault agent "$HOST_B" CONT || rc=1
  wait "$BG_LOAD" || rc=1
  sleep 5
  host_poll_stop
  l3=$(wc -l <"$LRD/server.log")
  step load-summary cat "$EVID/load-m58.summary.json"
  SERVER_LOG_LINE=$l0 step sel-before sel_count m58-before --to-line "$l1"
  SERVER_LOG_LINE=$l1 step sel-frozen sel_count m58-frozen --to-line "$l2"
  SERVER_LOG_LINE=$l2 step sel-after sel_count m58-after --to-line "$l3"
  step owned-after keep_owned "$dep" rejoined
  step same same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-rejoined.json" || rc=1
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 900 || rc=1
  sleep 3
  step cleanup-ids cleanup_check_partial "$dep" "$HOST_A" "$EVID/owned-$dep-ready.json" || rc=1
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
