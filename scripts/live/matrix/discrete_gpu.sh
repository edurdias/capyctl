#!/usr/bin/env bash
# Live rows DG1-DG7 on a discrete-GPU machine (plan Task 19). It runs on that
# machine itself: a standalone role (or a server and a host role for DG7) of the
# binary under test, the vLLM and SGLang environments registered with
# `mllm engine add`, and two instruct models in the models directory.
#
#   scripts/live/matrix/discrete_gpu.sh prepare     # register both engines
#   scripts/live/matrix/discrete_gpu.sh dg1         # one row
#   scripts/live/matrix/discrete_gpu.sh all         # DG1-DG6 (DG7 is optional)
#   scripts/live/matrix/discrete_gpu.sh dgx         # restart_only with a pinned GPU
#
# Settings come from the untracked hosts.local.env (see hosts.example.env):
# DGPU_HOST, DGPU_VLLM_VENV, DGPU_SGLANG_VENV, DGPU_MODEL_A (the larger model)
# and DGPU_MODEL_B, plus optional DGPU_LISTEN_ADDR (this machine's own
# tailnet or LAN address, for DG6), DGPU_PREVIOUS_BIN (a previous release, for
# DG6) and DGPU_STATE (default ~/mllm-dgpu-live; owner-only ancestors).
#
# Evidence goes to target/live/dgpu/<row>.log (untracked). The harness records
# what happened; it does not decide whether a row passed. CPU and Fake-engine
# tests are not qualification, and neither is a dry run of this script.
#
# Safety: the inference listener is loopback or DGPU_LISTEN_ADDR, always with
# the API key; the key is read into the environment and never printed. On exit
# the trap stops only what this harness started: the deployments it created
# (drained with verified cleanup) and its own role processes.
set -euo pipefail

MATRIX_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$MATRIX_DIR/../../.." && pwd)
HOSTS_ENV=${MLLM_MATRIX_HOSTS_ENV:-$MATRIX_DIR/hosts.local.env}
die() { echo "discrete_gpu: $*" >&2; exit 2; }
[ -f "$HOSTS_ENV" ] || die "missing $HOSTS_ENV (copy hosts.example.env and set the DGPU_ values)"
# shellcheck disable=SC1090
. "$HOSTS_ENV"
for _v in DGPU_HOST DGPU_VLLM_VENV DGPU_SGLANG_VENV DGPU_MODEL_A DGPU_MODEL_B; do
  [ -n "${!_v:-}" ] || die "$HOSTS_ENV does not set $_v (see hosts.example.env)"
done
unset _v

MLLM_BIN=${MLLM_BIN:-$REPO/target/release/mllm}
STATE=${DGPU_STATE:-$HOME/mllm-dgpu-live}
LIVE=${MLLM_DGPU_LIVE:-$REPO/target/live/dgpu}
PORT=8443
PORTS=${DGPU_ENGINE_PORTS:-8300-8399}
mkdir -p "$LIVE"
mllm() { "$MLLM_BIN" --state-dir "$STATE" "$@"; }
ROLE_PID=
ROLE_LOG=
CREATED=()
LOG=$LIVE/harness.log

say() { echo "[$(date +%T)] $*" | tee -a "$LOG"; }

gpu_used() { nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -1; }
gpu_apps() {
  nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits |
    while IFS=', ' read -r pid mib; do
      [ -n "$pid" ] || continue
      host=$(awk '/^RssAnon:|^RssShmem:/ {s += $2} END {print int(s / 1024)}' "/proc/$pid/status" 2>/dev/null || echo "?")
      echo "pid=$pid gpu_mib=$mib host_mib=$host"
    done
}
state_of() { mllm list deployments 2>/dev/null | awk -v n="$1" '$1 == n {print $4}'; }
wait_state() { # wait_state <deployment> <state> [seconds]
  local i
  for i in $(seq $((${3:-120} * 2))); do
    [ "$(state_of "$1")" = "$2" ] && return 0
    sleep 0.5
  done
  return 1
}

start_role() { # start_role <log name> [start args...]
  local log=$LIVE/$1.log
  ROLE_LOG=$log
  shift
  setsid "$MLLM_BIN" --state-dir "$STATE" start standalone --engine-ports "$PORTS" "$@" \
    >"$log" 2>&1 </dev/null &
  ROLE_PID=$!
  local i
  for i in $(seq 120); do
    grep -q "standalone ready" "$log" && { say "standalone up (pid $ROLE_PID): $*"; return 0; }
    kill -0 "$ROLE_PID" 2>/dev/null || { cat "$log" >&2; die "standalone exited at start"; }
    sleep 0.5
  done
  die "standalone did not become ready"
}
stop_role() {
  [ -n "$ROLE_PID" ] || return 0
  kill -TERM "$ROLE_PID" 2>/dev/null || true
  local i
  for i in $(seq 120); do kill -0 "$ROLE_PID" 2>/dev/null || break; sleep 0.5; done
  ROLE_PID=
}

cleanup() {
  local status=$?
  if [ -n "$ROLE_PID" ] && [ "${#CREATED[@]}" -gt 0 ]; then
    for d in "${CREATED[@]}"; do mllm delete deployment "$d" --stop >/dev/null 2>&1 || true; done
    mllm drain standalone >/dev/null 2>&1 || true
  fi
  stop_role
  exit "$status"
}
trap cleanup EXIT INT TERM

# deploy <name> <engine> <model> [residency] [extra YAML lines...]
deploy() {
  local name=$1 engine=$2 model=$3 residency=${4:-} file=$LIVE/deploy-$1.yaml
  shift 3; [ $# -gt 0 ] && shift
  {
    echo "name: $name"
    echo "engine: $engine"
    echo "model: $model"
    [ -n "$residency" ] && echo "residency: $residency"
    # SGLang without FlashInfer on this machine: the Triton attention backend.
    if [ "$engine" = sglang ]; then
      echo "engine_config:"
      echo "  accept_extra_args: true"
      echo "  extra_args: [\"--attention-backend\", \"triton\", \"--sampling-backend\", \"pytorch\"]"
    fi
    local line
    for line in "$@"; do echo "$line"; done
  } >"$file"
  say "deploy $name ($engine, $model, ${residency:-default residency})"
  mllm deploy model --file "$file" 2>&1 | grep -v '^warning' | tail -1 | tee -a "$LOG"
  CREATED+=("$name")
  # The checkpoint digest is measured after acceptance; wait for it.
  local i
  for i in $(seq 240); do
    mllm --json status deployment "$name" 2>/dev/null |
      grep -q '"checkpoint_digest":{[^}]*"state":"recorded"' && return 0
    sleep 0.5
  done
  say "$name: checkpoint digest not recorded after 120 s"
}
undeploy() { mllm delete deployment "$1" --stop 2>&1 | tail -1 | tee -a "$LOG"; }

key() { sed -n 's/^api_key: *//p' "$STATE/identity/credentials" | tr -d '"'; }
# request <row log> <route> [addr]: one chat completion, timed.
request() {
  local log=$1 route=$2 addr=${3:-127.0.0.1:$PORT} t0 out code
  t0=$EPOCHREALTIME
  out=$(curl -s --max-time 900 -H "Authorization: Bearer $(key)" -H 'Content-Type: application/json' \
    -d "{\"model\":\"$route\",\"messages\":[{\"role\":\"user\",\"content\":\"Say hello in five words.\"}],\"max_tokens\":24}" \
    -w ' HTTP%{http_code}' "http://$addr/v1/chat/completions" || echo " HTTP000")
  code=${out##* HTTP}
  {
    printf '%s request %s: HTTP %s in %.2fs\n' "$(date +%T)" "$route" "$code" "$(echo "$EPOCHREALTIME - $t0" | bc)"
    echo "  body: ${out% HTTP*}" | cut -c1-220
    echo "  gpu used MiB: $(gpu_used)"; gpu_apps | sed 's/^/  /'
    mllm list deployments 2>/dev/null | sed 's/^/  /'
    echo "  MemAvailable MiB: $(awk '/MemAvailable/ {print int($2 / 1024)}' /proc/meminfo)"
  } | tee -a "$LIVE/$log.log"
  [ "$code" = 200 ]
}
switch_notes() { # the switch outcomes this run's role logged
  grep -h '"event":"switch"' "$ROLE_LOG" 2>/dev/null | grep -E '"phase":"(Released|Failed)"' |
    sed 's/.*"detail":"\([^"]*\)".*/  switch: \1/' | tail -"${1:-4}"
}

prepare() {
  mllm engine list 2>/dev/null | awk '{print $1}' | grep -qx vllm || mllm engine add "$DGPU_VLLM_VENV"
  # An SGLang profile carries no host-fixed arguments; the Triton attention
  # backend goes in each SGLang deployment's extra_args (deploy above).
  mllm engine list 2>/dev/null | awk '{print $1}' | grep -qx sglang || mllm engine add "$DGPU_SGLANG_VENV"
  mllm engine list | tee -a "$LOG"
}

# The switching rows share one shape: A cold, B (A released), A (B released), B again.
switch_row() { # switch_row <row> <engine A> <engine B> <residency>
  local row=$1 ea=$2 eb=$3 tier=$4 a=$1-a b=$1-b
  deploy "$a" "$ea" "$DGPU_MODEL_A" "$tier"
  deploy "$b" "$eb" "$DGPU_MODEL_B" "$tier"
  request "$row" "$a" || true
  request "$row" "$b" || true; switch_notes 2 | tee -a "$LIVE/$row.log"
  request "$row" "$a" || true; switch_notes 2 | tee -a "$LIVE/$row.log"
  request "$row" "$b" || true; switch_notes 2 | tee -a "$LIVE/$row.log"
  # A wake with nothing else to release: A ready alone, parked explicitly,
  # then requested.
  undeploy "$b"
  request "$row" "$a" || true
  local t0=$EPOCHREALTIME
  mllm park deployment "$a" >/dev/null 2>&1 || true
  wait_state "$a" parked 120 || say "$a did not park"
  printf '  parked %s in %.2fs: gpu used MiB %s\n' "$a" "$(echo "$EPOCHREALTIME - $t0" | bc)" "$(gpu_used)" |
    tee -a "$LIVE/$row.log"; gpu_apps | sed 's/^/  /' | tee -a "$LIVE/$row.log"
  request "$row" "$a" || true
  undeploy "$a"
}

dg1_vllm_host_backed() { switch_row dg1 vllm vllm host_backed; }
dg2_vllm_deep() { switch_row dg2 vllm vllm deep; }
dg3_sglang() { switch_row dg3 sglang sglang host_backed; switch_row dg3d sglang sglang deep; }
dg4_mixed() { switch_row dg4 vllm sglang host_backed; }

# A deployment whose derived memory request exceeds the card (the larger
# model with a 12 GiB KV cache): accepted provisionally while its checkpoint is
# measured, then refused with exit 4 and insufficient_device_memory, within
# 30 s of the deploy, with no engine started.
dg5_refusal() {
  local t0 status=0 before
  before=$(gpu_apps | wc -l)
  t0=$EPOCHREALTIME
  printf 'name: dg5-big\nengine: vllm\nmodel: %s\nengine_config:\n  memory:\n    kv_cache: 12GiB\n' \
    "$DGPU_MODEL_A" >"$LIVE/deploy-dg5-big.yaml"
  mllm deploy model --file "$LIVE/deploy-dg5-big.yaml" >"$LIVE/dg5.out" 2>&1 || status=$?
  CREATED+=(dg5-big)
  printf 'deploy exit %s after %.2fs\n' "$status" "$(echo "$EPOCHREALTIME - $t0" | bc)" | tee -a "$LIVE/dg5.log"
  status=0
  mllm start deployment dg5-big --wait >"$LIVE/dg5-start.out" 2>&1 || status=$?
  {
    printf 'start --wait exit %s, %.2fs after the deploy\n' "$status" "$(echo "$EPOCHREALTIME - $t0" | bc)"
    grep -v '^warning\|^Request identity' "$LIVE/dg5-start.out" | tail -1
    echo "engine processes before/after: $before/$(gpu_apps | wc -l)"
  } | tee -a "$LIVE/dg5.log"
}

# restart_only on a discrete GPU (a release is a stop, a return a cold start)
# with the GPU pinned in the deployment: the engine sees only that GPU, by
# UUID (only the CUDA_* variables of the engine's environment are read).
dgx_pin_restart_only() {
  local pin='devices: [{id: gpu0, sharing: shared}]'
  deploy dgx-a vllm "$DGPU_MODEL_A" restart_only "$pin"
  deploy dgx-b vllm "$DGPU_MODEL_B" restart_only "$pin"
  request dgx dgx-a || true
  local pid
  for pid in $(nvidia-smi --query-compute-apps=pid --format=csv,noheader); do
    echo "  engine pid $pid: $(tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null |
      grep -E '^CUDA_(VISIBLE_DEVICES|DEVICE_ORDER)=' | tr '\n' ' ')" | tee -a "$LIVE/dgx.log"
  done
  echo "  GPU UUIDs: $(nvidia-smi --query-gpu=index,uuid --format=csv,noheader | tr '\n' ' ')" | tee -a "$LIVE/dgx.log"
  request dgx dgx-b || true; switch_notes 2 | tee -a "$LIVE/dgx.log"
  request dgx dgx-a || true; switch_notes 2 | tee -a "$LIVE/dgx.log"
  undeploy dgx-a; undeploy dgx-b
}

# The listener: loopback and DGPU_LISTEN_ADDR with the key. A keyless run on a
# non-loopback address is not made here (owner rule for this machine); its
# warning is covered by the CPU tests only.
dg6_network() {
  local addr=${DGPU_LISTEN_ADDR:?set DGPU_LISTEN_ADDR to this machine\'s own address}
  stop_role; start_role role-dg6 --listen "$addr:$PORT"
  {
    echo "listen $addr:$PORT"
    echo "with key: HTTP $(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $(key)" "http://$addr:$PORT/v1/models")"
    echo "without key: HTTP $(curl -s -o /dev/null -w '%{http_code}' "http://$addr:$PORT/v1/models")"
    echo "wrong key: HTTP $(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer wrong' "http://$addr:$PORT/v1/models")"
    echo "loopback while bound to $addr: HTTP $(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$PORT/v1/models" || true)"
  } | tee -a "$LIVE/dg6.log"
  stop_role; start_role role-dg6b --listen "127.0.0.1:$PORT"
  echo "bound to loopback, $addr: HTTP $(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://$addr:$PORT/v1/models" || true)" |
    tee -a "$LIVE/dg6.log"
  mllm config show 2>&1 | grep -E 'SETTING|inference|models_root|model_store|engine_ports|endpoint_port' | tee -a "$LIVE/dg6.log"
}

# Upgrade from a previous release: its standalone document keeps the old
# loopback default; the first start of this build moves it to all interfaces
# once, with a notice and a backup beside the document.
dg6_migration() {
  local old=${DGPU_PREVIOUS_BIN:?set DGPU_PREVIOUS_BIN to a previous release binary}
  local mig=${STATE}-migration pid
  [ ! -e "$mig" ] || die "$mig exists; remove it first"
  mkdir -m 700 "$mig"
  "$old" --version | tee -a "$LIVE/dg6.log"
  # A previous release names its state root by variable only.
  MLLM_STATE_DIR=$mig MLLM_VLLM_BIN=$DGPU_VLLM_VENV/bin/vllm MLLM_MODELS_ROOT=$HOME/models \
    setsid "$old" start standalone >"$LIVE/dg6-previous.log" 2>&1 </dev/null &
  pid=$!; sleep 8; kill -TERM "$pid"; wait "$pid" 2>/dev/null || true
  grep -n 'bind' "$mig/config/standalone.yaml" | tee -a "$LIVE/dg6.log"
  local i
  for i in 1 2; do
    setsid "$MLLM_BIN" --state-dir "$mig" start standalone --engine-ports "$PORTS" \
      --listen "127.0.0.1:$PORT" >"$LIVE/dg6-upgrade-$i.log" 2>&1 </dev/null &
    pid=$!; sleep 8; kill -TERM "$pid"; wait "$pid" 2>/dev/null || true
    echo "start $i notices: $(grep -ci 'notice\|0.0.0.0' "$LIVE/dg6-upgrade-$i.log")" | tee -a "$LIVE/dg6.log"
    grep -i 'notice\|0.0.0.0' "$LIVE/dg6-upgrade-$i.log" | head -3 | tee -a "$LIVE/dg6.log"
  done
  grep -n 'bind' "$mig/config/standalone.yaml" | tee -a "$LIVE/dg6.log"
  ls "$mig/config" | tee -a "$LIVE/dg6.log"
}

# Optional for 0.1.0: a server and a host role on this machine. Recorded
# pending when not run.
dg7_remote_host() { echo "DG7 not run by this harness yet (optional for 0.1.0)" | tee -a "$LIVE/dg7.log"; }

run() {
  case $1 in
    prepare) prepare ;;
    dg1) dg1_vllm_host_backed ;;
    dg2) dg2_vllm_deep ;;
    dg3) dg3_sglang ;;
    dg4) dg4_mixed ;;
    dg5) dg5_refusal ;;
    dgx) dgx_pin_restart_only ;;
    dg6) dg6_network; stop_role; dg6_migration ;;
    dg7) dg7_remote_host ;;
    *) die "unknown row $1" ;;
  esac
}

[ $# -ge 1 ] || die "usage: $0 prepare|dg1|...|dg7|all"
mkdir -m 700 -p "$STATE"
if [ "$1" = prepare ]; then prepare; exit 0; fi
start_role "role-$1" --listen "127.0.0.1:$PORT"
if [ "$1" = all ]; then
  for row in dg1 dg2 dg3 dg4 dg5 dg6; do run "$row"; [ -n "$ROLE_PID" ] || start_role "role-$row-restart" --listen "127.0.0.1:$PORT"; done
else
  run "$1"
fi
