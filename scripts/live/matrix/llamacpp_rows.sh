# shellcheck shell=bash
# Shared helpers of the llama.cpp rows LC1-LC6 (rows/LC*.sh), sourced by them
# after rowlib.sh. Each row stops the host's matrix role, runs a standalone role
# with its own state directory on the host (as ENG2 does), and restarts the
# matrix role afterwards.
#
# Prerequisites on the host (scripts/live/llamacpp-build.sh source model):
#   ~/llamacpp-v0.6.0/build/bin/llama-server   a CUDA build of tag v0.6.0
#   <models root>/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf
# and this tree synced to the host (sync.sh), because the rows run
# llamacpp_probe.py from it.

LC_TAG=${LLAMACPP_TAG:-v0.6.0}
LC_VERSION=${LC_TAG#v}
LC_COMMIT=d812350
LC_BIN=${LLAMACPP_BIN:-$REMOTE_HOME/llamacpp-$LC_TAG/build/bin/llama-server}
LC_MODEL=Qwen3-4B-GGUF
LC_GGUF=$MODELS_ROOT/$LC_MODEL/Qwen3-4B-Q4_K_M.gguf
LC_GGUF_BYTES=2497280256
LC_PROBE=$REMOTE_TREE/scripts/live/matrix/llamacpp_probe.py
# The row's standalone state directory (RRD is set by load_run before rows load).
LC_DIR=$RRD/lc-standalone
LC_HOST=

# lc_pre <host> [vllm]: what the row needs is there, and nothing that would make
# CapyCTL refuse llama.cpp is. Fails early with the missing piece named.
lc_pre() {
  local host=$1 want_vllm=${2:-}
  rsh "$host" "rc=0
line=\$(timeout 60 $LC_BIN --version 2>&1 >/dev/null | grep -m1 '^version: ' || true)
echo \"version_line: \${line:-none}\"
case \$line in 'version: $LC_VERSION ('*'commit $LC_COMMIT'*) ;; *) echo 'PRECONDITION: no llama-server $LC_VERSION ($LC_COMMIT) at $LC_BIN: run scripts/live/llamacpp-build.sh source' >&2; rc=1 ;; esac
(cd \$(dirname $LC_BIN) && find . -maxdepth 1 -type f \\( -name llama-server -o -name 'lib*.so*' \\) -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum)
[ \"\$(stat -c %s $LC_GGUF 2>/dev/null)\" = $LC_GGUF_BYTES ] || { echo 'PRECONDITION: $LC_GGUF missing or not $LC_GGUF_BYTES bytes: run scripts/live/llamacpp-build.sh model' >&2; rc=1; }
[ \"\$(ls $MODELS_ROOT/$LC_MODEL | wc -l)\" = 1 ] || { echo 'PRECONDITION: $MODELS_ROOT/$LC_MODEL must hold one GGUF only' >&2; rc=1; }
[ ! -e /etc/llama.cpp/config.ini ] || { echo 'PRECONDITION: /etc/llama.cpp/config.ini exists' >&2; rc=1; }
[ ! -e \$HOME/.config/capyctl/engines.yaml ] || { echo 'PRECONDITION: ~/.config/capyctl/engines.yaml exists (the rows write and remove it)' >&2; rc=1; }
[ -f $LC_PROBE ] || { echo 'PRECONDITION: $LC_PROBE missing: sync this tree to the host (sync.sh)' >&2; rc=1; }
if [ -n '$want_vllm' ]; then [ -x $(vllm_venv "$host")/bin/vllm ] || { echo 'PRECONDITION: no vLLM environment at $(vllm_venv "$host")' >&2; rc=1; }; fi
nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
grep -E 'MemTotal|MemAvailable|^Cached' /proc/meminfo
exit \$rc"
}

# lc_start <host> [env assignments...] -- [start standalone flags...]: a
# standalone role on the host with its own state directory and private engine
# logs; CAPYCTL_LLAMACPP_BIN is set unless LC_NO_ENV_BIN=1 (LC1 registers it with
# `engine add` instead).
lc_start() {
  local host=$1 envs=() flags=()
  shift
  while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
  [ "${1:-}" = -- ] && shift
  flags=("$@")
  [ "${LC_NO_ENV_BIN:-0}" = 1 ] || envs+=("CAPYCTL_LLAMACPP_BIN=$LC_BIN")
  rsh "$host" "rm -rf $LC_DIR && mkdir -m 700 $LC_DIR && $MX_TMUX new-session -d -s mx-lc-$RUN \
env CAPYCTL_STATE_DIR=$LC_DIR CAPYCTL_MODELS_ROOT=$MODELS_ROOT ${envs[*]} \
$REMOTE_TREE/scripts/live/matrix/role_exec.sh $LC_DIR/role.pid $LC_DIR/role.log \
$(rbin "$host") start standalone --debug-engine-logs ${flags[*]}"
  rsh "$host" "for i in \$(seq 1 120); do [ -S $LC_DIR/control.sock ] && exit 0; sleep 1; done; tail -n 30 $LC_DIR/role.log; exit 1"
}

lc_stop() { # lc_stop <host>
  rsh "$1" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile $LC_DIR/role.pid --expect 'start standalone' --signal TERM; \
sleep 10; rm -f \$HOME/.config/capyctl/engines.yaml \$HOME/.config/capyctl/engines.yaml.lock; rm -rf $LC_DIR"
}

# lc <host> <capyctl args...>: the CLI against the row's standalone role.
lc() { local host=$1; shift; rsh "$host" "CAPYCTL_STATE_DIR=$LC_DIR timeout 1800 $(rbin "$host") $*"; }

# lc_doc <name> <engine profile> [extra_args...]: a llama.cpp deployment of the
# laptop rows' shape (8192 tokens, 4 slots, f16 KV, every layer on the GPU, no
# resources: CapyCTL derives the request), copied to the host.
lc_doc() {
  local name=$1 profile=$2 file
  shift 2
  file=$EVID/$name.yaml
  {
    # `engine` is the short form of `runtime_profile`: the profile's name.
    printf 'schema_version: 1\nkind: deployment\nname: %s\nengine: %s\n' "$name" "$profile"
    printf 'model: %s\ndevices: [{id: gpu0, sharing: shared}]\nengine_config:\n' "$LC_MODEL"
    printf '  context_length: 8192\n  max_concurrent_requests: 4\n  kv_cache_dtype: f16\n'
    printf '  llamacpp:\n    n_gpu_layers: all\n'
    if [ $# -gt 0 ]; then
      printf '  accept_extra_args: true\n  extra_args: [%s]\n' "$(IFS=,; printf '%s' "$*")"
    fi
  } >"$file"
  cat "$file"
  rcopy "$file" "$LC_HOST:$LC_DIR/$name.yaml"
}

# lc_deploy <host> <name>: deploy the copied document, activate and wait for Ready.
lc_deploy() { lc "$1" deploy model --file "$LC_DIR/$2.yaml" --activate --wait --format json; }

lc_status() { lc "$1" status deployment "$2" --format json; }
lc_delete() { lc "$1" delete deployment "$2" --stop --format json; }

# lc_probe <host> <subcommand> [args...]: llamacpp_probe.py on the host.
lc_probe() {
  local host=$1 sub=$2
  shift 2
  case $sub in
    chat|hangup|streams|load) rsh "$host" "python3 $LC_PROBE $sub --dir $LC_DIR $*" ;;
    *) rsh "$host" "python3 $LC_PROBE $sub $*" ;;
  esac
}

# lc_sampler_start <host> <label>: memory samples every 0.5 s until stopped.
lc_sampler_start() {
  rsh "$1" "nohup python3 $LC_PROBE sample --gguf $LC_GGUF --label $2 --out $LC_DIR/mem-$2.jsonl >/dev/null 2>&1 & echo \$! >$LC_DIR/sampler-$2.pid"
}

# lc_sampler_stop <host> <label>: stop the sampler, copy its samples into the
# evidence and print the peak, steady and after-exit figures.
lc_sampler_stop() {
  local host=$1 label=$2
  rsh "$host" "kill \$(cat $LC_DIR/sampler-$label.pid) 2>/dev/null; sleep 1; python3 $LC_PROBE summary --samples $LC_DIR/mem-$label.jsonl"
  x scp -q "${SSH_OPTS[@]}" "$host:$LC_DIR/mem-$label.jsonl" "$EVID/mem-$label.jsonl"
}

# lc_mark <host> <label>: one memory sample line into the evidence (between phases).
lc_mark() {
  rsh "$1" "echo '## $2'; grep -E 'MemAvailable|^Cached|AnonPages|Mapped' /proc/meminfo; \
nvidia-smi --query-compute-apps=pid,process_name,used_memory --format=csv,noheader; \
for p in \$(pgrep -x llama-server); do grep -E 'VmRSS|VmHWM|RssAnon|RssFile' /proc/\$p/status; done; true"
}

# lc_engine_lines <host>: llama-server's own memory and start lines from the
# private engine logs (KV buffer, model buffers, compute buffers, load mode).
lc_engine_lines() {
  rsh "$1" "grep -rhaE 'KV buffer size|model buffer size|compute buffer size|CUDA0|load_tensors|mmap|llama_model_load|n_slots|n_ctx_slot|server is listening|model loaded|ggml_cuda_init|Device 0|unified' \
$LC_DIR/logs 2>/dev/null | grep -viE 'key|token=|secret|bearer|password' | tail -n 60; true"
}

# lc_no_engine <host>: no llama-server, no GPU compute process left.
lc_no_engine() {
  rsh "$1" "pgrep -ax llama-server && echo LEFTOVER_LLAMA; nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader | grep . && echo LEFTOVER_GPU; true" |
    tee "$EVID/no-engine-$STEP.txt"
  dry && return 0
  ! grep -q LEFTOVER "$EVID/no-engine-$STEP.txt"
}

# lc_begin <host> [vllm]: the common opening: preconditions, an idle host, the
# matrix role down.
lc_begin() {
  local host=$1
  LC_HOST=$host
  step precondition lc_pre "$@" || return 1
  step before host_idle "$host" || return 1
  step matrix-host-down "$MATRIX_DIR/roles.sh" host-down "$host" || return 1
}

# lc_end <host>: the common closing: standalone stopped, the host idle, the matrix
# role back and online.
lc_end() {
  local host=$1 rc=0
  step stop-standalone lc_stop "$host" || rc=1
  step idle host_idle "$host" || rc=1
  step matrix-host-up "$MATRIX_DIR/roles.sh" host-up "$host" || rc=1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  return "$rc"
}
