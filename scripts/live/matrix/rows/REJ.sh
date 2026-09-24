# shellcheck shell=bash
# REJ (T17, T19; SPEC §10; owner decision 2026-09-24): an engine's
# invalid-request rejection (complete 400/413/422 with a JSON body) is relayed as
# `engine_rejected` with the engine's status and message, and its lease closes.
# More rejections than the deployment's outstanding bound (32) leave the route
# serving and no request lease held.
#   run_row.sh REJ --tag v17-4 -- v17-4
row_main() {
  local fix=${1:-v17-4} host rc=0
  host=$(fixture_host "$fix")
  step before host_idle "$host" || return 1
  step deploy deploy "$fix" --activate --wait || return 1
  step owned keep_owned "$fix" ready
  step reject python3 "$MATRIX_DIR/reject.py" "$fix" 40 || rc=1
  step leases bash -c "python3 '$MATRIX_DIR/ledger.py' accounting --db '$SERVER_DB' --deployment '$fix' | python3 -c 'import json,sys; a=json.load(sys.stdin); print(json.dumps({\"leases\": a[\"request_leases_deployment\"], \"total\": a[\"request_leases_total\"]})); sys.exit(1 if a[\"request_leases_deployment\"] else 0)'" || rc=1
  step infer infer "$fix" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stop stop_dep "$fix" || rc=1
  step stopped wait_state "$fix" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$fix" "$host" "$EVID/owned-$fix-ready.json" "$EVID/accounting-$fix-ready.json" || rc=1
  step delete delete_dep "$fix" || rc=1
  return "$rc"
}
