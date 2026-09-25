# shellcheck shell=bash
# M69 (T33; G06, G15): SIGTERM the host-a host agent under load; restart it:
# gates close, in-flight drains, exit 0, engines retained; Ready again only
# after a fresh probe; the same engine processes serve.
#   run_row.sh M69 -- va-4
. "$MATRIX_DIR/rows/M36.sh"
row_main() {
  local a=${1:-va-4} ha rc=0 lpid
  ha=$(fixture_host "$a")
  step before host_idle "$ha" || return 1
  step deploy deploy "$a" --activate --wait || return 1
  step owned keep_owned "$a" ready
  python3 "$MATRIX_DIR/loadgen.py" --route "$a" --out "$EVID/load-bg.jsonl" --summary "$EVID/load-bg.summary.json" \
    --stream 16 --nonstream 400 --concurrency 16 --max-tokens 512 >"$EVID/load-bg.out" 2>&1 &
  lpid=$!
  sleep 8
  echo "agent_term $(now_ms)" >>"$EVID/marks.txt"
  step agent-term "$MATRIX_DIR/roles.sh" host-down "$ha" TERM || rc=1
  echo "agent_exited $(now_ms)" >>"$EVID/marks.txt"
  step agent-exit rsh "$ha" "tail -n 3 $RRD/host.log | grep -viE 'key|secret|bearer'"
  step alive alive_check "$ha" "$EVID/owned-$a-ready.json" || rc=1
  step status-down status_dep "$a"
  step agent-up "$MATRIX_DIR/roles.sh" host-up "$ha" || return 1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  wait "$lpid"
  step load-summary cat "$EVID/load-bg.summary.json"
  step ready2 wait_state "$a" ready 300 || rc=1
  step owned2 keep_owned "$a" after
  step same same_identities "$EVID/owned-$a-ready.json" "$EVID/owned-$a-after.json" || rc=1
  step infer infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stop stop_dep "$a" || rc=1
  step stopped wait_state "$a" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$a" "$ha" "$EVID/owned-$a-ready.json" "$EVID/accounting-$a-ready.json" || rc=1
  step delete delete_dep "$a" || rc=1
  return "$rc"
}
