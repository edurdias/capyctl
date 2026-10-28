# shellcheck shell=bash
# OT9 (ADR 0037 §11): zero retention turns engine tracing off for its
# deployment, and only that. A standalone role exporting to the host's
# collector with engine tracing on; two Qwen3-4B deployments on one engine
# (vLLM by default), one after the other:
#   run_row.sh OT9 --no-e0 -- a [vllm|sglang]
#
#   ot9-zero    `retention: zero`
#   ot9-traced  the same without it, for contrast
#
# Expected:
#   a  status: ot9-zero `engine tracing: off (retention_zero)`; ot9-traced
#      `engine tracing: on`.
#   b  procs-ot9-zero.json: no tracing option on the engine's command line, no
#      tracing variable in its environment; procs-ot9-traced.json: §7's.
#   c  spans-ot9.json: the requests to ot9-zero have their CapyCTL spans and no
#      engine span (its engine got no trace header and exported nothing); the
#      requests to ot9-traced have the engine's span under `capyctl.forward`.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

ot9_serve() { # ot9_serve <host> <name> <kind> [yaml lines...]
  local host=$1 d=$2 kind=$3 rc=0
  shift 3
  step "doc-$d" ot_doc "$d" "$kind" "$@" || return 1
  step "deploy-$d" ot_deploy "$host" "$d" || rc=1
  step "status-$d" ot_status "$host" "$d" || rc=1
  step "procs-$d" ot_procs "$host" "$d" || rc=1
  step "collected-$d" ot_req "$host" --route "$d" --traceparent sampled --label "ot9-$d-collected" --expect 42 || rc=1
  step "stream-$d" ot_req "$host" --route "$d" --stream --traceparent sampled --label "ot9-$d-stream" --expect 42 || rc=1
  step "export-wait-$d" ot_export_wait
  step "delete-$d" ot_delete "$host" "$d" || rc=1
  return "$rc"
}

row_main() {
  local host engine=${2:-vllm} rc=0
  host=$(resolve_host "${1:-a}") || return 1
  case $engine in vllm|sglang) ;; *) echo "OT9: engine is vllm or sglang, not $engine" >&2; return 1 ;; esac
  ot_begin "$host" "$engine" || return 1
  step collector-start otelcol_start "$host" ot9 || rc=1
  step start ot_start "$host" -- "${OT_SET_ON[@]}" "${OT_SET_ENGINES[@]}" || rc=1
  ot_engines "$host" "$engine" || rc=1
  ot9_serve "$host" ot9-zero "$engine" "retention: zero" || rc=1
  ot9_serve "$host" ot9-traced "$engine" || rc=1
  ot_end "$host" || rc=1
  step spans ot_summary ot9 || rc=1
  return "$rc"
}
