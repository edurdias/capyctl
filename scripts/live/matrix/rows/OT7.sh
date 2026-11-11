# shellcheck shell=bash
# OT7 (ADR 0037 §8): the collector goes away while requests run. A standalone
# role exporting to the host's collector with the timing header on, one Qwen3-4B
# deployment (vLLM by default):
#   run_row.sh OT7 --no-e0 -- a [vllm|sglang|llamacpp] [seconds down, default 180]
#
# Phases, each a load of streamed requests with a sampled caller context at
# concurrency 4 (load-ot7-*.summary.json):
#   up       60 requests with the collector running
#   down     requests for the given seconds; the collector is stopped 20 s in
#            and stays down; the role log is followed meanwhile for its tracing
#            notice (`tracing: N spans dropped and M export failures ...`),
#            each line stamped with its time (bg-notices.out)
#   resumed  a new collector run (collector-ot7-resumed.jsonl), 60 requests
#
# Expected:
#   a  every request of every phase answered 200 (ok == n).
#   b  cost-ot7.json: the router phases of `down` within OT8's noise of `up`.
#   c  the notice appears at most once a minute (min_gap_s >= 60 when more than
#      one); status after `down` shows spans dropped and the last export error.
#   d  after the restart, collector-ot7-resumed.jsonl holds the `resumed`
#      requests' spans (spans-ot7-resumed.json).
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

row_main() {
  local host engine=${2:-vllm} down=${3:-180} rc=0 d=ot7-dep hev
  host=$(resolve_host "${1:-a}") || return 1
  case $engine in vllm|sglang|llamacpp) ;; *) echo "OT7: engine is vllm, sglang or llamacpp, not $engine" >&2; return 1 ;; esac
  local lg=(--route "$d" --traceparent sampled --concurrency 4 --max-tokens 64)
  ot_begin "$host" "$engine" || return 1
  step collector-start otelcol_start "$host" ot7 || rc=1
  step start ot_start "$host" -- "${OT_SET_ON[@]}" "${OT_SET_TIMING[@]}" || rc=1
  ot_engines "$host" "$engine" || rc=1
  step doc ot_doc "$d" "$engine" || rc=1
  step deploy ot_deploy "$host" "$d" || rc=1
  step warmup ot_load "$host" ot7-warmup "${lg[@]}" --n 8 || rc=1
  step load-up ot_load "$host" ot7-up "${lg[@]}" --n 60 || rc=1
  step status-up ot_status "$host" "$d" || rc=1
  step notices-follow ot_bg "$host" notices \
    "python3 $OT_PROBE stamp --file $OT_ROLE_LOG --match 'spans dropped and' --seconds $((down + 90)) --out $OT_HEV/notices.jsonl" || rc=1
  step load-down-start ot_bg "$host" load-down \
    "python3 $OT_PROBE load --dir $OT_STATE --label ot7-down --out $OT_HEV/load-ot7-down.jsonl --summary $OT_HEV/load-ot7-down.summary.json ${lg[*]} --duration $down" || rc=1
  dry || sleep 20
  step collector-stop otelcol_stop "$host" ot7 || rc=1
  step load-down-wait ot_bg_wait "$host" load-down $((down + 600)) || rc=1
  step status-down ot_status "$host" "$d" || rc=1
  step notices-wait ot_bg_wait "$host" notices $((down + 200)) || rc=1
  step collector-restart otelcol_start "$host" ot7-resumed || rc=1
  step load-resumed ot_load "$host" ot7-resumed "${lg[@]}" --n 60 || rc=1
  step export-wait ot_export_wait
  step status-resumed ot_status "$host" "$d" || rc=1
  ot_end "$host" || rc=1
  hev=$EVID/host-$(host_short "$host")
  step cost x python3 "$MATRIX_DIR/otel_spans.py" cost --arm "up=$hev/load-ot7-up.summary.json" \
    --arm "down=$hev/load-ot7-down.summary.json" --arm "resumed=$hev/load-ot7-resumed.summary.json" --out "$EVID/cost-ot7.json" || rc=1
  step spans-up ot_summary ot7 --requests "$hev/load-ot7-up.jsonl" --label-prefix ot7-up || rc=1
  step spans-resumed ot_summary ot7-resumed --requests "$hev/load-ot7-resumed.jsonl" --label-prefix ot7-resumed || rc=1
  return "$rc"
}
