# shellcheck shell=bash
# M63 (T15; G13): both replicas of `qwen3-4b` stopped (deployed, not activated);
# 8 simultaneous requests: exactly one activation of one replica, all answered.
#   run_row.sh M63 -- va-4
. "$MATRIX_DIR/rows/M54.sh"
ready_count() { status_dep "$1" | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
print(json.dumps([(x.get("index", x.get("instance")), x.get("host"), x.get("observed_state")) for x in d.get("instances",[])]))'; }
row_main() {
  local fix=${1:-va-4} dep rc=0
  dep=$fix-r63; REP_ROUTE=qwen3-4b
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant variant "$fix" r63 --route "$REP_ROUTE" \
    --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  FIXTURE_VARIANT=r63 step deploy deploy "$fix" || return 1
  step instances-before ready_count "$dep"
  sel_mark
  step burst load --route "$REP_ROUTE" --nonstream 8 --concurrency 8 --max-tokens 32 --timeout 900 || rc=1
  step instances-after ready_count "$dep"
  step selections sel_count m63
  step ops python3 "$MATRIX_DIR/ledger.py" evidence --db "$SERVER_DB" --deployment "$dep"
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
