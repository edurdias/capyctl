# shellcheck shell=bash
# OT2 (ADR 0037 §§2, 3, 9, 10): CapyCTL's own spans, standalone. A standalone
# role exporting to the host's collector (`server.observability.tracing.endpoint`,
# ratio 1.0 and `caller_context: parent` by default, engine tracing off), with
# the timing header on:
#   run_row.sh OT2 --no-e0 -- a [vllm|sglang]     (chat engine, default vllm)
#
# Expected (spans-ot2.json and its short summary):
#   a  status shows `tracing: on → <endpoint>` with no spans dropped.
#   b  a streamed and a collected chat request with a sampled caller
#      traceparent (and a tracestate): `capyctl.request` in the caller's trace
#      with the caller's span as parent, its `capyctl.request_id` the answer's
#      X-CapyCTL-Request-Id, and §3's children (`capyctl.admission`,
#      `capyctl.queue_wait`, `capyctl.dispatch`, `capyctl.forward` with its
#      `first_progress` event, `capyctl.settle`).
#   c  a Clef-Flash decision on SGLang likewise.
#   d  a chat request and a decision without traceparent: a new trace, no parent.
#   e  each malformed traceparent (uppercase hex, all-zero trace ID, all-zero
#      parent ID, version ff, a short value, two headers): answered 200 like the
#      others, in a new trace whose request span has no parent.
#   f  every attribute and resource key of CapyCTL's spans in §5's allow-list.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

row_main() {
  local host engine=${2:-vllm} rc=0 chat=ot2-chat dec=ot2-dec mode
  host=$(resolve_host "${1:-a}") || return 1
  case $engine in vllm|sglang) ;; *) echo "OT2: the chat engine is vllm or sglang, not $engine" >&2; return 1 ;; esac
  ot_begin "$host" "$engine" decisions || return 1
  step collector-start otelcol_start "$host" ot2 || rc=1
  step start ot_start "$host" -- "${OT_SET_ON[@]}" "${OT_SET_TIMING[@]}" || rc=1
  if [ "$engine" = sglang ]; then ot_engines "$host" sglang || rc=1; else ot_engines "$host" vllm sglang || rc=1; fi
  step doc-chat ot_doc "$chat" "$engine" || rc=1
  step deploy-chat ot_deploy "$host" "$chat" || rc=1
  step status-chat ot_status "$host" "$chat" || rc=1
  step stream ot_req "$host" --route "$chat" --stream --traceparent sampled --tracestate capyctl=ot2 \
    --label ot2-chat-stream --expect 42 || rc=1
  step collected ot_req "$host" --route "$chat" --traceparent sampled --tracestate capyctl=ot2 \
    --label ot2-chat-collected --expect 42 || rc=1
  step no-traceparent ot_req "$host" --route "$chat" --label ot2-chat-none --expect 42 || rc=1
  for mode in upper zero-trace zero-parent version-ff short twice; do
    step "malformed-$mode" ot_req "$host" --route "$chat" --traceparent "$mode" --tracestate capyctl=ot2 \
      --label "ot2-chat-malformed-$mode" --expect 42 || rc=1
  done
  step delete-chat ot_delete "$host" "$chat" || rc=1
  step doc-decision ot_doc "$dec" decisions || rc=1
  step deploy-decision ot_deploy "$host" "$dec" || rc=1
  step status-decision ot_status "$host" "$dec" || rc=1
  step decision ot_req "$host" --route "$dec" --api decisions --traceparent sampled --label ot2-dec-traced || rc=1
  step decision-none ot_req "$host" --route "$dec" --api decisions --label ot2-dec-none || rc=1
  step export-wait ot_export_wait
  step status-end ot_status "$host" "$dec" || rc=1
  step delete-decision ot_delete "$host" "$dec" || rc=1
  ot_end "$host" || rc=1
  step spans ot_summary ot2 || rc=1
  return "$rc"
}
