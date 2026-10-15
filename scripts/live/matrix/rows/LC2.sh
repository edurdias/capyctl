# shellcheck shell=bash
# LC2 (ADR 0029, design note "Testing" and live checks 1, 2, 3, 5) on a GB10 lab
# host: deploy Qwen3-4B Q4_K_M; plain and streaming chat, tool calls and
# reasoning_content through the router; memory against the derived request; the
# page cache beside the device copy under the default mmap load:
#   run_row.sh LC2 --no-e0 -- a
#
# Expected:
#   a  start to Ready recorded (deploy --activate --wait, digest included, and
#      from the engine's own lines); the rendered command (argv) as ADR 0029 §5.
#   b  llama.cpp's `KV buffer size` line equals the derived KV cache
#      (4 x 8192 x 36 x 8 x (128 x 2 + 128 x 2) B = 4608.00 MiB).
#   c  plain chat answers 42 with `timings` and `capyctl.metrics`; streaming deltas
#      within STREAM_DELTA_FIELDS (role, content, reasoning_content, tool_calls),
#      `timings` on the last chunk (and `usage` with include_usage); tool call
#      get_weather plain and streamed.
#   d  memory: peak and steady MemAvailable drop, process RSS (anon and file),
#      nvidia-smi's per-process figure if GB10 reports one, the GGUF bytes in the
#      page cache while serving, against status's derived request. Peak within the
#      request is the pass; the margin is recorded.
#   e  the same deployment with `--load-mode none` (no mmap): the same figures,
#      so the difference shows whether mmap keeps a page-cache copy of the
#      weights beside the device copy (live check 2, GB10 half).
#   f  delete --stop: exit, memory back, no engine left.
# shellcheck source=scripts/live/matrix/llamacpp_rows.sh
. "$MATRIX_DIR/llamacpp_rows.sh"

# lc2_serve <host> <name> <label> [extra_args...]: one deployment measured from
# before its start to after its exit.
lc2_serve() {
  local host=$1 name=$2 label=$3 rc=0
  shift 3
  step "doc-$label" lc_doc "$name" local "$@" || return 1
  step "sampler-$label" lc_sampler_start "$host" "$label"
  sleep 3
  step "deploy-$label" timed "ready-$label" lc_deploy "$host" "$name" || { step "errors-$label" lc_engine_lines "$host"; rc=1; }
  step "status-$label" lc_status "$host" "$name"
  step "engine-lines-$label" lc_engine_lines "$host"
  step "engine-$label" lc_probe "$host" engine || rc=1
  step "mark-ready-$label" lc_mark "$host" "ready-$label"
  step "chat-$label" lc_probe "$host" chat --route "$name" --expect 42 || rc=1
  step "chat-stream-$label" lc_probe "$host" chat --route "$name" --stream --expect 42 || rc=1
  step "chat-stream-usage-$label" lc_probe "$host" chat --route "$name" --stream --usage --expect 42 || rc=1
  step "tools-$label" lc_probe "$host" chat --route "$name" --tools || rc=1
  step "tools-stream-$label" lc_probe "$host" chat --route "$name" --tools --stream || rc=1
  # Steady state: the samples of the last 20 s before the stop.
  sleep 20
  step "mark-steady-$label" lc_mark "$host" "steady-$label"
  step "delete-$label" timed "stop-$label" lc_delete "$host" "$name" || rc=1
  sleep 8
  step "memory-$label" lc_sampler_stop "$host" "$label" || rc=1
  step "no-engine-$label" lc_no_engine "$host" || rc=1
  return "$rc"
}

row_main() {
  local host rc=0
  host=$(resolve_host "${1:-a}") || return 1
  lc_begin "$host" || return 1
  step start lc_start "$host" || rc=1
  step engine-list lc "$host" engine list --format json || rc=1
  lc2_serve "$host" lc2-qwen3-4b mmap || rc=1
  lc2_serve "$host" lc2-qwen3-4b-nommap nommap --load-mode none || rc=1
  lc_end "$host" || rc=1
  return "$rc"
}
