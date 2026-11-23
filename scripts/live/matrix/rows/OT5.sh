# shellcheck shell=bash
# OT5 (ADR 0037 §§6, 7, 10): engine spans. A standalone role exporting to the
# host's collector with `host.observability.tracing.engines: on`; one Qwen3-4B
# deployment at a time on vLLM 0.32.0, SGLang 0.5.22 (one tokenizer worker, its
# default) and llama.cpp v0.7.0:
#   run_row.sh OT5 --no-e0 -- a
#
# Expected:
#   a  status: `engine tracing: on` for the vLLM and SGLang instances,
#      `engine tracing: off (engine_unsupported)` for llama.cpp.
#   b  procs-*.json: vLLM's command line carries `--otlp-traces-endpoint
#      <endpoint>` (never `--collect-detailed-traces`) and its environment
#      OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf, OTEL_SERVICE_NAME=vllm
#      and OTEL_RESOURCE_ATTRIBUTES naming the deployment and instance;
#      SGLang's `--enable-trace --otlp-traces-endpoint <endpoint>
#      --otlp-service-name sglang` and OTEL_EXPORTER_OTLP_TRACES_PROTOCOL,
#      SGLANG_TRACE_LEVEL=1 (no SGLANG_TRACE_ASYNC); llama-server neither.
#   c  spans-ot5.json, per request with a sampled caller traceparent: vLLM's
#      `llm_request` span and SGLang's request span have `capyctl.forward` as
#      parent; on SGLang `gen_ai.request.id` equals `capyctl.request_id`.
#   d  llama.cpp: CapyCTL's spans only, no engine span in its requests' traces
#      (the header it would have received is not observable on the host; its
#      absence is shown by the status reason and by llama-server exporting
#      nothing).
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

ot5_serve() { # ot5_serve <host> <name> <kind>
  local host=$1 d=$2 kind=$3 rc=0
  step "doc-$d" ot_doc "$d" "$kind" || return 1
  step "deploy-$d" ot_deploy "$host" "$d" || rc=1
  step "status-$d" ot_status "$host" "$d" || rc=1
  step "procs-$d" ot_procs "$host" "$d" || rc=1
  step "collected-$d" ot_req "$host" --route "$d" --traceparent sampled --tracestate capyctl=ot5 \
    --label "ot5-$d-collected" --expect 42 || rc=1
  step "stream-$d" ot_req "$host" --route "$d" --stream --traceparent sampled --tracestate capyctl=ot5 \
    --label "ot5-$d-stream" --expect 42 || rc=1
  step "untraced-$d" ot_req "$host" --route "$d" --label "ot5-$d-none" --expect 42 || rc=1
  step "export-wait-$d" ot_export_wait
  step "delete-$d" ot_delete "$host" "$d" || rc=1
  return "$rc"
}

row_main() {
  local host rc=0
  host=$(resolve_host "${1:-a}") || return 1
  ot_begin "$host" vllm sglang llamacpp || return 1
  step collector-start otelcol_start "$host" ot5 || rc=1
  step start ot_start "$host" -- "${OT_SET_ON[@]}" "${OT_SET_ENGINES[@]}" || rc=1
  ot_engines "$host" vllm sglang llamacpp || rc=1
  ot5_serve "$host" ot5-v vllm || rc=1
  ot5_serve "$host" ot5-s sglang || rc=1
  ot5_serve "$host" ot5-l llamacpp || rc=1
  ot_end "$host" || rc=1
  step spans ot_summary ot5 || rc=1
  return "$rc"
}
