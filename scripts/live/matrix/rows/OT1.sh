# shellcheck shell=bash
# OT1 (ADR 0037 §1, live qualification): tracing off by default. A standalone
# role with no tracing setting, on a host whose collector runs (so any export
# would arrive), serves each engine; requests with and without a valid
# traceparent are answered alike:
#   run_row.sh OT1 --no-e0 -- a
#
# Expected:
#   a  status shows `tracing: off` for the role (no endpoint).
#   b  on vLLM 0.32.0, SGLang 0.5.22 and llama.cpp v0.7.0 (Qwen3-4B) and a
#      Clef-Flash decision on SGLang: a request without trace headers and one
#      with a valid sampled traceparent and a tracestate, collected and
#      streamed, are answered 200 with the same shape (requests.jsonl).
#   c  `ss` on the collector's ports (4317, OTELCOL_PORT) shows no connection
#      from any CapyCTL or engine process, while each engine serves and after.
#   d  each engine process's command line holds no tracing option and its
#      environment, like the role's, no OTEL_*, SGLANG_TRACE_*, SGLANG_OTLP_*,
#      TRACEPARENT or TRACESTATE name (procs-*.json).
#   e  the collector's file is empty or absent (spans-ot1.json: 0 spans).
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

# ot1_serve <host> <name> <kind>: one deployment, its requests and the checks
# while it serves, then deleted with --stop.
ot1_serve() {
  local host=$1 name=$2 kind=$3 rc=0 api=chat expect=(--expect 42)
  [ "$kind" = decisions ] && { api=decisions; expect=(); }
  step "doc-$name" ot_doc "$name" "$kind" || return 1
  step "deploy-$name" timed "ready-$name" ot_deploy "$host" "$name" || rc=1
  step "status-$name" ot_status "$host" "$name" || rc=1
  step "plain-$name" ot_req "$host" --route "$name" --api "$api" --label "ot1-$name-plain" "${expect[@]}" || rc=1
  step "traced-$name" ot_req "$host" --route "$name" --api "$api" --label "ot1-$name-traced" \
    --traceparent sampled --tracestate capyctl=ot1 "${expect[@]}" || rc=1
  if [ "$kind" != decisions ]; then
    step "plain-stream-$name" ot_req "$host" --route "$name" --stream --label "ot1-$name-plain-stream" "${expect[@]}" || rc=1
    step "traced-stream-$name" ot_req "$host" --route "$name" --stream --label "ot1-$name-traced-stream" \
      --traceparent sampled --tracestate capyctl=ot1 "${expect[@]}" || rc=1
  fi
  step "procs-$name" ot_procs "$host" "$name" || rc=1
  step "conns-$name" ot_conns "$host" "$name" || rc=1
  step "delete-$name" ot_delete "$host" "$name" || rc=1
  return "$rc"
}

row_main() {
  local host rc=0
  host=$(resolve_host "${1:-a}") || return 1
  ot_begin "$host" vllm sglang llamacpp decisions || return 1
  step collector-start otelcol_start "$host" ot1 || rc=1
  step start ot_start "$host" || rc=1
  ot_engines "$host" vllm sglang llamacpp || rc=1
  step procs-role ot_procs "$host" role || rc=1
  ot1_serve "$host" ot1-v vllm || rc=1
  ot1_serve "$host" ot1-s sglang || rc=1
  ot1_serve "$host" ot1-l llamacpp || rc=1
  ot1_serve "$host" ot1-d decisions || rc=1
  step export-wait ot_export_wait
  step conns-end ot_conns "$host" end || rc=1
  ot_end "$host" || rc=1
  step spans ot_summary ot1 || rc=1
  return "$rc"
}
