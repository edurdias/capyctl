# shellcheck shell=bash
# M46 (T15, T27): tight host; operator activates s17-30 and s17-14 at once:
# exactly one arms; the other is denied or queued; no double charge.
#   run_row.sh M46 -- s17-30 s17-14    (host-b on the tight policy)
row_main() {
  local a=${1:-s17-30} b=${2:-s17-14} host rc=0
  host=$(fixture_host "$a")
  step before host_idle "$host" || return 1
  step deploy-a deploy "$a" || return 1
  step deploy-b deploy "$b" || return 1
  cli start deployment "$a" --output json >"$EVID/start-a.out" 2>"$EVID/start-a.err" &
  cli start deployment "$b" --output json >"$EVID/start-b.out" 2>"$EVID/start-b.err" &
  wait
  step start-a cat "$EVID/start-a.out" "$EVID/start-a.err"
  step start-b cat "$EVID/start-b.out" "$EVID/start-b.err"
  step ledger-armed python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"
  sleep 20
  step ledger-20s python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"
  step status-a status_dep "$a"
  step status-b status_dep "$b"
  for d in "$a" "$b"; do step "stop-$d" stop_dep "$d"; done
  for d in "$a" "$b"; do step "stopped-$d" wait_state "$d" stopped 900 || rc=1; done
  sleep 3
  step clean host_idle "$host" || rc=1
  for d in "$a" "$b"; do step "delete-$d" delete_dep "$d" || rc=1; done
  return "$rc"
}
