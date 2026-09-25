# shellcheck shell=bash
# M57 and M59 without the rest of tier 5 (rows/M54.sh): the replica deployment on both hosts, the
# long-work skew and the short requests that should lean away from it.
#   run_row.sh M57 --tag rep -- va-14
# shellcheck source=scripts/live/matrix/rows/M54.sh
. "$MATRIX_DIR/rows/M54.sh"

row_main() {
  local fix=${1:-va-14} dep rc=0
  dep=$fix-rep
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant variant "$fix" rep --route "$REP_ROUTE" \
    --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  FIXTURE_VARIANT=rep step deploy deploy "$fix" --activate || return 1
  step ready both_ready "$dep" 1200 || return 1
  step owned keep_owned "$dep" ready
  m57_block "$dep" || rc=1
  [ "${M57_WITH_M59:-1}" = 1 ] && { m59_block "$dep" || rc=1; }
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 900 || rc=1
  sleep 3
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
