# shellcheck shell=bash
# LC6 (ADR 0029, design note "Testing" and live check 7) on a GB10 lab host:
# per-request figures (capyctl.metrics) and load reports during traffic:
#   run_row.sh LC6 --no-e0 -- a
#
# Expected:
#   a  a plain answer carries `timings` and `capyctl.metrics` (prefill_ms,
#      decode_tokens_per_second, cached_tokens, ttft_ms); a stream carries the
#      `x-capyctl-metrics` comment.
#   b  during 3 concurrent streams: llama-server /metrics requests_processing 3,
#      /slots three slots processing, and the management load read shows
#      running 3 with a fresh sample and kv_usage_ppm.
#   c  after the streams, requests_processing 0; delete --stop, nothing left.
#
# Not run here: `--spec-type draft-mtp` (the design note's second half of LC6 and
# live check 8). It needs a GGUF with MTP heads, and Qwen3-4B has none; no such
# model is staged on the lab hosts. Run it as a separate row once one is chosen.
# shellcheck source=scripts/live/matrix/llamacpp_rows.sh
. "$MATRIX_DIR/llamacpp_rows.sh"

row_main() {
  local host rc=0 dep=lc6-qwen3-4b
  host=$(resolve_host "${1:-a}") || return 1
  lc_begin "$host" || return 1
  step start lc_start "$host" || rc=1
  step doc lc_doc "$dep" local || rc=1
  step deploy timed ready lc_deploy "$host" "$dep" || rc=1
  step chat lc_probe "$host" chat --route "$dep" --expect 42 || rc=1
  step chat-stream lc_probe "$host" chat --route "$dep" --stream --expect 42 || rc=1
  step streams lc_probe "$host" streams --route "$dep" --n 3 --max-tokens 400 --read-after 3 || rc=1
  sleep 2
  step engine-after lc_probe "$host" engine || rc=1
  step load-after lc_probe "$host" load
  step delete lc_delete "$host" "$dep" || rc=1
  sleep 5
  step no-engine lc_no_engine "$host" || rc=1
  lc_end "$host" || rc=1
  return "$rc"
}
