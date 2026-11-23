# shellcheck shell=bash
# OT11 (ADR 0037 §§7, 12): refusals. Nothing here starts an engine:
#   run_row.sh OT11 --no-e0 -- a
#
# Expected, each refused with the message the guide gives:
#   a  `engines: on` with an endpoint that is not on loopback (the host's own
#      private address) refuses the start: of a standalone role
#      (server.observability.tracing.endpoint, host.observability.tracing.engines),
#      and of the host's matrix role (`start host --config` with the same two
#      settings); the message names loopback. Nothing was started (idle).
#   b  a profile carrying an engine tracing option: `engine add` of the vLLM
#      0.32.0 environment with `--arg=--otlp-traces-endpoint`, and of the
#      SGLang 0.5.22 environment with `--arg=--enable-trace`: refused
#      ("reserved option ... is rendered by capyctl").
#   c  a profile carrying OTEL_EXPORTER_OTLP_ENDPOINT (`engine add --env`):
#      refused engine_env_reserved:OTEL_EXPORTER_OTLP_ENDPOINT.
#   d  an approval of `--otlp-traces-endpoint` (`engine add
#      --approve-option`): what `engine add` answers is recorded; a deployment
#      passing it in extra_args, on that profile or on v32, is refused
#      ("reserved option").
#   e  a deployment with OTEL_EXPORTER_OTLP_ENDPOINT in engine_config.env:
#      refused engine_env_reserved.
#   f  engine list and list deployments afterwards: only v32 (and the
#      approval's profile, when `engine add` accepted it, then removed); no
#      deployment.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

# ot11_start_refused <host> <role>: a start with engines on and a non-loopback
# endpoint; the exit status and message are printed, and a start still running
# after 60 s is stopped by timeout (that would be the finding).
ot11_start_refused() {
  local host=$1 role=$2 endpoint ip
  ip=$(host_ip "$host")
  endpoint=http://$ip:$OTELCOL_PORT/v1/traces
  case $role in
    standalone) rsh "$host" "rm -rf $OT_DIR/refused && mkdir -p -m 700 $OT_DIR && mkdir -m 700 $OT_DIR/refused && \
XDG_CONFIG_HOME=$OT_DIR/refused/cfg CAPYCTL_STATE_DIR=$OT_DIR/refused/state timeout 60 $(rbin "$host") start standalone \
--set server.observability.tracing.endpoint=$endpoint --set host.observability.tracing.engines=on; rc=\$?; echo \"exit \$rc\"; rm -rf $OT_DIR/refused; exit \$rc" ;;
    host) rsh "$host" "timeout 60 $(rbin "$host") start host --config $RRD/host.yaml \
--set observability.tracing.endpoint=$endpoint --set observability.tracing.engines=on; rc=\$?; echo \"exit \$rc\"; exit \$rc" ;;
  esac
}

row_main() {
  local host rc=0 appr=0
  host=$(resolve_host "${1:-a}") || return 1
  ot_begin "$host" vllm sglang || return 1
  # (a) the role's start
  step refuse-standalone refused_with loopback ot11_start_refused "$host" standalone || rc=1
  step refuse-host refused_with loopback ot11_start_refused "$host" host || rc=1
  step idle-after-refusals host_idle "$host" || rc=1
  # (b)-(e) profiles and deployments, against a role with no tracing
  step start ot_start "$host" || rc=1
  ot_engines "$host" vllm || rc=1
  step refuse-arg-vllm refused_with "reserved option" ot_add "$host" "$OT_VLLM_VENV" ot11-arg \
    "--arg=--otlp-traces-endpoint" "--arg=$OT_ENDPOINT" || rc=1
  step refuse-arg-sglang refused_with "reserved option" ot_add "$host" "$OT_SGLANG_VENV" ot11-sarg "--arg=--enable-trace" || rc=1
  step refuse-env refused_with engine_env_reserved ot_add "$host" "$OT_VLLM_VENV" ot11-env \
    "--env=OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:$OTELCOL_PORT" || rc=1
  if step approve-option ot_add "$host" "$OT_VLLM_VENV" ot11-appr "--approve-option=--otlp-traces-endpoint"; then
    dry || appr=1
  fi
  local extra=("  accept_extra_args: true" "  extra_args: [--otlp-traces-endpoint, '$OT_ENDPOINT']")
  step doc-extra ot_doc ot11-extra vllm "${extra[@]}" || rc=1
  step refuse-extra refused_with "reserved option" ot_deploy "$host" ot11-extra --no-activate || rc=1
  if [ "$appr" = 1 ]; then
    OT_DOC_PROFILE=ot11-appr step doc-extra-approved ot_doc ot11-extra-appr vllm "${extra[@]}" || rc=1
    step refuse-extra-approved refused_with "reserved option" ot_deploy "$host" ot11-extra-appr --no-activate || rc=1
  fi
  step doc-env ot_doc ot11-env vllm "  env: {OTEL_EXPORTER_OTLP_ENDPOINT: 'http://127.0.0.1:$OTELCOL_PORT'}" || rc=1
  step refuse-deploy-env refused_with engine_env_reserved ot_deploy "$host" ot11-env --no-activate || rc=1
  step engine-list-after ot_cli "$host" engine list --format json || rc=1
  step deployments-after ot_cli "$host" list deployments --format json || rc=1
  [ "$appr" = 1 ] && { step remove-approved ot_cli "$host" engine remove ot11-appr || rc=1; }
  ot_end "$host" || rc=1
  return "$rc"
}
