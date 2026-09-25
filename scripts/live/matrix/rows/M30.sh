# shellcheck shell=bash
# M30 (T17): park during a long stream waits for the request lease:
#   run_row.sh M30 --tag sa-14 -- sa-14
# M30_MAX_TOKENS (default 3000): the router's fixed 300 s stream cap is gone
# (streams end only on the request deadline before the first event or on the
# stream idle timeout), so the vLLM rerun streams 3000 tokens.
# Expected: the stream finishes intact (200, well-formed SSE, terminal event);
# the park settles `parked` only after the stream ends (park-completed time >= stream
# end), or fails bounded with the deployment left serving; nothing is replayed; zero
# request leases afterwards. Then a request wakes it, and stop cleans up.

. "$MATRIX_DIR/rows/M28.sh"

LONG_PROMPT="Write a detailed, numbered list of 300 distinct facts about the history of mathematics, two sentences each."

row_main() {
  local fix=${1:?fixture} dep host rc=0 spid
  dep=$fix; host=$(fixture_host "$fix")
  step before host_idle "$host" || return 1
  step deploy deploy "$fix" --activate --wait || { step engine-errors engine_errors "$host"; return 1; }
  step owned-ready keep_owned "$dep" ready
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1

  # Long stream in the background; its own record goes to stream-long.jsonl.
  echo "stream_start $(now_ms)" >>"$EVID/marks.txt"
  if ! dry; then
    python3 "$MATRIX_DIR/infer.py" --route "$dep" --prompt "$LONG_PROMPT" --stream --max-tokens "${M30_MAX_TOKENS:-3000}" \
      --out "$EVID/stream-long.jsonl" >"$EVID/stream-long.out" 2>"$EVID/stream-long.err" &
    spid=$!
  fi
  sleep 4
  echo "park_request $(now_ms)" >>"$EVID/marks.txt"
  step park-during timed park park_dep "$dep" || rc=1
  step parked wait_state "$dep" parked 900 || rc=1
  echo "parked_seen $(now_ms)" >>"$EVID/marks.txt"
  if ! dry; then wait "$spid" || rc=1; fi
  echo "stream_end_joined $(now_ms)" >>"$EVID/marks.txt"
  step stream-result cat "$EVID/stream-long.out"
  step evidence-parked evidence "$dep" parked
  step order python3 - "$EVID/stream-long.jsonl" "$EVID/marks.txt" "$EVID/evidence-$dep-parked.json" <<'PY'
import json, sys
recs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
marks = dict(l.split() for l in open(sys.argv[2]) if l.strip())
r = recs[-1] if recs else {}
parked_seen = int(marks.get("parked_seen", 0)); end = r.get("finished_unix_ms") or 0
print(json.dumps({"stream_end_before_parked_seen": bool(end) and end <= parked_seen, "stream": {k: r.get(k) for k in ("status", "verdict", "sse_well_formed", "finish_reason", "elapsed_s", "finished_unix_ms", "transport_error", "error")}, "marks": marks}))
PY
  step leases python3 "$MATRIX_DIR/ledger.py" accounting --db "$SERVER_DB" --deployment "$dep"
  step wake-infer timed wake infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step woken wait_state "$dep" ready 600 || rc=1
  step owned-woken keep_owned "$dep" woken
  step same-woken same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-woken.json" || rc=1
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
