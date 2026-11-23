# shellcheck shell=bash
# OT4 (ADR 0037 §4): cold start, park and wake, and a switch, as activation
# traces linked from the waiting requests. A standalone role exporting to the
# host's collector with the timing header on, and a managed limit under which a
# vLLM and an SGLang Qwen3-4B deployment (both `residency: deep`) each fit and
# do not fit together (the LC4 method):
#   run_row.sh OT4 --no-e0 -- a [managed limit, default 40GiB]
#
# Expected (spans-ot4.json and its short summary):
#   a  cold start: ot4-v deployed, started and asked at once; the request's
#      `capyctl.activation_wait` links to an activation trace with
#      `capyctl.launch`, `capyctl.readiness`.
#   b  wake: ot4-v parked, then asked: the activation (kind `wake`) shows
#      `capyctl.wake`, `capyctl.readiness` and `capyctl.canary`.
#   c  switch: ot4-s deployed and started without room, then asked: its
#      activation shows the victim's (ot4-v) `capyctl.drain` and
#      `capyctl.release`, then `capyctl.reserve` and `capyctl.launch`; a
#      request back to ot4-v switches again.
#   d  for each of these requests, `timing_vs_spans`: the span durations of
#      `capyctl.activation_wait`, `capyctl.queue_wait` and `capyctl.request`
#      against the answer's x-capyctl-timing `activation_wait_ms`,
#      `queue_wait_ms` and `total_ms` (within a millisecond per phase).
#   e  activation traces are present whatever the ratio: the role runs at the
#      default 1.0 here; T48 covers ratio 0.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

row_main() {
  local host limit=${2:-40GiB} limit_bytes rc=0 v=ot4-v s=ot4-s
  host=$(resolve_host "${1:-a}") || return 1
  limit_bytes=$(python3 -c 'import sys; v=sys.argv[1]; print(int(v[:-3]) << 30 if v.endswith("GiB") else int(v))' "$limit")
  local tp=(--traceparent sampled --timeout 1800)
  ot_begin "$host" vllm sglang || return 1
  step collector-start otelcol_start "$host" ot4 || rc=1
  step start ot_start "$host" -- "${OT_SET_ON[@]}" "${OT_SET_TIMING[@]}" \
    --set "host.resource_policy.memory.system.managed_limit=$limit" || rc=1
  ot_engines "$host" vllm sglang || rc=1
  step doc-v ot_doc "$v" vllm "residency: deep" || rc=1
  step doc-s ot_doc "$s" sglang "residency: deep" || rc=1
  # (a) cold start
  step deploy-v ot_deploy "$host" "$v" --no-activate || rc=1
  step start-v ot_cli "$host" start deployment "$v" --format json || rc=1
  step cold-start timed cold-start ot_req "$host" --route "$v" "${tp[@]}" --label ot4-cold --expect 42 || rc=1
  step status-v-ready ot_status "$host" "$v" || rc=1
  # (b) park, then wake on request
  step park-v ot_cli "$host" park deployment "$v" --format json || rc=1
  step parked-v ot_wait_state "$host" "$v" parked 600 || rc=1
  step wake timed wake ot_req "$host" --route "$v" "${tp[@]}" --label ot4-wake --expect 42 || rc=1
  # (c) a switch forced by a request
  step deploy-s ot_deploy "$host" "$s" --no-activate || rc=1
  if ! step pressure ot_fits "$host" "$v" "$s" "$limit_bytes"; then
    echo "OT4: the two deployments fit together under $limit; no switch can be forced" >&2
    rc=1
  else
    step start-s ot_cli "$host" start deployment "$s" --format json || rc=1
    step switch-to-s timed switch-to-s ot_req "$host" --route "$s" "${tp[@]}" --label ot4-switch-s --expect 42 || rc=1
    step status-v-after-switch ot_status "$host" "$v" || rc=1
    step switch-to-v timed switch-to-v ot_req "$host" --route "$v" "${tp[@]}" --label ot4-switch-v --expect 42 || rc=1
    step status-s-after-switch ot_status "$host" "$s" || rc=1
  fi
  step export-wait ot_export_wait
  ot_end "$host" || rc=1
  step spans ot_summary ot4 || rc=1
  return "$rc"
}
