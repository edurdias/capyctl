# shellcheck shell=bash
# OT6 (ADR 0037 §§4, 6): a remote host. The run's server on the control host
# and the host's matrix role (host A by default), both exporting, through the
# matrix roles (roles.sh up first):
#   run_row.sh OT6 -- a
#
# The collector runs on the host, bound to loopback. The server's endpoint is
# a loopback port on the control host (OT_TUNNEL_PORT, default 14318) that an
# SSH local forward, started and stopped by the row, carries to the host's
# collector, so the server's and the host's spans land in one file and no
# collector listens beyond loopback. The host registers the approved vLLM 0.32.0
# and SGLang 0.5.22 environments as ot-v32 and ot-s22 (`engine add` into
# $RRD/engines.yaml, removed at the end). The row restores the server's
# document and the host's matrix document afterwards.
#
# Arm 1, the host with `observability.tracing` {endpoint: its collector,
# engines: on}; arm 2, the host restarted with its matrix document (no
# endpoint) while both engines keep running (retained, SPEC 4.3), so their
# launches keep engine tracing.
#
# Expected:
#   a  list hosts shows the host's tracing on in arm 1 and off in arm 2; status
#      shows the server's tracing on and each instance's engine tracing.
#   b  arm 1 (spans-ot6-a1.json), per request with a sampled caller context:
#      `capyctl.ingress` (service capyctl-host) has the router's
#      `capyctl.forward` as parent; the engine's span (vLLM `llm_request`,
#      SGLang's request span) has `capyctl.ingress` as parent.
#   c  arm 1, the wake of ot6-v after a park: the activation trace shows
#      `capyctl.host.wake` under the server's `capyctl.wake`.
#   d  arm 2 (spans-ot6-a2.json): no `capyctl.ingress` span; the host passed the
#      router's context on, and the engine's span has the router's
#      `capyctl.forward` as parent.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

OT6_TUNNEL_PORT=${OT_TUNNEL_PORT:-14318}
OT6_SERVER_BACKUP=$LRD/server.yaml.pre-ot6

# ot6_tunnel_up <host>: the control host's loopback port forwarded to the
# host's collector, under the harness's tmux server with a recorded identity.
ot6_tunnel_up() {
  local host=$1 _
  x bash -c "! ss -Hltn | grep -Eq '[:.]$OT6_TUNNEL_PORT\\b'" || { echo "port $OT6_TUNNEL_PORT is in use on the control host" >&2; return 1; }
  # shellcheck disable=SC2086  # MX_TMUX is "tmux -L <socket>", as in roles.sh
  x $MX_TMUX new-session -d -s "mx-otel-tunnel-$RUN" "$MATRIX_DIR/role_exec.sh" "$LRD/otel-tunnel.pid" "$EVID/otel-tunnel.log" \
    ssh -N -o BatchMode=yes -o ConnectTimeout=15 -o ExitOnForwardFailure=yes \
    -L "127.0.0.1:$OT6_TUNNEL_PORT:$OTELCOL_LISTEN" "$host" || return 1
  dry && return 0
  for _ in $(seq 1 30); do
    if ss -Hltn | grep -Eq "127[.]0[.]0[.]1:$OT6_TUNNEL_PORT\b"; then echo "127.0.0.1:$OT6_TUNNEL_PORT -> $host $OTELCOL_LISTEN"; return 0; fi
    sleep 1
  done
  return 1
}

ot6_tunnel_down() {
  x python3 "$MATRIX_DIR/signal_owned.py" --pidfile "$LRD/otel-tunnel.pid" --expect "ssh -N" --signal TERM
}

# ot6_add <host> <path> <profile> / ot6_remove <host> <profile>: `engine` on the
# host, against its matrix document (as ENG1).
ot6_add() {
  rsh "$1" "if [ -x /usr/local/cuda/bin/nvcc ]; then export CUDA_HOME=/usr/local/cuda; fi; $(rbin "$1") engine add $2 --name $3 --config $RRD/host.yaml"
}
ot6_remove() { rsh "$1" "$(rbin "$1") engine remove $2 --config $RRD/host.yaml"; }

# ot6_server_tracing on|off: restart the server with
# observability.tracing.endpoint (the tunnel), or with its own document back.
ot6_server_tracing() {
  local mode=$1
  "$MATRIX_DIR/roles.sh" server-down || return 1
  if [ "$mode" = on ]; then
    x cp -p "$SERVER_CFG" "$OT6_SERVER_BACKUP" || return 1
    x python3 - "$SERVER_CFG" "http://127.0.0.1:$OT6_TUNNEL_PORT/v1/traces" <<'PY' || return 1
import json, os, sys
path, endpoint = sys.argv[1:3]
doc = json.load(open(path))
doc.setdefault("observability", {})["tracing"] = {"endpoint": endpoint}
fd = os.open(path + ".tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as handle:
    json.dump(doc, handle, indent=2)
os.replace(path + ".tmp", path)
print(json.dumps(doc["observability"]))
PY
  else
    x mv "$OT6_SERVER_BACKUP" "$SERVER_CFG" || return 1
  fi
  "$MATRIX_DIR/roles.sh" server-up && "$MATRIX_DIR/roles.sh" wait-online 180
}

# ot6_host_tracing <host> <policy> on|off: restart the host role with its matrix
# document plus observability.tracing (on), or with its matrix document (off).
ot6_host_tracing() {
  local host=$1 policy=$2 mode=$3 doc
  "$MATRIX_DIR/roles.sh" host-down "$host" || return 1
  if [ "$mode" = on ]; then
    doc=$EVID/host-$host-ot6.yaml
    x python3 - "$RUNSTATE/host-$host-$policy.yaml" "$doc" "$OT_ENDPOINT" <<'PY' || return 1
import json, os, sys
src, out, endpoint = sys.argv[1:4]
doc = json.load(open(src))
doc.setdefault("observability", {})["tracing"] = {"endpoint": endpoint, "engines": "on"}
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as handle:
    json.dump(doc, handle, indent=1)
print(json.dumps(doc["observability"]))
PY
    rcopy "$doc" "$host:$RRD/host.yaml" || return 1
    rsh "$host" "chmod 600 $RRD/host.yaml" || return 1
  else
    "$MATRIX_DIR/roles.sh" host-doc "$host" "$policy" || return 1
  fi
  "$MATRIX_DIR/roles.sh" host-up "$host" && "$MATRIX_DIR/roles.sh" wait-online 180
}

# ot6_deploy <host> <name> <profile>: a Qwen3-4B deployment on the host, deep
# residency, through the server.
ot6_deploy() {
  local host=$1 name=$2 profile=$3 file=$EVID/$2.yaml
  printf 'schema_version: 1\nkind: deployment\nname: %s\nhost: %s\nengine: %s\nmodel: %s\nresidency: deep\nlmcache: off\nengine_config:\n  context_length: 8192\n' \
    "$name" "$host" "$profile" "$MODELS_ROOT/$OT_CHAT_MODEL" >"$file"
  cat "$file"
  dry || echo "$name" >>"$EVID/deployed.txt"
  cli deploy model --file "$file" --activate --wait --format json
}

# ot6_req <otel_probe.py chat args...>: a request through the server's router.
ot6_req() { x python3 "$MATRIX_DIR/otel_probe.py" chat --url "$CAPYCTL_INFER_URL" --out "$EVID/requests.jsonl" "$@"; }

row_main() {
  local host code policy pv rc=0 d arm
  host=$(resolve_host "${1:-a}") || return 1
  code=$(host_short "$host")
  pv=POLICY_$code
  policy=${!pv:-normal}
  step evidence-dir ot_hev_init "$host" || return 1
  step precondition ot_pre "$host" vllm sglang || return 1
  step online "$MATRIX_DIR/roles.sh" wait-online 60 || return 1
  step engines-file-absent rsh "$host" "! test -e $RRD/engines.yaml" || return 1
  step collector-start otelcol_start "$host" ot6 || rc=1
  step tunnel-up ot6_tunnel_up "$host" || rc=1
  step add-ot-v32 ot6_add "$host" "$OT_VLLM_VENV" ot-v32 || rc=1
  step add-ot-s22 ot6_add "$host" "$OT_SGLANG_VENV" ot-s22 || rc=1
  step engines cli list engines --format json || rc=1
  step server-tracing-on ot6_server_tracing on || rc=1
  step host-tracing-on ot6_host_tracing "$host" "$policy" on || rc=1
  step hosts-a1 cli list hosts --format json || rc=1
  step deploy-ot6-v ot6_deploy "$host" ot6-v ot-v32 || rc=1
  step deploy-ot6-s ot6_deploy "$host" ot6-s ot-s22 || rc=1
  for arm in a1 a2; do
    if [ "$arm" = a2 ]; then
      step host-tracing-off ot6_host_tracing "$host" "$policy" off || rc=1
      step hosts-a2 cli list hosts --format json || rc=1
    fi
    for d in ot6-v ot6-s; do
      step "status-$arm-$d" status_dep "$d" || rc=1
      step "collected-$arm-$d" ot6_req --route "$d" --traceparent sampled --tracestate capyctl=ot6 \
        --label "ot6-$arm-$d-collected" --expect 42 || rc=1
      step "stream-$arm-$d" ot6_req --route "$d" --stream --traceparent sampled --label "ot6-$arm-$d-stream" --expect 42 || rc=1
    done
    if [ "$arm" = a1 ]; then
      step park-ot6-v park_dep ot6-v || rc=1
      step parked-ot6-v wait_state ot6-v parked 600 || rc=1
      step wake-ot6-v timed wake ot6_req --route ot6-v --traceparent sampled --label ot6-a1-wake --expect 42 --timeout 1800 || rc=1
    fi
  done
  step export-wait ot_export_wait
  step delete-ot6-v cli delete deployment ot6-v --stop --format json || rc=1
  step delete-ot6-s cli delete deployment ot6-s --stop --format json || rc=1
  step idle host_idle "$host" || rc=1
  step remove-ot-v32 ot6_remove "$host" ot-v32 || rc=1
  step remove-ot-s22 ot6_remove "$host" ot-s22 || rc=1
  step engines-file-removed rsh "$host" "rm -f $RRD/engines.yaml $RRD/engines.yaml.lock" || rc=1
  step server-tracing-off ot6_server_tracing off || rc=1
  step tunnel-down ot6_tunnel_down || rc=1
  otelcol_stop_all || rc=1
  step fetch ot_fetch "$host" || rc=1
  for arm in a1 a2; do
    step "spans-$arm" x python3 "$MATRIX_DIR/otel_spans.py" summary --collector "$EVID/host-$code/collector-ot6.jsonl" \
      --requests "$EVID/requests.jsonl" --label-prefix "ot6-$arm" --out "$EVID/spans-ot6-$arm.json" || rc=1
  done
  return "$rc"
}
