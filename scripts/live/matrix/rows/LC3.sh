# shellcheck shell=bash
# LC3 (ADR 0029, design note "Testing" and live check 4) on a GB10 lab host: a
# llama.cpp deployment does not park in place; stopping it releases the memory,
# and a start relaunches it:
#   run_row.sh LC3 --no-e0 -- a
#
# Expected:
#   a  Ready; `park deployment` is refused unsupported_capability (restart-only).
#   b  `stop deployment`: llama-server exits on SIGTERM (time recorded) and the
#      memory comes back (MemAvailable within a few hundred MiB of the baseline;
#      the GGUF's page cache may stay, which the samples show apart).
#   c  a request while stopped (recorded, not judged: a stopped deployment is
#      expected to be refused rather than woken; the wake on demand under memory
#      pressure is LC4).
#   d  `start deployment`: a fresh launch (generation 2) reaches Ready, start time
#      recorded, and answers; delete --stop.
# shellcheck source=scripts/live/matrix/llamacpp_rows.sh
. "$MATRIX_DIR/llamacpp_rows.sh"

row_main() {
  local host rc=0 dep=lc3-qwen3-4b
  host=$(resolve_host "${1:-a}") || return 1
  lc_begin "$host" || return 1
  step start lc_start "$host" || rc=1
  step doc lc_doc "$dep" local || rc=1
  step sampler lc_sampler_start "$host" lc3
  sleep 3
  step deploy timed ready-1 lc_deploy "$host" "$dep" || rc=1
  step infer lc_probe "$host" chat --route "$dep" --expect 42 || rc=1
  step mark-ready lc_mark "$host" ready-1
  step park-refused refused_with unsupported_capability lc "$host" park deployment "$dep" --format json || rc=1
  step stop timed stop-1 lc "$host" stop deployment "$dep" --format json || rc=1
  step stopped lc "$host" status deployment "$dep" --format json
  sleep 8
  step mark-stopped lc_mark "$host" stopped
  step no-engine lc_no_engine "$host" || rc=1
  step request-while-stopped lc_probe "$host" chat --route "$dep" --expect 42 --timeout 60
  step restart timed ready-2 lc "$host" start deployment "$dep" --wait --format json || rc=1
  step status-2 lc_status "$host" "$dep"
  step infer-2 lc_probe "$host" chat --route "$dep" --expect 42 || rc=1
  step delete timed stop-2 lc_delete "$host" "$dep" || rc=1
  sleep 8
  step memory lc_sampler_stop "$host" lc3 || rc=1
  step no-engine-2 lc_no_engine "$host" || rc=1
  lc_end "$host" || rc=1
  return "$rc"
}
