# shellcheck shell=bash
# OT8 (ADR 0037 §14): the cost of tracing, on one engine per run:
#   run_row.sh OT8 --no-e0 --tag vllm -- a vllm
#   run_row.sh OT8 --no-e0 --tag sglang -- a sglang
#   run_row.sh OT8 --no-e0 --tag llamacpp -- a llamacpp
#
# Each arm is OT8_N (default 200) identical streamed chat requests (Qwen3-4B,
# 256 output tokens, temperature 0) at a fixed concurrency OT8_CONCURRENCY
# (default 8), after OT8_WARMUP (default 10) warm-up requests, through a
# standalone role with the timing header on, restarted for each arm with its
# settings and a new collector run (collector-ot8-<arm>.jsonl):
#   off        no endpoint; requests carry a sampled caller context
#   sampled    the role exports (ratio 1.0); a sampled caller context
#   unsampled  the role exports; an unsampled caller context (flags 00): no
#              span recorded, the context still propagated
#   engines    the role exports with engine tracing on (vLLM and SGLang only)
# SGLang only, the trace level: CapyCTL renders SGLANG_TRACE_LEVEL=1 and
# reserves the name, so levels 1 and 3 are measured on SGLang directly (the
# approved environment, launched by the row outside CapyCTL on loopback port
# OT8_DIRECT_PORT, default 30123, with --mem-fraction-static OT8_MEM_FRACTION,
# default 0.3), each against the same launch without tracing:
#   direct-off, direct-l1, direct-l3   engine throughput only, no router
#
# Expected (cost-ot8.json and cost-ot8-direct.json): per arm, ok == n, output
# tokens per second at the fixed concurrency, and the router's phases from
# x-capyctl-timing (p50, p99); each arm against `off` (and the direct arms
# against `direct-off`). The record sets SGLang's level and is the cost the
# guide quotes.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

OT8_N=${OT8_N:-200}
OT8_CONCURRENCY=${OT8_CONCURRENCY:-8}
OT8_WARMUP=${OT8_WARMUP:-10}
OT8_DIRECT_PORT=${OT8_DIRECT_PORT:-30123}
OT8_MEM_FRACTION=${OT8_MEM_FRACTION:-0.3}

# ot8_arm <host> <engine> <arm> <traceparent mode> [standalone flags...]: one
# arm through CapyCTL, from a new role to its stop.
ot8_arm() {
  local host=$1 engine=$2 arm=$3 mode=$4 rc=0 d=ot8-dep
  shift 4
  local lg=(--route "$d" --traceparent "$mode" --concurrency "$OT8_CONCURRENCY" --max-tokens 256)
  step "collector-$arm" otelcol_start "$host" "ot8-$arm" || rc=1
  step "start-$arm" ot_start "$host" -- "${OT_SET_TIMING[@]}" "$@" || return 1
  step "deploy-$arm" ot_deploy "$host" "$d" || rc=1
  step "status-$arm" ot_status "$host" "$d" || rc=1
  step "warmup-$arm" ot_load "$host" "ot8-$arm-warmup" "${lg[@]}" --n "$OT8_WARMUP" || rc=1
  step "load-$arm" ot_load "$host" "ot8-$arm" "${lg[@]}" --n "$OT8_N" || rc=1
  step "export-wait-$arm" ot_export_wait
  step "status-after-$arm" ot_status "$host" "$d" || rc=1
  step "stop-$arm" ot_stop "$host" || rc=1
  step "collector-stop-$arm" otelcol_stop "$host" "ot8-$arm" || rc=1
  return "$rc"
}

# ot8_direct <host> <arm> <off|level>: SGLang launched directly on loopback,
# with or without tracing at a level; the load; then the launch stopped by its
# recorded identity and no SGLang process left.
ot8_direct() {
  local host=$1 arm=$2 level=$3 rc=0 env="" flags=""
  if [ "$level" != off ]; then
    env="OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf SGLANG_TRACE_LEVEL=$level"
    flags="--enable-trace --otlp-traces-endpoint $OT_ENDPOINT --otlp-service-name sglang"
  fi
  step "collector-$arm" otelcol_start "$host" "ot8-$arm" || rc=1
  step "launch-$arm" rsh "$host" "$MX_TMUX new-session -d -s mx-ot8-$arm-$RUN env $env \
$REMOTE_TREE/scripts/live/matrix/role_exec.sh $OT_HEV/sglang-$arm.pid $OT_HEV/sglang-$arm.log \
$OT_SGLANG_VENV/bin/python3 -m sglang.launch_server --model-path $MODELS_ROOT/$OT_CHAT_MODEL --served-model-name ot8-direct \
--host 127.0.0.1 --port $OT8_DIRECT_PORT --context-length 8192 --mem-fraction-static $OT8_MEM_FRACTION $flags" || return 1
  step "ready-$arm" rsh "$host" "for i in \$(seq 1 450); do curl -sf -m 5 http://127.0.0.1:$OT8_DIRECT_PORT/health >/dev/null && { echo 'ready after '\$((i * 2))' s'; exit 0; }; sleep 2; done; tail -n 40 $OT_HEV/sglang-$arm.log; exit 1" || rc=1
  local lg=(--url "http://127.0.0.1:$OT8_DIRECT_PORT" --no-auth --route ot8-direct --traceparent sampled
    --concurrency "$OT8_CONCURRENCY" --max-tokens 256)
  step "warmup-$arm" rsh "$host" "python3 $OT_PROBE load --label ot8-$arm-warmup --out $OT_HEV/load-ot8-$arm-warmup.jsonl ${lg[*]} --n $OT8_WARMUP" || rc=1
  step "load-$arm" rsh "$host" "python3 $OT_PROBE load --label ot8-$arm --out $OT_HEV/load-ot8-$arm.jsonl --summary $OT_HEV/load-ot8-$arm.summary.json ${lg[*]} --n $OT8_N" || rc=1
  step "export-wait-$arm" ot_export_wait
  step "stop-$arm" rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile $OT_HEV/sglang-$arm.pid --expect sglang.launch_server --signal TERM; \
for i in \$(seq 1 120); do pgrep -f 'sglang[.]launch_server|sglang::schedule[r]|sglang::detokenize[r]' >/dev/null || { echo 'no SGLang process left after '\$i' s'; exit 0; }; sleep 1; done; pgrep -af 'sglang[.]launch_server|sglang::' >&2; exit 1" || rc=1
  step "collector-stop-$arm" otelcol_stop "$host" "ot8-$arm" || rc=1
  return "$rc"
}

row_main() {
  local host engine=${2:-vllm} rc=0 arms=() arm hev cost=()
  host=$(resolve_host "${1:-a}") || return 1
  case $engine in vllm|sglang|llamacpp) ;; *) echo "OT8: engine is vllm, sglang or llamacpp, not $engine" >&2; return 1 ;; esac
  ot_begin "$host" "$engine" || return 1
  # The row's config directory (engines.yaml) outlives each arm's role.
  step start-register ot_start "$host" || rc=1
  ot_engines "$host" "$engine" || rc=1
  step doc ot_doc ot8-dep "$engine" || rc=1
  step stop-register ot_stop "$host" || rc=1
  ot8_arm "$host" "$engine" off sampled || rc=1
  ot8_arm "$host" "$engine" sampled sampled "${OT_SET_ON[@]}" || rc=1
  ot8_arm "$host" "$engine" unsampled unsampled "${OT_SET_ON[@]}" || rc=1
  arms=(off sampled unsampled)
  if [ "$engine" != llamacpp ]; then
    ot8_arm "$host" "$engine" engines sampled "${OT_SET_ON[@]}" "${OT_SET_ENGINES[@]}" || rc=1
    arms+=(engines)
  fi
  if [ "$engine" = sglang ]; then
    ot8_direct "$host" direct-off off || rc=1
    ot8_direct "$host" direct-l1 1 || rc=1
    ot8_direct "$host" direct-l3 3 || rc=1
  fi
  ot_end "$host" || rc=1
  hev=$EVID/host-$(host_short "$host")
  for arm in "${arms[@]}"; do cost+=(--arm "$arm=$hev/load-ot8-$arm.summary.json"); done
  step cost x python3 "$MATRIX_DIR/otel_spans.py" cost "${cost[@]}" --out "$EVID/cost-ot8.json" || rc=1
  if [ "$engine" = sglang ]; then
    step cost-direct x python3 "$MATRIX_DIR/otel_spans.py" cost --arm "direct-off=$hev/load-ot8-direct-off.summary.json" \
      --arm "direct-l1=$hev/load-ot8-direct-l1.summary.json" --arm "direct-l3=$hev/load-ot8-direct-l3.summary.json" \
      --out "$EVID/cost-ot8-direct.json" || rc=1
  fi
  for arm in "${arms[@]}"; do
    step "spans-$arm" ot_summary "ot8-$arm" --requests "$hev/load-ot8-$arm.jsonl" --label-prefix "ot8-$arm" || rc=1
  done
  return "$rc"
}
