# shellcheck shell=bash
# M66 (T17; G10): `delete deployment --stop` during a long stream: the stream
# completes or fails within the drain bound; the deployment is removed only
# after cleanup evidence. The prompt's answer outlasts the 30 s drain bound
# (override with M66_PROMPT). After the delete the deployment has no status, so
# cleanup is judged by the recorded identities being gone and the ledger
# snapshot holding nothing for the deleted id (its tombstone excepted).
#   run_row.sh M66 --tag sb-4 -- sb-4
row_main() {
  local fix=${1:-sb-4} host rc=0 spid depid
  host=$(fixture_host "$fix")
  step before host_idle "$host" || return 1
  step deploy deploy "$fix" --activate --wait || return 1
  step owned keep_owned "$fix" ready
  depid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["deployment_id"])' "$EVID/accounting-$fix-ready.json")
  echo "stream_start $(now_ms)" >>"$EVID/marks.txt"
  python3 "$MATRIX_DIR/infer.py" --route "$fix" --prompt "${M66_PROMPT:-Count from 1 to 1500 in English words, one number per line, with no other text.}" \
    --stream --max-tokens 3000 --out "$EVID/stream-long.jsonl" >"$EVID/stream-long.out" 2>"$EVID/stream-long.err" &
  spid=$!
  sleep 6
  echo "delete_request $(now_ms)" >>"$EVID/marks.txt"
  step delete-stop timed delete-stop cli delete deployment "$fix" --stop --format json || rc=1
  echo "delete_returned $(now_ms)" >>"$EVID/marks.txt"
  wait "$spid"
  echo "stream_joined $(now_ms)" >>"$EVID/marks.txt"
  step stream python3 -c 'import json,sys
for l in open(sys.argv[1]):
    d=json.loads(l); print(json.dumps({k:d.get(k) for k in ("status","finish_reason","sse_done","sse_well_formed","sse_events","sse_malformed","elapsed_s","started_unix_ms","finished_unix_ms","verdict")}))' "$EVID/stream-long.jsonl"
  step gone refused status_dep "$fix" || rc=1
  sleep 3
  step identities alive_check "$host" "$EVID/owned-$fix-ready.json"
  step residue residue_check "$depid" || rc=1
  step host host_idle "$host" || rc=1
  step timing python3 -c 'import json,sys
m=dict(l.split() for l in open(sys.argv[1]))
d=[json.loads(l) for l in open(sys.argv[2])][0]
req=int(m["delete_request"]); end=d["finished_unix_ms"]; ret=int(m["delete_returned"])
print(json.dumps({"stream_end_after_delete_ms": end-req, "delete_return_after_request_ms": ret-req, "finish_reason": d.get("finish_reason"), "sse_done": d.get("sse_done")}))' "$EVID/marks.txt" "$EVID/stream-long.jsonl"
  return "$rc"
}
