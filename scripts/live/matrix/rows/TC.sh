# shellcheck shell=bash
# TC (T19; SPEC §10 "preserve ... tool calls", 2026-09-24): a routed tool-call
# request is forwarded and answered with tool_calls when the engine is launched
# with its tool parser through `accept_extra_args`. Named and auto tool choice,
# non-streaming and streaming. Tool calls need the engine's own parser; without
# one vLLM rejects `auto` (relayed `engine_rejected`, rows/REJ.sh) and SGLang
# answers the call as plain text.
#   run_row.sh TC --tag v17-4 -- v17-4 '["--enable-auto-tool-choice","--tool-call-parser","hermes"]'
#   run_row.sh TC --tag s92-4 -- s92-4 '["--tool-call-parser","qwen25"]'
row_main() {
  local fix=$1 extra=$2 host rc=0 dep choice
  host=$(fixture_host "$fix"); dep=$fix-tc
  step before host_idle "$host" || return 1
  step variant variant "$fix" tc --engine-config-json "{\"accept_extra_args\": true, \"extra_args\": $extra}" || return 1
  FIXTURE_VARIANT=tc step deploy deploy "$fix" --activate --wait || { step errors engine_errors "$host"; return 1; }
  step owned keep_owned "$dep" ready
  for choice in named auto; do
    step "$choice" python3 "$MATRIX_DIR/toolcall.py" --route "$dep" --choice "$choice" --out "$EVID/toolcall.jsonl" || rc=1
    step "$choice-stream" python3 "$MATRIX_DIR/toolcall.py" --route "$dep" --choice "$choice" --stream --out "$EVID/toolcall.jsonl" || rc=1
  done
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step acct accounting "$dep"
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
