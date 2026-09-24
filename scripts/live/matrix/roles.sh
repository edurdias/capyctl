#!/usr/bin/env bash
# Role bring-up and tear-down for the matrix (plan unit W2).
#
#   roles.sh up [normal|tight]         preflight, server, both hosts enrolled and online, fixtures
#   roles.sh down                      SIGTERM both hosts, then the server (engines are retained, SPEC 4.3)
#   roles.sh preflight                 no engine or role running, trees match the snapshot
#   roles.sh server-init | server-up | server-down [SIG]
#   roles.sh host-init <host> [policy] run dir, `init host`, device probe, versions, host document
#   roles.sh host-doc <host> <policy>  regenerate and upload the host document (restart the host after)
#   roles.sh enroll <host>             invite on control-host, copy the invitation, `join host`
#   roles.sh host-up <host> [--debug-engine-logs] | host-down <host> [SIG]
#   roles.sh wait-online [timeout]     poll list hosts until both are online and reconciled
#   roles.sh fixtures [gen_deployment args]   all <v|s><92|17>-<model> fixtures for this run
#
# Hosts: host-a, host-b. DRY_RUN=1 prints the plan only.
. "$(dirname "$0")/lib.sh"

usage() { sed -n '2,16p' "$0"; exit 2; }

preflight() {
  local host want
  want=$(if dry; then echo "<snapshot-digest>"; else cat "$SNAPSHOT/tree.sha256"; fi)
  x bash -c "! ss -ltn | grep -Eq '127.0.0.1:(7443|8443)\\b'" || die "a server already listens on control-host"
  for host in "${MATRIX_HOSTS[@]}"; do
    rsh "$host" "if pgrep -af '$ENGINE_PGREP' | grep -vE 'pgrep|tailscaled|bash -c'; then echo 'an engine is running; refusing' >&2; exit 3; fi; \
if tmux ls 2>/dev/null | grep -q '^mx-'; then echo 'a matrix tmux session exists; refusing' >&2; exit 3; fi; \
test -x $REMOTE_TREE/target/release/mllm; cd $REMOTE_TREE && test \"\$($TREE_DIGEST_SH)\" = '$want'; \
echo \"\$(hostname) clock \$(date +%s%3N) boot \$(cat /proc/sys/kernel/random/boot_id)\"; grep MemAvailable /proc/meminfo"
    echo "control-host clock $(now_ms)"
  done
}

server_init() {
  if [ ! -f "$RUNSTATE/run.env" ] || [ "${1:-}" = --new ]; then
    save_run_var RUN "matrix-$(date -u +%Y%m%dT%H%M%SZ)"
    dry || save_run_var SNAPSHOT_DIGEST "$(cat "$SNAPSHOT/tree.sha256")"
  fi
  load_run
  x install -d -m 700 "$LRD"
  x install -m 755 "$LIVE/build/release/mllm" "$MLLM"
  x env "MLLM_STATE_DIR=$LRD/server" "$MLLM" init server --output "$SERVER_CFG"
  # Bootstrap and control listen on control-host's Tailscale address; management and
  # inference stay on loopback (ServerConfig::parse refuses anything else).
  # MLLM_TIMING_HEADER=1 turns on `observability.timing_header` (M80, SPEC 17).
  x python3 - "$SERVER_CFG" "$SERVER_IP" "${MLLM_TIMING_HEADER:-0}" <<'PY'
import json, os, sys
path, ip, timing = sys.argv[1], sys.argv[2], sys.argv[3]
doc = json.load(open(path))
doc["listeners"]["bootstrap"]["bind"] = f"{ip}:7444"
doc["listeners"]["control"]["bind"] = f"{ip}:7445"
doc["enrollment"] = {"bootstrap_address": f"https://{ip}:7444", "control_address": f"https://{ip}:7445"}
if timing == "1":
    doc["observability"] = {"timing_header": True}
fd = os.open(path + ".tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as handle:
    json.dump(doc, handle, indent=2)
os.replace(path + ".tmp", path)
PY
}

server_up() {
  load_run
  x tmux new-session -d -s "mx-srv-$RUN" "$MATRIX_DIR/role_exec.sh" "$LRD/server.pid" "$LRD/server.log" \
    "$MLLM" start server --config "$SERVER_CFG"
  local i
  for i in $(seq 1 60); do
    dry && break
    if cli list hosts --output json >/dev/null 2>&1; then echo "server up"; return 0; fi
    sleep 1
  done
  dry || die "server did not answer within 60 s (see $LRD/server.log)"
}

stop_role_local() { # stop_role_local <pidfile> <expect> <sig>
  x python3 "$MATRIX_DIR/signal_owned.py" --pidfile "$1" --expect "$2" --signal "$3"
  x bash -c "read -r pid _ < '$1'; for i in \$(seq 1 90); do [ -d /proc/\$pid ] || exit 0; sleep 1; done; echo 'still running after 90 s' >&2; exit 1"
}

server_down() {
  load_run
  stop_role_local "$LRD/server.pid" "start server" "${1:-TERM}"
}

host_init() {
  local host=$1 policy=${2:-normal} device sgv vv
  load_run
  rsh "$host" "[ -d $RRD ] || mkdir -p -m 700 $RRD; MLLM_STATE_DIR=$RRD/host $RBIN init host --output $RRD/host.yaml"
  # -B: the probe imports runtime modules, and a written __pycache__ makes the
  # host refuse every launch `runtime_integrity` (found live 2026-09-24).
  device=$(rsh_out "$host" '{"schema":"mllm-nvidia-inventory-v1","host_id":"'"$host"'","digest":"0000000000000000000000000000000000000000000000000000000000000000","devices":[{"physical_gpu_uuid":"GPU-00000000-0000-0000-0000-000000000000"}]}' \
    "cd $REMOTE_TREE && PYTHONDONTWRITEBYTECODE=1 python3 -B -m runtime.sglang_device")
  sgv=$(rsh_out "$host" 0.5.20 "$SGLANG_VENV/bin/python3 -c 'import sglang; print(sglang.__version__)' 2>/dev/null | tail -1")
  vv=$(rsh_out "$host" 0.29.0 "$(vllm_venv "$host")/bin/python3 -c 'import importlib.metadata as m; print(m.version(\"vllm\"))'")
  # The runtime tree must still pass the host's integrity rule after the probes.
  rsh "$host" "python3 -B $REMOTE_TREE/scripts/live/matrix/check_runtime.py $REMOTE_TREE/runtime >/dev/null || { python3 -B $REMOTE_TREE/scripts/live/matrix/check_runtime.py $REMOTE_TREE/runtime | grep FAIL >&2; exit 3; }"
  if ! dry; then
    mkdir -p "$RUNSTATE"
    printf '%s\n' "$device" >"$RUNSTATE/device-$host.json"
    printf 'sglang=%s\nvllm=%s\n' "$sgv" "$vv" >"$RUNSTATE/versions-$host.txt"
  fi
  host_doc "$host" "$policy" "$sgv" "$vv"
}

host_doc() {
  local host=$1 policy=$2 sgv=${3:-} vv=${4:-} doc
  load_run
  if [ -z "$sgv" ]; then
    if dry; then sgv=0.5.20 vv=0.29.0; else
      sgv=$(sed -n 's/^sglang=//p' "$RUNSTATE/versions-$host.txt"); vv=$(sed -n 's/^vllm=//p' "$RUNSTATE/versions-$host.txt"); fi
  fi
  doc=$RUNSTATE/host-$host-$policy.yaml
  x python3 "$MATRIX_DIR/gen_host_doc.py" --device-json "$RUNSTATE/device-$host.json" --ip "$(host_ip "$host")" \
    --run-root "$RRD" --policy "$policy" --sglang-version "$sgv" --vllm-version "$vv" \
    --vllm-venv "$(vllm_venv "$host")" --sglang-venv "$SGLANG_VENV" --remote-tree "$REMOTE_TREE" \
    --models-root "$MODELS_ROOT" --ingress-port "$INGRESS_PORT" --out "$doc"
  rcopy "$doc" "$host:$RRD/host.yaml"
  rsh "$host" "chmod 600 $RRD/host.yaml"
  save_run_var "POLICY_$(host_short "$host")" "$policy"
}

enroll() {
  local host=$1 out id
  load_run
  x "$MLLM" invite host --name "$host" --config "$SERVER_CFG" --output "$LRD/$host.join"
  rcopy "$LRD/$host.join" "$host:$RRD/$host.join"
  # The host must not be running (identity lock); the join file path is absolute.
  out=$(rsh_out "$host" '{"host_id":"01DRYRUNHOSTID'"$(host_short "$host")"'0000000000","enrolled":true}' \
    "chmod 600 $RRD/$host.join && $RBIN join host --join-file $RRD/$host.join --config $RRD/host.yaml")
  id=$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read().strip().splitlines()[-1])["host_id"])')
  save_run_var "HOST_ID_$(host_short "$host")" "$id"
  echo "$host enrolled as $id"
}

host_up() {
  local host=$1 debug=${2:-}
  load_run
  rsh "$host" "tmux new-session -d -s mx-host-$RUN $REMOTE_TREE/scripts/live/matrix/role_exec.sh $RRD/host.pid $RRD/host.log $RBIN start host --config $RRD/host.yaml $debug"
}

host_down() {
  local host=$1 sig=${2:-TERM}
  load_run
  rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile $RRD/host.pid --expect 'start host' --signal $sig && \
read -r pid _ < $RRD/host.pid && for i in \$(seq 1 90); do [ -d /proc/\$pid ] || exit 0; sleep 1; done; echo 'host role still running after 90 s' >&2; exit 1"
}

wait_online() {
  local timeout=${1:-120} i
  load_run
  dry || mkdir -p "$RUNSTATE"
  for i in $(seq 1 "$timeout"); do
    if dry; then cli list hosts --output json >/dev/null; break; fi
    cli list hosts --output json >"$RUNSTATE/hosts.json" 2>/dev/null || true
    if python3 - "$RUNSTATE/hosts.json" "${MATRIX_HOSTS[@]}" <<'PY'
import json, sys
hosts = {h["name"]: h for h in json.load(open(sys.argv[1])).get("hosts", [])}
ok = all(n in hosts and hosts[n].get("online") and (hosts[n].get("session") or {}).get("reconciled") for n in sys.argv[2:])
sys.exit(0 if ok else 1)
PY
    then echo "hosts online (after ${i}s)"; return 0; fi
    sleep 1
  done
  dry || die "hosts not online and reconciled within ${timeout}s; see $RUNSTATE/hosts.json"
}

fixtures() {
  load_run
  local extra=()
  [ -f "$RUNSTATE/measured.json" ] && extra+=(--measured "$RUNSTATE/measured.json")
  [ -f "$RUNSTATE/checkpoints.json" ] && extra+=(--checkpoints "$RUNSTATE/checkpoints.json")
  if dry; then
    x python3 "$MATRIX_DIR/gen_deployment.py" --hosts-json "$RUNSTATE/hosts.json" --out-dir "$RUNSTATE/fixtures" "${extra[@]}" --all "$@"
  else
    x python3 "$MATRIX_DIR/gen_deployment.py" --hosts-json "$RUNSTATE/hosts.json" --out-dir "$RUNSTATE/fixtures" "${extra[@]}" --all "$@" >/dev/null
    echo "fixtures in $RUNSTATE/fixtures"
  fi
}

cmd=${1:-}; shift || true
case $cmd in
  up)
    policy=${1:-normal}
    preflight
    server_init --new
    server_up
    for h in "${MATRIX_HOSTS[@]}"; do host_init "$h" "$policy"; enroll "$h"; host_up "$h"; done
    wait_online 180
    fixtures ;;
  down) for h in "${MATRIX_HOSTS[@]}"; do host_down "$h" || true; done; server_down ;;
  preflight) preflight ;;
  server-init) server_init "$@" ;;
  server-up) server_up ;;
  server-down) server_down "$@" ;;
  host-init) host_init "$@" ;;
  host-doc) [ $# -eq 2 ] || usage; host_doc "$@" ;;
  enroll) enroll "$@" ;;
  host-up) host_up "$@" ;;
  host-down) host_down "$@" ;;
  wait-online) wait_online "$@" ;;
  fixtures) fixtures "$@" ;;
  *) usage ;;
esac
