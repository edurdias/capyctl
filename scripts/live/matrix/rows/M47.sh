# shellcheck shell=bash
# M47 (T30; U5-G1): stop s92-14 during Initialize: cleanup verified, release
# only after absence proof.
#   run_row.sh M47 -- s92-14
row_main() {
  local fix=${1:-s92-14} host rc=0
  host=$(fixture_host "$fix")
  step before host_idle "$host" || return 1
  step deploy deploy "$fix" --activate || return 1
  sleep "${M47_DELAY_S:-25}"
  step state-mid status_dep "$fix"
  step owned-mid keep_owned "$fix" mid
  echo "stop_request $(now_ms)" >>"$EVID/marks.txt"
  step stop timed stop stop_dep "$fix" || rc=1
  step stopped wait_state "$fix" stopped 900 || rc=1
  echo "stopped_seen $(now_ms)" >>"$EVID/marks.txt"
  sleep 3
  step cleanup cleanup_check "$fix" "$host" "$EVID/owned-$fix-mid.json" "$EVID/accounting-$fix-mid.json" || rc=1
  step evidence evidence "$fix" stopped
  step delete delete_dep "$fix" || rc=1
  return "$rc"
}
