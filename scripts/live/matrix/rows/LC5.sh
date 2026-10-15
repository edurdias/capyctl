# shellcheck shell=bash
# LC5 (ADR 0029, design note "Testing" and live check 6) on a GB10 lab host: a
# streaming request cancelled midway; llama-server's requests_processing returns
# to 0 within the drain bound; then the deployment stops (it does not park):
#   run_row.sh LC5 --no-e0 -- a
#
# Expected:
#   a  a stream cut after 5 chunks (mid-decode): requests_processing 0 within
#      10 s (the laptop: under 0.1 s).
#   b  a stream with a ~6.6k-token prompt cut before its first chunk
#      (mid-prefill): 0 within 10 s (the laptop: 0.5-0.8 s).
#   c  the deployment still answers; stop releases it (park is refused for
#      llama.cpp, LC3); nothing of the engine left.
# shellcheck source=scripts/live/matrix/llamacpp_rows.sh
. "$MATRIX_DIR/llamacpp_rows.sh"

row_main() {
  local host rc=0 dep=lc5-qwen3-4b
  host=$(resolve_host "${1:-a}") || return 1
  lc_begin "$host" || return 1
  step start lc_start "$host" || rc=1
  step doc lc_doc "$dep" local || rc=1
  step deploy timed ready lc_deploy "$host" "$dep" || rc=1
  step hangup-decode lc_probe "$host" hangup --route "$dep" --after-chunks 5 --max-tokens 2048 \
    --prompt "'Write a long story about a lighthouse.'" || rc=1
  step hangup-prefill lc_probe "$host" hangup --route "$dep" --long-prompt 550 --wait-before-cut 0.3 || rc=1
  step engine-after lc_probe "$host" engine || rc=1
  step load-after lc_probe "$host" load
  step infer lc_probe "$host" chat --route "$dep" --expect 42 || rc=1
  step stop timed stop lc "$host" stop deployment "$dep" --format json || rc=1
  sleep 5
  step no-engine lc_no_engine "$host" || rc=1
  step delete lc_delete "$host" "$dep" || rc=1
  lc_end "$host" || rc=1
  return "$rc"
}
