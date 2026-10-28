# shellcheck shell=bash
# OT3 (ADR 0037 §5, §11): nothing of a request's content leaves in a span,
# CapyCTL's or an engine's. A standalone role exporting to the host's collector
# with engine tracing on (OT5's settings), so vLLM's and SGLang's spans are in
# the same file:
#   run_row.sh OT3 --no-e0 -- a
#
# Each sentinel is a fresh random marker (sentinels.txt in the evidence), sent
# with a sampled traceparent:
#   message  in the user message, collected and streamed, on vLLM and SGLang
#   system   in a system prompt
#   tool     in a tool definition, an assistant tool call's arguments and the
#            tool result
#   salt     as cache_salt
#   xrid     as X-Request-Id (the caller's ID, never a span attribute, §10)
#   model    as the model name of a request refused for an unknown model
#   engerr   in a field the engine refuses and quotes in its error: a forced
#            tool_choice the request does not define, and a json_schema
#            response_format whose type no validator knows (requests.jsonl
#            records whether the error body quoted it)
#   decision in a Clef-Flash decision's state and question (SGLang)
#
# Expected:
#   a  sentinels.json: every sentinel occurs 0 times in the collector's file,
#      in either case.
#   b  spans-ot3.json: each answered request has its capyctl.request span (the
#      export ran), engine spans present for vLLM and SGLang, and no attribute
#      or resource key of CapyCTL's spans outside §5's allow-list.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

OT3_KINDS=(message system tool salt xrid model engerr decision)
declare -A OT3_S=()

ot3_sentinels() {
  local kind
  for kind in "${OT3_KINDS[@]}"; do
    OT3_S[$kind]=OT3$(printf '%s' "$kind" | tr '[:lower:]' '[:upper:]')$(python3 -c 'import secrets; print(secrets.token_hex(8).upper())')
    printf '%s %s\n' "$kind" "${OT3_S[$kind]}"
  done | tee "$EVID/sentinels.txt"
  while read -r kind value; do OT3_S[$kind]=$value; done <"$EVID/sentinels.txt"
}

# ot3_chat <host> <deployment>: every chat sentinel against one chat deployment.
ot3_chat() {
  local host=$1 d=$2 rc=0
  local tp=(--traceparent sampled --tracestate capyctl=ot3)
  step "message-$d" ot_req "$host" --route "$d" "${tp[@]}" --sentinel-message "${OT3_S[message]}" --label "ot3-$d-message" || rc=1
  step "message-stream-$d" ot_req "$host" --route "$d" --stream "${tp[@]}" --sentinel-message "${OT3_S[message]}" --label "ot3-$d-message-stream" || rc=1
  step "system-$d" ot_req "$host" --route "$d" "${tp[@]}" --sentinel-system "${OT3_S[system]}" --label "ot3-$d-system" || rc=1
  step "tool-$d" ot_req "$host" --route "$d" "${tp[@]}" --sentinel-tool "${OT3_S[tool]}" --label "ot3-$d-tool" || rc=1
  # cache_salt may be refused by an engine that does not take it; the record says.
  step "salt-$d" ot_req "$host" --route "$d" "${tp[@]}" --cache-salt "${OT3_S[salt]}" --label "ot3-$d-salt"
  step "xrid-$d" ot_req "$host" --route "$d" "${tp[@]}" --x-request-id "${OT3_S[xrid]}" --label "ot3-$d-xrid" || rc=1
  step "engerr-tool-$d" ot_req "$host" --route "$d" "${tp[@]}" --engine-error "${OT3_S[engerr]}" \
    --engine-error-kind tool_choice --label "ot3-$d-engerr-tool" --expect-status 400
  step "engerr-schema-$d" ot_req "$host" --route "$d" "${tp[@]}" --engine-error "${OT3_S[engerr]}" \
    --engine-error-kind response_format --label "ot3-$d-engerr-schema" --expect-status 400
  return "$rc"
}

row_main() {
  local host rc=0 d s args=()
  host=$(resolve_host "${1:-a}") || return 1
  ot_begin "$host" vllm sglang decisions || return 1
  step sentinels ot3_sentinels || return 1
  step collector-start otelcol_start "$host" ot3 || rc=1
  step start ot_start "$host" -- "${OT_SET_ON[@]}" "${OT_SET_ENGINES[@]}" || rc=1
  ot_engines "$host" vllm sglang || rc=1
  step unknown-model ot_req "$host" --route ot3-none --model "${OT3_S[model]}" --traceparent sampled \
    --label ot3-unknown-model --expect-status 404
  for d in ot3-v:vllm ot3-s:sglang; do
    step "doc-${d%%:*}" ot_doc "${d%%:*}" "${d#*:}" || rc=1
    step "deploy-${d%%:*}" ot_deploy "$host" "${d%%:*}" || rc=1
    step "status-${d%%:*}" ot_status "$host" "${d%%:*}" || rc=1
    ot3_chat "$host" "${d%%:*}" || rc=1
    step "delete-${d%%:*}" ot_delete "$host" "${d%%:*}" || rc=1
  done
  step doc-ot3-d ot_doc ot3-d decisions || rc=1
  step deploy-ot3-d ot_deploy "$host" ot3-d || rc=1
  step decision ot_req "$host" --route ot3-d --api decisions --traceparent sampled \
    --sentinel-message "${OT3_S[decision]}" --x-request-id "${OT3_S[xrid]}" --label ot3-d-decision || rc=1
  step export-wait ot_export_wait
  step delete-ot3-d ot_delete "$host" ot3-d || rc=1
  ot_end "$host" || rc=1
  for s in "${OT3_KINDS[@]}"; do args+=(--sentinel "${OT3_S[$s]}"); done
  step sentinel-counts x python3 "$MATRIX_DIR/otel_spans.py" sentinels \
    --collector "$EVID/host-$(host_short "$host")/collector-ot3.jsonl" "${args[@]}" --out "$EVID/sentinels.json" || rc=1
  step spans ot_summary ot3 || rc=1
  return "$rc"
}
