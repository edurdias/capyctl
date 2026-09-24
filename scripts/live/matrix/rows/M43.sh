# shellcheck shell=bash
# M43 (T38): SIGKILL the server during a stream: the client sees an honest
# failure; after restart nothing is replayed and the same engine serves again.
#   run_row.sh M43 -- v92-4
row_main() {
  local a=${1:-v92-4} ha rc=0 spid
  ha=$(fixture_host "$a")
  step before host_idle "$ha" || return 1
  step deploy deploy "$a" --activate --wait || return 1
  step owned keep_owned "$a" ready
  python3 "$MATRIX_DIR/infer.py" --route "$a" --prompt "Count from 1 to 400 separated by commas." --stream --max-tokens 1500 \
    --out "$EVID/stream.jsonl" >"$EVID/stream.out" 2>&1 &
  spid=$!
  sleep 4
  echo "server_kill $(now_ms)" >>"$EVID/marks.txt"
  step server-kill fault server KILL || return 1
  wait "$spid"
  echo "client_done $(now_ms)" >>"$EVID/marks.txt"
  step stream python3 -c 'import json,sys
r=[json.loads(l) for l in open(sys.argv[1])][-1]
print(json.dumps({k:r.get(k) for k in ("status","verdict","sse_well_formed","finish_reason","elapsed_s","transport_error","error","sse_events")}))' "$EVID/stream.jsonl"
  step alive alive_check "$ha" "$EVID/owned-$a-ready.json" || rc=1
  step server-up "$MATRIX_DIR/roles.sh" server-up || return 1
  step online "$MATRIX_DIR/roles.sh" wait-online 120 || rc=1
  step ready2 wait_state "$a" ready 300 || rc=1
  step owned2 keep_owned "$a" after
  step same same_identities "$EVID/owned-$a-ready.json" "$EVID/owned-$a-after.json" || rc=1
  step leases python3 "$MATRIX_DIR/ledger.py" accounting --db "$SERVER_DB" --deployment "$a"
  step infer infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stop stop_dep "$a" || rc=1
  step stopped wait_state "$a" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$a" "$ha" "$EVID/owned-$a-ready.json" "$EVID/accounting-$a-ready.json" || rc=1
  step delete delete_dep "$a" || rc=1
  return "$rc"
}
