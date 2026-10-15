#!/usr/bin/env bash
# Host side of run.sh: the 0.1.3 release live checks (B1-B17 of the 0.1.3 live
# backlog) on one GB10 host, through CapyCTL standalone roles that each have
# their own state, config and model-sources directories, ports and kernel caches.
#
#   remote.sh estimate  print the estimated minutes per check (runs anywhere)
#   remote.sh run       every selected check, in $WORK, from the settings in $WORK/rc.env
#   remote.sh plan      print what `run` does, step by step, running nothing (any machine)
#   remote.sh down      delete what `run` created and stop its role (also run on exit)
#   remote.sh purge     remove $WORK (after run.sh copied the results back)
#
# An existing CapyCTL install on the host is not touched; only a role, HTTP
# server or container this script started is ever signalled. Engine keys are
# never read; the role's inference key and admin token are read from its
# credentials file by rc.py only, never printed. The role's ledger is read only
# through read-only SELECTs (rc.py sql, mode=ro).
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
cmd=${1:-}

# Estimated minutes per check on one GB10, from earlier runs' start and wake
# times. A deployment shared by several checks is counted under the first.
estimates() {
  cat <<'EOF'
B1	40	vLLM 0.30.0 model 1: cold start with empty kernel caches ~5, three deep park/wake cycles ~4 each on the default loader; lazy variant (informational) start ~5 and up to three cycles (a hung wake gives up after 10 minutes); carries B2 and the vLLM parts of B3, B5
B2	0	inside B1's three cycles
B3	14	vLLM requests ~1; TensorFold 0.6.6 model 1 with the DFlash2 drafter start and requests ~7; SGLang 0.5.21 4B cold start with empty kernel caches and requests ~6
B4	16	own role (parked_growth_limit 0B): vLLM eager start ~4; up to 6 park/wake cycles ~2 each until the growth stop (SKIP when the charge never grows)
B5	2	vLLM and SGLang: one marked request each, log scan, management tail
B6	2	SGLang 4B: two requests in flight, load read polled, role log scanned
B7	4	own role (managed 110GiB, reserve 11GiB): one refused start, no engine runs
B8	6	SGLang 4B explicit: start, park, 409, start (wake), answer
B9	22	SGLang 27B: cold start with kernel builds ~12 (--max-mamba-cache-size 8, KV 1536MiB), second start ~5 (KV 1GiB)
B10	150	Flash-Next: ~135 GB download ~40, SGLang first start with kernel builds up to 30, TensorFold MLX download and start ~40, ~0 when no complete copy (SKIP)
B11	6	Qwen2.5-0.5B: 1 GB download into the run's sources directory, cold vLLM start
B12	12	SGLang 27B with model.draft (DFlash2 drafter download ~3 GB) and DFLASH: start ~8
B13	12	own role (plain HTTP approved for 127.0.0.1:<http port>): two tar payloads, three loopback fetches, two vLLM starts
B14	6	vLLM Qwen3-4B (dense, restart_only) start and three salted requests; SGLang: three salted requests; TensorFold: one refused request
B15	2	SGLang 27B (B9's first deployment): load read during a request against the engine log
B16	0	SKIP: no gpt-oss-120b on the host
B17	15	container: image build, role as PID 1, vLLM 4B start and stop
EOF
}
if [ "$cmd" = estimate ]; then estimates; exit 0; fi

WORK=${WORK:-$HERE}
# shellcheck disable=SC1091
. "$WORK/rc.env"
: "${PORT:?rc.env sets PORT}" "${MPORT:?}" "${ENGINE_PORTS:?}" "${HTTP_PORT:?}" "${CPORT:?}" "${C_ENGINE_PORTS:?}"
: "${ONLY:?}" "${DOWNLOAD_FLASH_NEXT:?}" "${CONTAINER:?}" "${DRAFT_SOURCE:?}" "${MAX_PARKS:?}" "${RID:?}" "${CAPYCTL_COMMIT:?}"
PLAN=0
[ "$cmd" = plan ] && PLAN=1
RES=$WORK/res
CAPYCTL=${CAPYCTL_BIN:-capyctl}
export XDG_CONFIG_HOME=$WORK/cfg
HELPER=(python3 "$WORK/rc.py")
EP=http://127.0.0.1:$PORT
MODELS=$HOME/models
V30=$HOME/vllm-0.30.0-venv
S21=$HOME/sglang-0.5.21-venv
TF66=$HOME/tensorfold-0.6.6-venv
M1=$MODELS/qwen3.8-27b-nvfp4
Q4B=$MODELS/qwen3-4b-instruct
DRAFTER=$HOME/drafters/Qwen3.8-27B-DFlash2
DRAFT_HF="z-lab/Qwen3.8-27B-DFlash2@50307d4c4cde6860d4eee73e2547cd786fe8e8a4"
Q05_HF=Qwen/Qwen2.5-0.5B-Instruct@7ae557604adf67be50417f59c2c2f167def9a775
Q05_FILES="config.json generation_config.json merges.txt model.safetensors tokenizer.json tokenizer_config.json vocab.json"
FN_LOCAL=$MODELS/Qwen3.8-Flash-Next-hibrid48
FN_SG_HF=RadixArk/Qwen3.8-Flash-Next-NVFP4@7b719225242aacd3dbd3f9407468c2ee9a9d2594
FN_SG_STORE=$MODELS/sources/huggingface/RadixArk--Qwen3.8-Flash-Next-NVFP4@7b719225242aacd3dbd3f9407468c2ee9a9d2594
FN_TF_HF=TensorFold/Qwen3.8-Flash-Next-MLX-4bit-MTP@2b170fa6309d5d1ee380b35636075fac7945f286
FN_TF_STORE=$MODELS/sources/huggingface/TensorFold--Qwen3.8-Flash-Next-MLX-4bit-MTP@2b170fa6309d5d1ee380b35636075fac7945f286
SYS=host.resource_policy.memory.system
PIDFILE=$WORK/role.pid
CONTAINER_NAME=capyctl-rc-$RID
IMAGE=capyctl-rc:$RID
# Any engine on the host (the lab rule: one live session at a time), and this
# run's own: argv names one of its venvs, or the engines' renamed processes.
ANY_ENGINE='sglang[.]launch_server|sglang_entr[y]|vllm_entr[y]|vllm[ ]serve|Engine[C]ore|sglang::schedule[r]|tensorfol[d]'
ENGINE_RE="$V30/bin/|$S21/bin/|$TF66/bin/|$ANY_ENGINE"

# The current role: set by role_up, cleared by role_down.
ROLE="" SD="" CRED="" MADDR="" DB="" ROLE_LOG="" PROFILES="" DEPLOYS="" HTTP_PID=""

# ---------------------------------------------------------------------------
# Effects. In plan mode every effect is printed instead (fd 3 is the terminal,
# so a step whose output is redirected still shows), waits are skipped,
# evaluations print their pass criteria and measured values read as placeholders.
# ---------------------------------------------------------------------------
exec 3>&1
mkdir -p "$RES" "$WORK/dep"
show() { printf '    %s\n' "$*" | sed -e "s#$HOME#~#g" >&3; }
say() {
  if [ "$PLAN" = 1 ]; then printf '\n  %s\n' "$*" | sed -e "s#$HOME#~#g" >&3; return 0; fi
  printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RES/progress.log"
}
phase() {
  if [ "$PLAN" = 1 ]; then printf '\n== %s\n' "$*" | sed -e "s#$HOME#~#g" >&3; return 0; fi
  say "== $*"
}
run() { if [ "$PLAN" = 1 ]; then show "\$ $(printf '%q ' "$@")"; return 0; fi; "$@"; }
c() { run "$CAPYCTL" --state-dir "$SD" "$@"; }
helper() { run "${HELPER[@]}" "$@"; }
# Local reads of files this run wrote (status, logs, the ledger): nothing to print in a plan.
ev() { [ "$PLAN" = 1 ] && return 0; "${HELPER[@]}" "$@"; }
check() { # id scope criteria: printed in a plan, logged in a run
  if [ "$PLAN" = 1 ]; then printf '    => %s (%s) PASS when: %s\n' "$1" "$2" "$3" | sed -e "s#$HOME#~#g" >&3
  else say "checking $1 ($2)"; fi
}
result() { # id scope pass|fail|skip detail
  [ "$PLAN" = 1 ] && return 0
  printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >>"$RES/results.tsv"
  say "[$3] $1 ($2): $4"
}
resr() { local r=${3:-}; [ -n "$r" ] || r=$'fail\tno result'; result "$1" "$2" "${r%%$'\t'*}" "${r#*$'\t'}"; }
fail_ids() { # scope detail id...
  local scope=$1 detail=$2 id; shift 2
  for id in "$@"; do want "$id" && result "$id" "$scope" fail "$detail"; done
  return 0
}
skip() { # id scope reason
  if [ "$PLAN" = 1 ]; then printf '    => %s (%s) SKIP: %s\n' "$1" "$2" "$3" | sed -e "s#$HOME#~#g" >&3; return 0; fi
  result "$1" "$2" skip "$3"
}
want() { local id; [ "$ONLY" = all ] && return 0; for id in "$@"; do case ",$ONLY," in *",$id,"*) return 0 ;; esac; done; return 1; }
now() { date +%s.%N; }
elapsed() { if [ "$PLAN" = 1 ]; then echo 0; else printf '%.1f' "$(echo "$(now) - $1" | bc)"; fi; }
memavail() { if [ "$PLAN" = 1 ]; then echo 0; else awk '/^MemAvailable:/ {printf "%.2f", $2/1048576}' /proc/meminfo; fi; }
sub() { echo "${1:-0} - ${2:-0}" | bc; }
gib() { awk -v b="${1:-0}" 'BEGIN {printf "%.1f", b / 1073741824}'; }
pause() { [ "$PLAN" = 1 ] || sleep "$1"; }
tag() { od -An -N5 -tx1 /dev/urandom | tr -d ' \n'; }
waitgone() {
  [ "$PLAN" = 1 ] && { show "wait until no engine process of this run is left (600 s)"; return 0; }
  local _
  for _ in $(seq 600); do pgrep -f -- "$ENGINE_RE" >/dev/null || return 0; sleep 1; done
  return 1
}
# How many engine processes of a venv carry FLAG on their command line. Only the
# count is kept: a command line is never printed or written.
argv_count() { # venv flag
  [ "$PLAN" = 1 ] && { show "count the processes from ${1/#$HOME/~}/bin/ whose command line holds '$2' (the command line itself is never printed)"; echo 1; return 0; }
  pgrep -af -- "$1/bin/" | grep -cF -- "$2"
}
engine_pid() { [ "$PLAN" = 1 ] && { echo 0; return 0; }; pgrep -o -f -- "$1/bin/" || echo none; }

# ---------------------------------------------------------------------------
# Roles, profiles, deployments
# ---------------------------------------------------------------------------

role_vars() { # tag: the role's state directory and the files in it
  ROLE=$1 SD=$WORK/$1
  CRED=$SD/identity/credentials MADDR=$SD/run/management-address DB=$SD/server/srv.sqlite3
  ROLE_LOG=$RES/role-$1.log
}

role_up() { # tag, then `start standalone` options
  local tag=$1 _
  shift
  role_vars "$tag"
  PROFILES="" DEPLOYS=""
  [ "$PLAN" = 1 ] || echo "$tag" >"$WORK/role.tag"
  say "role $tag: standalone, own state directory $SD, 127.0.0.1:$PORT, management 127.0.0.1:$MPORT, engine ports $ENGINE_PORTS"
  run mkdir -m 700 -p "$SD"
  if [ "$PLAN" = 1 ]; then
    show "\$ setsid nohup capyctl --state-dir $SD start standalone --listen 127.0.0.1:$PORT --management-listen 127.0.0.1:$MPORT --engine-ports $ENGINE_PORTS $* --json >res/role-$tag.log 2>&1 &"
    show "wait for '\"ready\":true' in res/role-$tag.log (180 s)"
    return 0
  fi
  setsid nohup "$CAPYCTL" --state-dir "$SD" start standalone --listen "127.0.0.1:$PORT" \
    --management-listen "127.0.0.1:$MPORT" --engine-ports "$ENGINE_PORTS" "$@" --json \
    >"$ROLE_LOG" 2>&1 </dev/null &
  echo $! >"$PIDFILE"
  for _ in $(seq 180); do grep -q '"ready":true' "$ROLE_LOG" 2>/dev/null && return 0; sleep 1; done
  say "role $tag: no ready line in 180 s"
  return 1
}

role_down() {
  [ -n "$ROLE" ] || return 0
  local d p pid _
  say "role $ROLE down: delete its deployments (--stop), remove its profiles, TERM its own PID, keep its engine logs"
  for d in $DEPLOYS; do c delete deployment "$d" --stop >>"$RES/cli.log" 2>&1; done
  waitgone
  for p in $PROFILES; do c engine remove "$p" >>"$RES/cli.log" 2>&1; done
  if [ "$PLAN" = 1 ]; then
    show "kill -TERM <role pid from role.pid, checked against its argv>; wait 60 s; record the drained line"
    show "copy $SD/logs (redacted engine logs) to res/engine-logs-$ROLE"
  else
    if [ -f "$PIDFILE" ] && pid=$(cat "$PIDFILE") && [ -r "/proc/$pid/cmdline" ] &&
       tr '\0' ' ' <"/proc/$pid/cmdline" | grep -qF -- "$SD"; then
      # A signal is the role's drained shutdown: admission closes, in-flight requests finish.
      kill -TERM "$pid"
      for _ in $(seq 60); do [ -d "/proc/$pid" ] || break; sleep 1; done
      grep -o '"drained":[a-z]*[^}]*' "$ROLE_LOG" | tail -1 >"$RES/drained-$ROLE.txt"
    fi
    rm -f "$PIDFILE" "$WORK/role.tag"
    [ -d "$SD/logs" ] && cp -r "$SD/logs" "$RES/engine-logs-$ROLE" 2>/dev/null
    if pgrep -f -- "$ENGINE_RE" >/dev/null; then say "warning: an engine process is still running after role $ROLE"; fi
  fi
  ROLE=""
}

cache_env() { # engine dir -> CACHE_ARGS: the profile's own kernel caches, empty at first
  local d=$2
  case $1 in
    vllm) CACHE_ARGS=(--env "VLLM_CACHE_ROOT=$d/vllm" --env "TRITON_CACHE_DIR=$d/triton"
      --env "TORCHINDUCTOR_CACHE_DIR=$d/inductor" --env "FLASHINFER_WORKSPACE_BASE=$d"
      --env "CUDA_CACHE_PATH=$d/nv" --env "TVM_FFI_CACHE_DIR=$d/tvm-ffi") ;;
    sglang) CACHE_ARGS=(--env "SGLANG_CACHE_DIR=$d" --env "SGLANG_JIT_CACHE_DIR=$d/jit"
      --env "SGLANG_DG_CACHE_DIR=$d/deep_gemm" --env "FLASHINFER_WORKSPACE_BASE=$d"
      --env "TRITON_CACHE_DIR=$d/triton" --env "TORCHINDUCTOR_CACHE_DIR=$d/inductor"
      --env "CUDA_CACHE_PATH=$d/nv" --env "TVM_FFI_CACHE_DIR=$d/tvm-ffi") ;;
    tensorfold) CACHE_ARGS=(--env "TRITON_CACHE_DIR=$d/triton" --env "CUDA_CACHE_PATH=$d/nv") ;;
  esac
}

profile() { # name engine venv cache-key [engine add options]: once per role
  local n=$1 e=$2 v=$3 k=$4
  shift 4
  case " $PROFILES " in *" $n "*) return 0 ;; esac
  cache_env "$e" "$WORK/jit/$k"
  run mkdir -p "$WORK/jit/$k"
  if c engine add "$v" --name "$n" "${CACHE_ARGS[@]}" "$@" >>"$RES/cli.log" 2>&1; then
    PROFILES="$PROFILES $n"
    return 0
  fi
  say "engine add $n failed; see cli.log"
  return 1
}

yaml() { # name: stdin -> $WORK/dep/<name>.yaml, shown in a plan
  cat >"$WORK/dep/$1.yaml"
  if [ "$PLAN" = 1 ]; then
    show "deployment file dep/$1.yaml:"
    sed -e 's/^/        /' -e "s#$HOME#~#g" "$WORK/dep/$1.yaml" >&3
  fi
}

deploy() { # name
  if c deploy model --file "$WORK/dep/$1.yaml" >>"$RES/cli.log" 2>&1; then
    DEPLOYS="$DEPLOYS $1"
    return 0
  fi
  say "deploy $1 refused; see cli.log"
  [ "$PLAN" = 1 ] || tail -5 "$RES/cli.log" | sed 's/^/    /' | tee -a "$RES/progress.log"
  return 1
}

START_S="" READY_GIB="" BEFORE_GIB=""
startd() { # name label [start options]
  local n=$1 l=$2 s rc
  shift 2
  BEFORE_GIB=$(memavail)
  s=$(now)
  c start deployment "$n" --wait "$@" >>"$RES/cli.log" 2>&1
  rc=$?
  if [ "$rc" = 0 ]; then
    START_S=$(elapsed "$s")
    pause 10
    READY_GIB=$(sub "$BEFORE_GIB" "$(memavail)")
    [ "$PLAN" = 1 ] || printf '%s\t%s\t%s\t%s\n' "$n" "$l" "$START_S" "$READY_GIB" >>"$RES/startup.tsv"
    [ "$PLAN" = 1 ] || say "started $n ($l) in $START_S s; MemAvailable down $READY_GIB GiB"
    return 0
  fi
  say "start $n ($l) failed (exit $rc); see cli.log"
  c status deployment "$n" >>"$RES/cli.log" 2>&1
  return 1
}
stopd() { c stop deployment "$1" >>"$RES/cli.log" 2>&1; waitgone; pause 5; }
status_json() { c status deployment "$1" --json >"$2" 2>>"$RES/cli.log"; }
state_of() { # name [plan value]
  [ "$PLAN" = 1 ] && { echo "${2:-ready}"; return 0; }
  c status deployment "$1" --json >"$RES/state.json" 2>/dev/null
  ev get "$RES/state.json" observed_state
}
poll_state() { # name timeout state...: prints the state reached
  local n=$1 t=$2 st="" _
  shift 2
  if [ "$PLAN" = 1 ]; then show "poll status until observed_state is one of: $* (${t} s)"; echo "$1"; return 0; fi
  for _ in $(seq "$t"); do
    st=$(state_of "$n")
    case " $* " in *" $st "*) echo "$st"; return 0 ;; esac
    sleep 1
  done
  echo "${st:-unknown}"
  return 1
}
dep_id() { [ "$PLAN" = 1 ] && { echo '<deployment-id>'; return 0; }; status_json "$1" "$RES/id.json"; ev get "$RES/id.json" id; }
launch_logs() { # deployment id -> its engine log files, newest last
  [ "$PLAN" = 1 ] && { echo "$SD/logs/<deployment-id>/<incarnation>.log"; return 0; }
  find "$SD/logs/$1" -maxdepth 1 -type f -printf '%T@ %p\n' 2>/dev/null | sort -n | cut -d' ' -f2-
}
poll_sources() { # name timeout: every model source of the deployment verified
  local n=$1 t=$2 st _ i=0
  if [ "$PLAN" = 1 ]; then show "poll status until every model_sources[].state is verified (${t} s; progress every 5 min)"; echo verified; return 0; fi
  for _ in $(seq $((t / 5))); do
    status_json "$n" "$RES/sources-$n.json"
    st=$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); s=[x.get("state") for x in d.get("model_sources") or []]
print("failed" if "failed" in s else "verified" if s and all(x=="verified" for x in s) else "none" if not s else "pending")' \
      "$RES/sources-$n.json" 2>/dev/null)
    case $st in verified|failed) echo "$st"; return 0 ;; none) [ $i -ge 6 ] && { echo none; return 0; } ;; esac
    i=$((i + 1))
    [ $((i % 60)) = 0 ] && say "$n: sources $(python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); print(", ".join("%s %s %.1f/%.1f GiB"%(x.get("source_key","?")[:60],x.get("state"),x.get("bytes_done",0)/2**30,x.get("bytes_total",0)/2**30) for x in d.get("model_sources") or []))' "$RES/sources-$n.json" 2>/dev/null)"
    sleep 5
  done
  echo timeout
}

# ---------------------------------------------------------------------------
# Deployment files
# ---------------------------------------------------------------------------

dep_vllm_m1() { # name loader: model 1 on vLLM 0.30.0, the catalog recipe less the drafter
  local loader=""
  [ -n "$2" ] && loader="  vllm: {safetensors_load_strategy: $2}"
  yaml "$1" <<EOF
schema_version: 1
kind: deployment
name: $1
engine: v30
model: $M1
timeouts: {initialize: 1800s}
engine_config:
  kv_cache_dtype: fp8
  language_model_only: true
  context_length: 32768
  memory: {request: 48GiB}
${loader:-  # no safetensors_load_strategy: CapyCTL picks it by memory shape}
  # Cached-token counts in usage (an ordinary extra flag), for the cache_salt check.
  accept_extra_args: true
  extra_args: [--enable-prompt-tokens-details]
EOF
}

phases_yaml() { # cold ready: the five phases of a declared reservation on unified memory
  local p b
  printf 'devices: [{id: gpu0}]\nresources:\n'
  for p in cold:$1 ready:$2 parking:$2 parked:0B wake:$1; do
    b=${p#*:}
    printf '  %s:\n    allocations: [{domain: unified, bytes: %s, host_kv_bytes: 0B}]\n' "${p%%:*}" "$b"
    if [ "${p%%:*}" = parked ]; then printf '    devices: []\n'; else printf '    devices: [{id: gpu0}]\n'; fi
  done
}

dep_tf_m1() { # model 1 on TensorFold 0.6.6 with the DFlash2 drafter (TensorFold needs one for this model)
  # The quick-check reservation for Qwen3.8-27B with its drafter (scripts/live/quick-check/remote.sh);
  # --drafter and ~/drafters are approved by the tf66 profile (SPEC §7 extra-argument approval).
  yaml t-m1 <<EOF
schema_version: 1
kind: deployment
name: t-m1
engine: tf66
model: $M1
residency: restart_only
$(phases_yaml 60GiB 58GiB)
engine_config:
  context_length: 32768
  accept_extra_args: true
  extra_args: [--drafter, $DRAFTER]
EOF
}

dep_sglang_4b() { # name explicit(0|1)
  local act=""
  [ "$2" = 1 ] && act="lifecycle: {activation: explicit}"
  yaml "$1" <<EOF
schema_version: 1
kind: deployment
name: $1
engine: s21
model: $Q4B
timeouts: {initialize: 1800s}
${act:-# activation on demand (the default)}
engine_config:
  context_length: 16384
  max_concurrent_requests: 2
EOF
}

dep_sglang_m1() { # name kv extra-args-yaml
  # modelopt on SGLang has no deep park in this build (capability_missing:deep_park,
  # by design): the deployment declares restart_only.
  yaml "$1" <<EOF
schema_version: 1
kind: deployment
name: $1
engine: s21
model: $M1
residency: restart_only
timeouts: {initialize: 1800s}
engine_config:
  quantization: modelopt
  kv_cache_dtype: fp8_e4m3
  context_length: 32768
  memory: {kv_cache: $2}
${3:-  # no extra arguments}
EOF
}

dep_sglang_draft() { # [memory request]
  local draft="$DRAFTER"
  [ "$DRAFT_SOURCE" = hf ] && draft="{hf: $DRAFT_HF}"
  yaml s-m1-draft <<EOF
schema_version: 1
kind: deployment
name: s-m1-draft
engine: s21
# The drafter is the deployment's second model source (model.draft): CapyCTL
# materializes it, waits for it and renders its path; no approval, no path argument.
model:
  path: $M1
  draft: $draft
residency: restart_only
timeouts: {initialize: 1800s}
engine_config:
  quantization: modelopt
  kv_cache_dtype: fp8_e4m3
  context_length: 32768
  max_concurrent_requests: 8
  # The size CapyCTL named when it refused 65GiB (the recurrent state of 8 requests
  # and 72 draft-token states, beside weights and the KV cache).
  memory: {request: ${1:-67486MiB}, kv_cache: 16GiB}
  accept_extra_args: true
  extra_args: [--speculative-algorithm, DFLASH, --speculative-num-draft-tokens, "8", --speculative-draft-model-quantization, unquant]
EOF
}

dep_vllm_small() { # name model-yaml [profile] [extra engine_config yaml]
  yaml "$1" <<EOF
schema_version: 1
kind: deployment
name: $1
engine: ${3:-v30}
model: $2
timeouts: {initialize: 1800s}
engine_config:
  context_length: 8192
${4:-  # nothing more}
EOF
}

# ---------------------------------------------------------------------------
# Shared checks
# ---------------------------------------------------------------------------

metrics_check() { # scope engine deployment
  local scope=$1 eng=$2 n=$3 r
  check B3 "$scope" "the answer carries capyctl.metrics (vLLM: ttft_ms and prefill_ms source engine; SGLang: ttft_ms source router; TensorFold CUDA: prefill_ms source engine, ttft_ms router); a stream with stream_options.include_usage has a usage chunk and ': x-capyctl-metrics {...}' before 'data: [DONE]' with the same engine figures"
  helper chat --cred "$CRED" --endpoint "$EP" --model "$n" --out "$RES/b3-$eng-chat.json" >>"$RES/cli.log" 2>&1
  helper stream --cred "$CRED" --endpoint "$EP" --model "$n" --out "$RES/b3-$eng-stream.json" >>"$RES/cli.log" 2>&1
  r=$(ev metrics --engine "$eng" --chat "$RES/b3-$eng-chat.json" --stream "$RES/b3-$eng-stream.json")
  [ "$PLAN" = 1 ] || resr B3 "$scope" "$r"
}

salt_check() { # scope deployment expect(salt|refused)
  local r
  if [ "$3" = refused ]; then
    check B14 "$1" "a request with cache_salt answers 400 with code cache_salt_unsupported (flat error object), before any engine sees it"
  else
    check B14 "$1" "cache_salt accepted (200); the same long prompt twice under one salt reports usage.prompt_tokens_details.cached_tokens > 0 on the repeat (capyctl.metrics.cached_tokens source engine); under a second salt it reports 0"
  fi
  helper cached --cred "$CRED" --endpoint "$EP" --model "$2" --salt "rc-$RID-a" --salt2 "rc-$RID-b" \
    --out "$RES/b14-$1.json" >>"$RES/cli.log" 2>&1
  r=$(ev cache-eval "$RES/b14-$1.json" --expect "$3")
  [ "$PLAN" = 1 ] || resr B14 "$1" "$r"
}

redaction_check() { # scope deployment id [--sglang]
  local scope=$1 n=$2 id=$3 flag=${4:-} marker r logs
  marker="rcmark-$RID-$(tag)"
  check B5 "$scope log" "after a request whose prompt holds a unique marker: the launch's engine log holds neither the marker nor the role's key or token, no credential-shaped value (api_key/token/secret assignments, hf_ tokens, bearer values) unredacted$([ -n "$flag" ] && echo "; SGLang's info-level server_args line is present with api_key and admin_api_key '<redacted>' or None (warning lines counted)")"
  helper chat --cred "$CRED" --endpoint "$EP" --model "$n" --prompt "Keep this tag in mind: $marker. What is 17+25?" \
    --max-tokens 256 --out "$RES/b5-$scope-chat.json" >>"$RES/cli.log" 2>&1
  pause 3
  mapfile -t logs < <(launch_logs "$id")
  # shellcheck disable=SC2086 # flag is empty or one word
  r=$(ev redaction --cred "$CRED" --marker "$marker" $flag "${logs[@]}")
  [ "$PLAN" = 1 ] || resr B5 "$scope log" "$r"
  check B5 "$scope tail" "GET /management/v1/deployments/<id>/engine-log?instance=0&kib=64 with the admin token answers 200 with the log's tail, without the marker or a role credential"
  [ "$PLAN" = 1 ] && show "rc.py tail: GET 127.0.0.1:$MPORT/management/v1/deployments/<id>/engine-log?instance=0&kib=64 (admin token from the credentials file)"
  r=$(ev tail --cred "$CRED" --maddr "$MADDR" --id "$id" --marker "$marker" --out "$RES/b5-$scope-tail.json")
  [ "$PLAN" = 1 ] || resr B5 "$scope tail" "$r"
}

# ---------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------

preflight() {
  phase "preflight: venvs, models, no other engine, free ports"
  if [ "$PLAN" = 1 ]; then
    show "check ~/vllm-0.30.0-venv reports vllm 0.30.0, ~/sglang-0.5.21-venv sglang 0.5.21, ~/tensorfold-0.6.6-venv tensorfold 0.6.6 (each when a selected check needs it)"
    show "check ~/models/qwen3.8-27b-nvfp4 and ~/models/qwen3-4b-instruct hold config.json$(want B3 B14 && echo ", ~/drafters/Qwen3.8-27B-DFlash2 too (TensorFold's drafter)")"
    show "check no engine process runs (pgrep '$ANY_ENGINE') and ports $PORT, $MPORT, $HTTP_PORT, $CPORT, $((CPORT + 1)) are free"
    return 0
  fi
  local got p bad=0
  if want B1 B2 B3 B4 B5 B7 B11 B13 B14 B17; then
    got=$("$V30/bin/python" -c 'import vllm; print(vllm.__version__)' 2>/dev/null)
    [ "$got" = 0.30.0 ] || { say "preflight: vllm-0.30.0-venv reports '$got'"; bad=1; }
  fi
  if want B3 B5 B6 B8 B9 B10 B12 B14 B15; then
    got=$("$S21/bin/python" -c 'import sglang; print(sglang.__version__)' 2>/dev/null)
    [ "$got" = 0.5.21 ] || { say "preflight: sglang-0.5.21-venv reports '$got'"; bad=1; }
  fi
  if want B3 B10 B14; then
    got=$("$TF66/bin/tensorfold" --version 2>/dev/null | awk '{print $NF}')
    [ "$got" = 0.6.6 ] || { say "preflight: tensorfold-0.6.6-venv reports '$got'"; bad=1; }
  fi
  for p in "$M1" "$Q4B"; do [ -f "$p/config.json" ] || { say "preflight: ${p/#$HOME/~} has no config.json"; bad=1; }; done
  if { want B3 B14 || { [ "$DRAFT_SOURCE" = local ] && want B12; }; } && [ ! -f "$DRAFTER/config.json" ]; then say "preflight: no ~/drafters/Qwen3.8-27B-DFlash2 (TensorFold's drafter, and B12's with --draft-source local)"; bad=1; fi
  if pgrep -af -- "$ANY_ENGINE" >/dev/null; then say "preflight: an engine process is running on the host; one live session at a time"; bad=1; fi
  for p in "$PORT" "$MPORT" "$HTTP_PORT" "$CPORT" "$((CPORT + 1))"; do
    if ss -ltnH "sport = :$p" 2>/dev/null | grep -q .; then say "preflight: port $p is in use"; bad=1; fi
  done
  [ "$bad" = 0 ] && say "preflight passed"
  [ "$bad" = 0 ]
}

record_versions() {
  say "record versions"
  [ "$PLAN" = 1 ] && { show "write versions.txt: capyctl, vllm, sglang, tensorfold, torch, driver, CUDA, kernel"; return 0; }
  {
    echo "capyctl_version=$("$CAPYCTL" --version 2>/dev/null | head -1)"
    echo "capyctl_commit=$CAPYCTL_COMMIT"
    echo "vllm=$("$V30/bin/python" -c 'import vllm; print(vllm.__version__)' 2>/dev/null)"
    echo "vllm_torch=$("$V30/bin/python" -c 'import torch; print(torch.__version__)' 2>/dev/null)"
    echo "sglang=$("$S21/bin/python" -c 'import sglang; print(sglang.__version__)' 2>/dev/null)"
    echo "sglang_torch=$("$S21/bin/python" -c 'import torch; print(torch.__version__)' 2>/dev/null)"
    echo "tensorfold=$("$TF66/bin/tensorfold" --version 2>/dev/null | awk '{print $NF}')"
    echo "gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1 | sed 's/^NVIDIA //')"
    echo "driver=$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1)"
    echo "cuda=$(/usr/local/cuda/bin/nvcc --version 2>/dev/null | sed -n 's/.*release \([0-9.]*\).*/\1/p')"
    echo "kernel=$(uname -r)"
    echo "mem_total_gib=$(awk '/^MemTotal:/ {printf "%.1f", $2/1048576}' /proc/meminfo)"
    echo "mem_available_gib=$(memavail)"
    echo "docker=$(docker --version 2>/dev/null)"
  } >"$RES/versions.txt"
}

# B7: admission figures, on a role whose managed limit plus free reserve is
# more than the memory available (docs/changes/admission-figures.md).
step_b7() {
  want B7 || return 0
  phase "B7 admission figures: own role, managed limit 110GiB and free reserve 11GiB (121 GiB, more than is available)"
  role_up s2 --set "$SYS.managed_limit=110GiB" --set "$SYS.free_reserve=11GiB" ||
    { fail_ids "role" "standalone did not become ready" B7; role_down; return; }
  profile v30 vllm "$V30" vllm || { fail_ids "profile" "engine add failed" B7; role_down; return; }
  yaml b7-admit <<EOF
schema_version: 1
kind: deployment
name: b7-admit
engine: v30
model: $Q4B
# A 109 GiB cold charge: inside the 110 GiB managed limit, but what is available
# (about 118 GiB on an idle GB10) less 109 GiB leaves less than the 11 GiB reserve.
residency: restart_only
$(phases_yaml 109GiB 20GiB)
engine_config:
  context_length: 8192
EOF
  deploy b7-admit || { fail_ids "deploy" "refused; see cli.log" B7; role_down; return; }
  local rc avail code msg reason warns textw rolew ok
  avail=$(memavail)
  check B7 "start --wait" "capyctl start deployment b7-admit --wait --json exits 4 with code insufficient_resources and a message with the four figures ('needs X GiB of unified memory, Y GiB available and a 11.0 GiB free reserve to keep, Z GiB short'); status latest_operation.reason carries them too"
  run timeout 1200 "$CAPYCTL" --state-dir "$SD" start deployment b7-admit --wait --json >"$RES/b7-start.out" 2>"$RES/b7-start.err"
  rc=$?
  status_json b7-admit "$RES/b7-status.json"
  c status deployment b7-admit >"$RES/b7-status.txt" 2>&1
  if [ "$PLAN" = 0 ]; then
    code=$(ev get "$RES/b7-start.err" code)
    msg=$(ev get "$RES/b7-start.err" message)
    reason=$( { ev get "$RES/b7-status.json" 'latest_operation.reason'; ev get "$RES/b7-status.json" 'instances[0].latest_operation.reason'; } | tr '\n' ' ')
    ok=fail
    if [ "$rc" = 4 ] && [ "$code" = insufficient_resources ] &&
       [[ $msg =~ GiB\ available\ and\ a\ .*free\ reserve\ to\ keep,\ .*short ]] && [[ $reason == *"free reserve to keep"* ]]; then ok=pass; fi
    result B7 "start --wait" "$ok" "exit $rc, code ${code:-none}; MemAvailable $avail GiB before; message: $(printf '%s' "$msg" | grep -o 'insufficient_memory: [^;]*' | head -1); status reason $([[ $reason == *"free reserve to keep"* ]] && echo 'has the figures' || echo 'lacks the figures')"
  fi
  check B7 "start-up warning" "the role log has the warning '... managed limit plus its ... free reserve ...: a deployment near the limit cannot be admitted until ... more is free'; list hosts --json has hosts[].session.memory_warnings and list hosts prints a 'warning: <host>: ...' line"
  c list hosts --json >"$RES/b7-hosts.json" 2>>"$RES/cli.log"
  c list hosts >"$RES/b7-hosts.txt" 2>>"$RES/cli.log"
  if [ "$PLAN" = 0 ]; then
    warns=$(ev get "$RES/b7-hosts.json" 'session.memory_warnings')
    textw=$(grep -c '^warning: .*managed limit plus its' "$RES/b7-hosts.txt")
    rolew=$(grep -c 'managed limit plus its' "$ROLE_LOG")
    ok=fail
    [ -n "$warns" ] && [ "$textw" -gt 0 ] && [ "$rolew" -gt 0 ] && ok=pass
    result B7 "start-up warning" "$ok" "role log lines $rolew, list hosts warning lines $textw, memory_warnings: $(printf '%s' "$warns" | cut -c1-240)"
  fi
  role_down
}

# B1, B2 and the vLLM parts of B3, B5: model 1, deep, no declared loader. The
# default is whatever CapyCTL resolves by memory shape (provenance 'capyctl
# default'); the run records which and checks the launch carries it.
step_vllm_default() {
  want B1 B2 B3 B5 || return 0
  phase "vLLM 0.30.0, model 1, deep parking, no declared loader: B1, B2, B3 vLLM, B5 vLLM"
  profile v30 vllm "$V30" vllm || { fail_ids "vLLM" "engine add failed" B1 B2 B3 B5; return; }
  dep_vllm_m1 v-default ""
  deploy v-default || { fail_ids "vLLM" "deploy refused; see cli.log" B1 B2 B3 B5; return; }
  startd v-default cold-default || { fail_ids "vLLM" "start failed; see cli.log" B1 B2 B3 B5; stopd v-default; return; }
  local id base ready_gib strat prov sleepf loadf metf
  base=$BEFORE_GIB ready_gib=$READY_GIB
  id=$(dep_id v-default)
  if want B1; then
    check B1 config "inspect deployment --effective-config --json: engine_config.provenance[\"vllm.safetensors_load_strategy\"] 'capyctl default' and engine_config.safetensors_load_strategy lazy or eager (recorded); the engine command line holds --enable-sleep-mode and '--safetensors-load-strategy <that strategy>'"
    c inspect deployment v-default --effective-config --json >"$RES/b1-effective.json" 2>>"$RES/cli.log"
    strat=$(ev get "$RES/b1-effective.json" 'engine_config.safetensors_load_strategy')
    prov=$(ev get "$RES/b1-effective.json" 'engine_config.provenance["vllm.safetensors_load_strategy"]')
    sleepf=$(argv_count "$V30" "--enable-sleep-mode")
    loadf=$(argv_count "$V30" "--safetensors-load-strategy ${strat:-<strategy>}")
    if [ "$PLAN" = 0 ]; then
      if [[ $strat =~ ^(lazy|eager)$ ]] && [ "$prov" = "capyctl default" ] && [ "${sleepf:-0}" -gt 0 ] && [ "${loadf:-0}" -gt 0 ]; then
        result B1 config pass "strategy $strat ($prov); argv: sleep mode in $sleepf, $strat loader in $loadf process(es); cold start $START_S s, ready footprint $ready_gib GiB"
      else
        result B1 config fail "strategy '${strat}', provenance '${prov}', argv sleep mode $sleepf, '--safetensors-load-strategy ${strat}' in ${loadf:-0}"
      fi
    fi
  fi
  if want B3; then
    check B3 "vLLM argv" "the vLLM launch carries --enable-per-request-metrics and the protected entry accepted it (Ready)"
    metf=$(argv_count "$V30" "--enable-per-request-metrics")
    [ "$PLAN" = 1 ] || { if [ "${metf:-0}" -gt 0 ]; then result B3 "vLLM argv" pass "--enable-per-request-metrics in $metf process(es); Ready"
                          else result B3 "vLLM argv" fail "--enable-per-request-metrics not on the command line"; fi; }
    metrics_check vLLM vllm v-default
  fi
  want B5 && redaction_check vLLM v-default "$id"
  want B1 B2 && cycles v-default "$id" "$base" gate
  stopd v-default
}

# B1 lazy variant: the same deployment with safetensors_load_strategy lazy stated.
# Informational (never decides B1): on this hybrid model the lazy wake reloads
# weights in place and may hang or answer wrong (2026-10-14 run).
step_vllm_lazy() {
  want B1 || return 0
  phase "B1 lazy variant (informational): vLLM 0.30.0, model 1, deep, safetensors_load_strategy lazy"
  profile v30 vllm "$V30" vllm || { result B1 "lazy variant" info "engine add failed"; return; }
  dep_vllm_m1 v-lazy lazy
  deploy v-lazy || { [ "$PLAN" = 1 ] || result B1 "lazy variant" info "deploy refused; see cli.log"; return; }
  startd v-lazy cold-lazy || { result B1 "lazy variant" info "start failed; see cli.log"; stopd v-lazy; return; }
  local id base
  base=$BEFORE_GIB
  id=$(dep_id v-lazy)
  cycles v-lazy "$id" "$base" info
  stopd v-lazy
}

# B14 vLLM: cache_salt on a dense model (the hybrid model 1 reuses no prefix cache).
step_vllm_salt() {
  want B14 || return 0
  phase "B14 vLLM 0.30.0, Qwen3-4B (dense), restart_only: cache_salt"
  profile v30 vllm "$V30" vllm || { fail_ids "vLLM 4B" "engine add failed" B14; return; }
  yaml v-4b <<EOF
schema_version: 1
kind: deployment
name: v-4b
engine: v30
model: $Q4B
residency: restart_only
timeouts: {initialize: 1800s}
engine_config:
  context_length: 8192
  # Cached-token counts in usage (an ordinary extra flag), for the cache_salt check.
  accept_extra_args: true
  extra_args: [--enable-prompt-tokens-details]
EOF
  deploy v-4b || { fail_ids "vLLM 4B" "deploy refused; see cli.log" B14; return; }
  startd v-4b cold-vllm-4b || { fail_ids "vLLM 4B" "start failed; see cli.log" B14; stopd v-4b; return; }
  salt_check vLLM v-4b salt
  stopd v-4b
}

cycles() { # name id base-GiB gate|info: three deep parks and wakes on request (B1, B2)
  local n=$1 id=$2 base=$3 mode=$4 i st r pb gs ok=1 marks="" wakes=0 gen rows mm le scope=cycles wt=1800 pre=b1
  if [ "$mode" = info ]; then
    # A hung wake must not hold the run: ten minutes a wake, and no further cycle after one fails.
    scope="lazy variant" wt=600 pre=b1-lazy
    check B1 "$scope" "INFORMATIONAL, never decides B1: three parks and request wakes with the lazy loader stated; recorded per cycle as for the default"
  else
    check B1 cycles "three parks reach parked and three requests wake the deployment and answer 42; parked.growth[].state never past_limit or stopped; parked charge (status parked.bytes) and MemAvailable drop recorded per cycle (catalog lazy reference: parked ~8 GB after three cycles)"
  fi
  for i in 1 2 3; do
    if [ "$PLAN" = 1 ] && [ "$i" -gt 1 ]; then show "(cycles 2 and 3 repeat cycle 1)"; break; fi
    say "B1 $scope $i: park, then a request wakes it"
    c park deployment "$n" >>"$RES/cli.log" 2>&1
    st=$(poll_state "$n" 600 parked stopped failed)
    pause 10
    status_json "$n" "$RES/$pre-parked-$i.json"
    pb=$(ev get "$RES/$pre-parked-$i.json" parked.bytes)
    gs=$(ev get "$RES/$pre-parked-$i.json" 'parked.growth[0].state')
    marks="$marks c$i: $st, charge $(gib "$pb") GiB, MemAvailable -$(sub "$base" "$(memavail)") GiB, growth ${gs:-none};"
    [ "$st" = parked ] || ok=0
    case $gs in past_limit|stopped) ok=0 ;; esac
    r=$(helper chat --cred "$CRED" --endpoint "$EP" --model "$n" --timeout "$wt" --out "$RES/$pre-wake-$i.json" 2>>"$RES/cli.log")
    [ "$PLAN" = 1 ] && continue
    marks="$marks wake ${r#* } s;"
    if [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ]; then wakes=$((wakes + 1)); else ok=0; fi
    status_json "$n" "$RES/$pre-woken-$i.json"
    [ "$mode" = info ] && [ "$ok" = 0 ] && break
  done
  if [ "$mode" = info ]; then
    [ "$PLAN" = 1 ] || result B1 "$scope" info "$([ "$ok" = 1 ] && echo pass || echo fail):$marks"
    return 0
  fi
  [ "$PLAN" = 1 ] || { if [ "$ok" = 1 ]; then result B1 cycles pass "$marks"; else result B1 cycles fail "$marks"; fi; }
  want B2 || return 0
  check B2 GB10 "the three wakes above answered; the ledger (SELECT, mode=ro) holds a wake_canaries row for the current generation with mismatch_operation_id NULL; no operation has error_code wake_mismatch; instances[0].last_error is not wake_mismatch"
  [ "$PLAN" = 1 ] && { show "rc.py sql $DB \"SELECT instance_index, generation, mismatch_operation_id IS NULL, length(tokens_json), length(text) FROM wake_canaries WHERE deployment_id='<id>'\""
                        show "rc.py sql $DB \"SELECT count(*) FROM operations WHERE error_code='wake_mismatch'\""; return 0; }
  gen=$(ev get "$RES/b1-woken-3.json" 'instances[0].generation')
  le=$(ev get "$RES/b1-woken-3.json" 'instances[0].last_error')
  rows=$(ev sql "$DB" "SELECT instance_index, generation, mismatch_operation_id IS NULL, length(tokens_json), length(text) FROM wake_canaries WHERE deployment_id='$id'")
  mm=$(ev sql "$DB" "SELECT count(*) FROM operations WHERE error_code='wake_mismatch'")
  printf '%s\n' "$rows" >"$RES/b2-canaries.tsv"
  if [ "$wakes" = 3 ] && printf '%s\n' "$rows" | awk -F'\t' -v g="$gen" '$2 == g && $3 == 1 {f=1} END {exit !f}' &&
     [ "${mm:-1}" = 0 ] && [[ ${le:-} != *wake_mismatch* ]]; then
    result B2 GB10 pass "3/3 wakes answered; canary row for generation $gen, no mismatch; wake_mismatch operations $mm"
  else
    result B2 GB10 fail "wakes $wakes/3; generation ${gen:-?}; canary rows: $(printf '%s' "$rows" | tr '\t\n' ', ' | cut -c1-200); wake_mismatch operations ${mm:-?}; last_error ${le:-none}"
  fi
}

# B4: the eager loader reproduces parked growth; the park past the bound is a stop.
# Runs on its own role whose parked_growth_limit is 0B (ADR 0014 amendment A19):
# any growth past the first measured charge (strictly greater than 0 bytes) is
# past the bound. A launch whose charge never grows is within every bound, so
# zero growth over every park is SKIP with that evidence, not FAIL.
step_vllm_eager() {
  want B4 || return 0
  phase "B4 vLLM 0.30.0, model 1, deep, safetensors_load_strategy eager, parked_growth_limit 0B: parks until the growth bound turns one into a stop"
  profile v30 vllm "$V30" vllm || { fail_ids "vLLM eager" "engine add failed" B4; return; }
  dep_vllm_m1 v-eager eager
  deploy v-eager || { fail_ids "vLLM eager" "deploy refused" B4; return; }
  startd v-eager cold-eager || { fail_ids "vLLM eager" "start failed; see cli.log" B4; stopd v-eager; return; }
  local id i st r pb gs sop gen0 pid0 marks="" stop_park=0 jr own gen1 pid1 ok gfirst glast glim grown=0
  id=$(dep_id v-eager)
  check B4 "growth stop" "within $MAX_PARKS parks, a park after parked.growth[].state past_limit ends with observed_state stopped and growth state stopped with stop_operation_id; the ledger has journal_entries.state park_growth_stop for that operation; no engine process is left; instances[0].reservation_owner is gone; the next request answers 42 from a fresh launch (new generation and engine PID)"
  status_json v-eager "$RES/b4-ready.json"
  gen0=$(ev get "$RES/b4-ready.json" 'instances[0].generation')
  pid0=$(engine_pid "$V30")
  for i in $(seq "$MAX_PARKS"); do
    if [ "$PLAN" = 1 ] && [ "$i" -gt 1 ]; then show "(repeat park/wake up to $MAX_PARKS parks until a park is turned into the growth stop)"; break; fi
    say "B4 park $i"
    c park deployment v-eager --json >"$RES/b4-park-$i.json" 2>>"$RES/cli.log"
    st=$(poll_state v-eager 600 parked stopped failed)
    pause 5
    status_json v-eager "$RES/b4-after-park-$i.json"
    pb=$(ev get "$RES/b4-after-park-$i.json" parked.bytes)
    gs=$(ev get "$RES/b4-after-park-$i.json" 'parked.growth[0].state')
    sop=$(ev get "$RES/b4-after-park-$i.json" 'parked.growth[0].stop_operation_id')
    gfirst=$(ev get "$RES/b4-after-park-$i.json" 'parked.growth[0].first_bytes')
    glast=$(ev get "$RES/b4-after-park-$i.json" 'parked.growth[0].last_bytes')
    glim=$(ev get "$RES/b4-after-park-$i.json" 'parked.growth[0].limit_bytes')
    [ "${glast:-0}" -gt "${gfirst:-0}" ] 2>/dev/null && grown=1
    marks="$marks p$i: $st, charge $(gib "$pb") GiB, growth ${gs:-none} ($(( ${glast:-0} - ${gfirst:-0} )) B past the first, limit ${glim:-none} B);"
    if [ "$st" = stopped ] || [ "$gs" = stopped ]; then stop_park=$i; break; fi
    r=$(helper chat --cred "$CRED" --endpoint "$EP" --model v-eager --out "$RES/b4-wake-$i.json" 2>>"$RES/cli.log")
    [ "$PLAN" = 1 ] || marks="$marks wake HTTP ${r%% *} ${r#* } s;"
  done
  [ "$PLAN" = 1 ] && { show "then: rc.py sql $DB \"SELECT count(*) FROM journal_entries WHERE state='park_growth_stop' AND operation_id='<stop_operation_id>'\"; status; a request; status"; stopd v-eager; return 0; }
  if [ "$stop_park" = 0 ]; then
    if [ "$grown" = 0 ]; then
      result B4 "growth stop" skip "n/a (no growth in $MAX_PARKS parks under a 0B bound: the parked charge never exceeded the first measured one):$marks"
    else
      result B4 "growth stop" fail "no park became a stop in $MAX_PARKS parks although the charge grew past a 0B bound:$marks"
    fi
    stopd v-eager
    return
  fi
  waitgone
  status_json v-eager "$RES/b4-stopped.json"
  st=$(ev get "$RES/b4-stopped.json" observed_state)
  own=$(ev get "$RES/b4-stopped.json" 'instances[0].reservation_owner')
  jr=$(ev sql "$DB" "SELECT count(*) FROM journal_entries WHERE state='park_growth_stop' AND operation_id='$sop'")
  ev sql "$DB" "SELECT state, substr(evidence, 1, 300) FROM journal_entries WHERE operation_id='$sop'" >"$RES/b4-journal.tsv"
  r=$(helper chat --cred "$CRED" --endpoint "$EP" --model v-eager --out "$RES/b4-fresh.json" 2>>"$RES/cli.log")
  status_json v-eager "$RES/b4-fresh-status.json"
  gen1=$(ev get "$RES/b4-fresh-status.json" 'instances[0].generation')
  pid1=$(engine_pid "$V30")
  ok=fail
  if [ "$st" = stopped ] && [ -z "$own" ] && [ "${jr:-0}" -ge 1 ] && [ -n "$sop" ] &&
     [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ] && [ "$gen1" != "$gen0" ] && [ "$pid1" != "$pid0" ]; then ok=pass; fi
  result B4 "growth stop" "$ok" "$marks park $stop_park became the stop (operation ${sop:-none}, journal park_growth_stop rows ${jr:-0}); then state $st, reservation owner ${own:-none}; next request HTTP ${r%% *} in ${r#* } s, generation $gen0 -> $gen1, engine pid changed $([ "$pid1" != "$pid0" ] && echo yes || echo no)"
  stopd v-eager
}

# B3 and B14 on TensorFold 0.6.6 (CUDA prefill_s mapping, cache_salt refusal).
step_tf() {
  want B3 B14 || return 0
  phase "TensorFold 0.6.6, model 1 (CUDA): B3 TensorFold, B14 TensorFold"
  profile tf66 tensorfold "$TF66" tensorfold --approve-option=--drafter --approve-path "$HOME/drafters" ||
    { fail_ids "TensorFold" "engine add failed" B3 B14; return; }
  dep_tf_m1
  deploy t-m1 || { fail_ids "TensorFold" "deploy refused" B3 B14; return; }
  startd t-m1 cold-tf || { fail_ids "TensorFold" "start failed; see cli.log" B3 B14; stopd t-m1; return; }
  want B3 && metrics_check TensorFold tensorfold t-m1
  want B14 && salt_check TensorFold t-m1 refused
  stopd t-m1
}

# B3, B5, B6, B14 on SGLang 0.5.21 with a small model, two requests at a time.
step_sglang_4b() {
  want B3 B5 B6 B14 || return 0
  phase "SGLang 0.5.21, Qwen3-4B, max_concurrent_requests 2: B3 SGLang, B5 SGLang, B6, B14 SGLang"
  profile s21 sglang "$S21" sglang || { fail_ids "SGLang" "engine add failed" B3 B5 B6 B14; return; }
  dep_sglang_4b s-4b 0
  deploy s-4b || { fail_ids "SGLang" "deploy refused" B3 B5 B6 B14; return; }
  startd s-4b cold-sglang-4b || { fail_ids "SGLang" "start failed; see cli.log" B3 B5 B6 B14; stopd s-4b; return; }
  local id r
  id=$(dep_id s-4b)
  if want B3; then
    metrics_check SGLang sglang s-4b
    check B3 "SGLang cached_tokens" "the same long prompt twice: the repeat reports usage.prompt_tokens_details.cached_tokens > 0 and capyctl.metrics.cached_tokens source engine (enable_cache_report on by default)"
    helper cached --cred "$CRED" --endpoint "$EP" --model s-4b --out "$RES/b3-sglang-cached.json" >>"$RES/cli.log" 2>&1
    r=$(ev cache-eval "$RES/b3-sglang-cached.json" --expect cached)
    [ "$PLAN" = 1 ] || resr B3 "SGLang cached_tokens" "$r"
  fi
  want B14 && salt_check SGLang s-4b salt
  want B5 && redaction_check SGLang s-4b "$id" --sglang
  want B6 && load_check
  stopd s-4b
}

load_check() { # B6 on s-4b
  local from p1 p2 r
  check B6 "load read" "while two streamed requests run, GET /management/v1/metrics/load (admin token) shows for s-4b a fresh sample with engine.running >= 1 and engine.waiting present, and instances[].max_running.source engine"
  pause 5
  from=0
  [ "$PLAN" = 1 ] || from=$(wc -l <"$ROLE_LOG")
  helper stream --cred "$CRED" --endpoint "$EP" --model s-4b --prompt "Write a 600-word story about a lighthouse keeper." \
    --max-tokens 1200 --out "$RES/b6-stream-1.json" >>"$RES/cli.log" 2>&1 &
  p1=$!
  pause 2
  helper stream --cred "$CRED" --endpoint "$EP" --model s-4b --prompt "Write a 400-word story about a tide pool." \
    --max-tokens 800 --out "$RES/b6-stream-2.json" >>"$RES/cli.log" 2>&1 &
  p2=$!
  [ "$PLAN" = 1 ] && show "(both in the background) rc.py load: poll GET 127.0.0.1:$MPORT/management/v1/metrics/load every 0.5 s while the first runs (at most 120 s)"
  r=$(ev load --cred "$CRED" --maddr "$MADDR" --name s-4b --pid "$p1" --limit 120 --out "$RES/b6-load.jsonl")
  wait "$p1" "$p2" 2>/dev/null
  [ "$PLAN" = 1 ] || resr B6 "load read" "$r"
  check B6 router "the role log's router_selection events after the requests name a candidate with load_source engine"
  r=$(ev selection "$ROLE_LOG" --from-line "$from")
  [ "$PLAN" = 1 ] || resr B6 router "$r"
}

# B8: explicit activation, SGLang 4B.
step_sglang_explicit() {
  want B8 || return 0
  phase "B8 SGLang 0.5.21, Qwen3-4B, lifecycle.activation explicit"
  profile s21 sglang "$S21" sglang || { fail_ids "SGLang explicit" "engine add failed" B8; return; }
  dep_sglang_4b s-4b-x 1
  deploy s-4b-x || { fail_ids "SGLang explicit" "deploy refused" B8; return; }
  check B8 explicit "status activation explicit; after an operator park, a request answers 409 with code deployment_inactive at once and the deployment is still parked 15 s later (nothing woke it); capyctl start deployment --wait wakes it (ready) and a request answers 42"
  startd s-4b-x explicit-start || { fail_ids "SGLang explicit" "start failed; see cli.log" B8; stopd s-4b-x; return; }
  local act st1 r1 code st2 r2 r0 took
  status_json s-4b-x "$RES/b8-ready.json"
  act=$(ev get "$RES/b8-ready.json" activation)
  r0=$(helper chat --cred "$CRED" --endpoint "$EP" --model s-4b-x --out "$RES/b8-chat-ready.json" 2>>"$RES/cli.log")
  c park deployment s-4b-x >>"$RES/cli.log" 2>&1
  st1=$(poll_state s-4b-x 600 parked stopped failed)
  pause 5
  r1=$(helper chat --cred "$CRED" --endpoint "$EP" --model s-4b-x --out "$RES/b8-chat-parked.json" 2>>"$RES/cli.log")
  code=$(ev get "$RES/b8-chat-parked.json" code)
  pause 15
  st2=$(state_of s-4b-x parked)
  took=$(ev get "$RES/b8-chat-parked.json" _total_s)
  startd s-4b-x explicit-wake || st2="$st2 (start failed)"
  r2=$(helper chat --cred "$CRED" --endpoint "$EP" --model s-4b-x --out "$RES/b8-chat-woken.json" 2>>"$RES/cli.log")
  if [ "$PLAN" = 0 ]; then
    if [ "$act" = explicit ] && [ "${r0%% *}" = 200 ] && [ "$st1" = parked ] && [ "${r1%% *}" = 409 ] &&
       [ "$code" = deployment_inactive ] && [ "$st2" = parked ] && [ "${r2%% *}" = 200 ] && [ "${r2##* }" = ok ]; then
      result B8 explicit pass "activation $act; parked; request HTTP 409 $code in $took s; still $st2 15 s later; start woke it in $START_S s; request HTTP 200 answered"
    else
      result B8 explicit fail "activation ${act:-none}; ready request ${r0%% *}; park -> $st1; parked request HTTP ${r1%% *} code ${code:-none}; 15 s later $st2; after start request ${r2:-none}"
    fi
  fi
  stopd s-4b-x
}

# B9 and B15: SGLang's static pool with small KV caches on the hybrid model 1.
step_sglang_27b() {
  want B9 B15 || return 0
  phase "SGLang 0.5.21, model 1 (hybrid): B9 (kv_cache 1536MiB with --max-mamba-cache-size 8, then kv_cache 1GiB), B15"
  profile s21 sglang "$S21" sglang || { fail_ids "SGLang 27B" "engine add failed" B9 B15; return; }
  dep_sglang_m1 s-m1-mamba 1536MiB '  accept_extra_args: true
  extra_args: [--max-mamba-cache-size, "8"]'
  deploy s-m1-mamba || { fail_ids "SGLang 27B" "deploy refused" B9 B15; return; }
  local id r p logs want_max
  check B9 "kv 1536MiB, mamba 8" "the launch becomes Ready (no 'Loaded weights leave no GPU memory for the KV cache') and answers 42"
  if startd s-m1-mamba cold-sglang-27b; then
    r=$(helper chat --cred "$CRED" --endpoint "$EP" --model s-m1-mamba --out "$RES/b9-mamba-chat.json" 2>>"$RES/cli.log")
    [ "$PLAN" = 1 ] || { if [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ]; then result B9 "kv 1536MiB, mamba 8" pass "Ready in $START_S s, answered in ${r#* } s"
                          else result B9 "kv 1536MiB, mamba 8" fail "Ready, request $r"; fi; }
    if want B15; then
      id=$(dep_id s-m1-mamba)
      check B15 "load read" "during a request the load read's instances[].max_running is source engine and its count equals the max_running_requests SGLang resolved (the engine log's last max_running_requests=)"
      mapfile -t logs < <(launch_logs "$id")
      ev facts "${logs[@]}" --out "$RES/b15-facts.json" >/dev/null
      want_max=$(ev get "$RES/b15-facts.json" max_running_requests)
      helper stream --cred "$CRED" --endpoint "$EP" --model s-m1-mamba --prompt "Count from 1 to 200, one number per line." \
        --max-tokens 1500 --out "$RES/b15-stream.json" >>"$RES/cli.log" 2>&1 &
      p=$!
      [ "$PLAN" = 1 ] && show "(in the background) rc.py load --want-max <engine max_running_requests>: poll the load read while it runs (at most 90 s)"
      r=$(ev load --cred "$CRED" --maddr "$MADDR" --name s-m1-mamba --pid "$p" --limit 90 --out "$RES/b15-load.jsonl" ${want_max:+--want-max "$want_max"})
      wait "$p" 2>/dev/null
      [ "$PLAN" = 1 ] || { if [ -z "$want_max" ]; then result B15 "load read" fail "no max_running_requests= in the engine log; $r"; else resr B15 "load read" "$r"; fi; }
    fi
  else
    fail_ids "kv 1536MiB, mamba 8" "start failed; see cli.log" B9 B15
  fi
  stopd s-m1-mamba
  want B9 || return 0
  dep_sglang_m1 s-m1-kv1g 1GiB ""
  deploy s-m1-kv1g || { fail_ids "kv 1GiB" "deploy refused" B9; return; }
  check B9 "kv 1GiB" "the launch becomes Ready and answers 42"
  if startd s-m1-kv1g sglang-27b-kv1g; then
    r=$(helper chat --cred "$CRED" --endpoint "$EP" --model s-m1-kv1g --out "$RES/b9-kv1g-chat.json" 2>>"$RES/cli.log")
    [ "$PLAN" = 1 ] || { if [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ]; then result B9 "kv 1GiB" pass "Ready in $START_S s, answered in ${r#* } s"
                          else result B9 "kv 1GiB" fail "Ready, request $r"; fi; }
  else
    fail_ids "kv 1GiB" "start failed; see cli.log" B9
  fi
  stopd s-m1-kv1g
}

# B12: the drafter as the deployment's second model source.
step_draft() {
  want B12 || return 0
  phase "B12 SGLang 0.5.21, model 1 with model.draft ($([ "$DRAFT_SOURCE" = hf ] && echo "$DRAFT_HF, downloaded by CapyCTL" || echo "the local ~/drafters/Qwen3.8-27B-DFlash2")) and DFLASH"
  profile s21 sglang "$S21" sglang || { fail_ids "drafter" "engine add failed" B12; return; }
  dep_sglang_draft
  deploy s-m1-draft || { fail_ids "drafter" "deploy refused; see cli.log" B12; return; }
  local src id logs path want_path r nsrc named
  check B12 drafter "the drafter's source is verified (status model_sources[] all verified) before activation; the launch renders it (the engine log's server_args speculative_draft_model_path names $([ "$DRAFT_SOURCE" = hf ] && echo "a z-lab--Qwen3.8-27B-DFlash2@<pinned commit> directory under the main role's sources directory (src1/sources)" || echo "the local ~/drafters/Qwen3.8-27B-DFlash2")); Ready; answers 42 (one retry at the memory request CapyCTL names in a refusal)"
  src=$(poll_sources s-m1-draft 3600)
  if ! startd s-m1-draft draft; then
    # One retry at the size the refusal names ('raise engine_config.memory.request to "N"').
    named=$(tail -40 "$RES/cli.log" 2>/dev/null | grep -o 'raise engine_config\.memory\.request to "[0-9]*[KMGT]iB"' | tail -1 | cut -d'"' -f2)
    if [ -z "$named" ]; then result B12 drafter fail "start failed (sources: $src); see cli.log"; stopd s-m1-draft; return; fi
    say "B12: the refusal names memory.request $named; one retry at that size"
    c delete deployment s-m1-draft --stop >>"$RES/cli.log" 2>&1
    waitgone
    dep_sglang_draft "$named"
    deploy s-m1-draft || { result B12 drafter fail "redeploy at the named request $named refused; see cli.log"; return; }
    src=$(poll_sources s-m1-draft 3600)
    startd s-m1-draft draft-retry || { result B12 drafter fail "start failed at the named request $named (sources: $src); see cli.log"; stopd s-m1-draft; return; }
  fi
  id=$(dep_id s-m1-draft)
  mapfile -t logs < <(launch_logs "$id")
  ev facts "${logs[@]}" --out "$RES/b12-facts.json" >/dev/null
  path=$(ev get "$RES/b12-facts.json" speculative_draft_model_path)
  status_json s-m1-draft "$RES/b12-status.json"
  nsrc=$(ev get "$RES/b12-status.json" model_sources | python3 -c 'import json,sys
try: print(len(json.load(sys.stdin)))
except ValueError: print(0)' 2>/dev/null)
  r=$(helper chat --cred "$CRED" --endpoint "$EP" --model s-m1-draft --out "$RES/b12-chat.json" 2>>"$RES/cli.log")
  # The rendered path, as the engine's server_args name it: a fetched drafter is a directory
  # of the main role's sources store (--model-sources-path $WORK/src1), named repo@commit.
  path=$(realpath -m -- "${path/#\~\//$HOME/}" 2>/dev/null)
  if [ "$DRAFT_SOURCE" = hf ]; then
    want_path="$(realpath -m -- "$WORK/src1/sources")/*/z-lab--Qwen3.8-27B-DFlash2@${DRAFT_HF#*@}"
  else
    want_path=$(realpath -m -- "$DRAFTER")
  fi
  if [ "$PLAN" = 0 ]; then
    # shellcheck disable=SC2053 # want_path is a glob for hf: any store kind directory
    if [ -n "$path" ] && [[ $path == $want_path ]] && { [ "$DRAFT_SOURCE" = local ] || [ "$src" = verified ]; } &&
       [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ]; then
      result B12 drafter pass "sources $src ($nsrc listed); draft path rendered ${path/#$HOME/~}; Ready in $START_S s; answered in ${r#* } s"
    else
      result B12 drafter fail "sources ${src:-none} ($nsrc listed); draft path '${path/#$HOME/~}'; request ${r:-none}"
    fi
  fi
  stopd s-m1-draft
}

# B11 (and the download B13 reuses): a small Hugging Face model, fetched by CapyCTL.
step_q05() {
  want B11 B13 || return 0
  phase "Qwen2.5-0.5B-Instruct from Hugging Face into this run's sources directory: B11$(want B13 && echo ", and the files B13 serves")"
  profile v30 vllm "$V30" vllm || { fail_ids "download" "engine add failed" B11 B13; return; }
  dep_vllm_small q05 "{hf: $Q05_HF}"
  deploy q05 || { fail_ids "download" "deploy refused; see cli.log" B11 B13; return; }
  local src prov r txt
  src=$(poll_sources q05 1800)
  [ "$src" = verified ] || [ "$PLAN" = 1 ] || { fail_ids "download" "sources $src" B11 B13; return; }
  want B11 || return 0
  check B11 fetched "the first start after the download: status checkpoint_digest.provenance fetched and the text status line 'Checkpoint  digest fetched'; Ready; answers 42 (the role's rchar during the start is recorded: a second full read would add about 1 GB)"
  local pid io0 io1
  pid=$(cat "$PIDFILE" 2>/dev/null)
  io0=$(awk '/^rchar:/ {print $2}' "/proc/$pid/io" 2>/dev/null)
  startd q05 fetched-first || { result B11 fetched fail "start failed; see cli.log"; stopd q05; return; }
  io1=$(awk '/^rchar:/ {print $2}' "/proc/$pid/io" 2>/dev/null)
  status_json q05 "$RES/b11-status.json"
  c status deployment q05 >"$RES/b11-status.txt" 2>&1
  prov=$(ev get "$RES/b11-status.json" 'checkpoint_digest.provenance')
  txt=$(grep -c 'digest fetched' "$RES/b11-status.txt" 2>/dev/null)
  r=$(helper chat --cred "$CRED" --endpoint "$EP" --model q05 --out "$RES/b11-chat.json" 2>>"$RES/cli.log")
  if [ "$PLAN" = 0 ]; then
    if [ "$prov" = fetched ] && [ "${txt:-0}" -gt 0 ] && [ "${r%% *}" = 200 ]; then
      result B11 fetched pass "provenance $prov; Ready in $START_S s; request ${r%% *} (${r##* }); role rchar during the start $(gib $(( ${io1:-0} - ${io0:-0} ))) GiB"
    else
      result B11 fetched fail "provenance '${prov}', text note lines ${txt:-0}, request ${r:-none}"
    fi
  fi
  stopd q05
}

# B13: plain-HTTP mirror and a secret URL, on a role that approves plain HTTP
# for this run's loopback server only.
step_b13() {
  want B13 || return 0
  phase "B13 source extensions: own role with plain HTTP approved for 127.0.0.1:$HTTP_PORT only, a loopback python3 -m http.server"
  local store www=$WORK/www secret sha_a sha_b r st reason
  store="$WORK/src1/sources/huggingface/Qwen--Qwen2.5-0.5B-Instruct@${Q05_HF#*@}"
  if [ "$PLAN" = 0 ] && [ ! -f "$store/model.safetensors" ]; then
    store=$(find "$WORK/src1/sources/huggingface" -maxdepth 1 -type d -name 'Qwen--Qwen2.5-0.5B-Instruct@*' 2>/dev/null | head -1)
    [ -n "$store" ] && [ -f "$store/model.safetensors" ] || { fail_ids "payload" "no downloaded Qwen2.5-0.5B files to serve (the B11 download failed)" B13; return; }
  fi
  run mkdir -p "$www/extra"
  [ "$PLAN" = 1 ] || echo variant-b >"$www/extra/rc-variant.txt"
  # CapyCTL extracts plain ustar only (crates/capyctl-agent/src/sources/tar.rs, ADR 0008):
  # regular files and directories under relative names; a pax extended header
  # (GNU tar's --format=posix writes one per file), a link or a device refuses the
  # whole archive as unsafe_archive. So: ustar, symlinks dereferenced (-h), bare
  # relative member names, numeric owner.
  # shellcheck disable=SC2086 # the file list is words
  run tar --format=ustar -h --numeric-owner --owner=0 --group=0 -cf "$www/q05a.tar" -C "$store" $Q05_FILES
  # shellcheck disable=SC2086
  run tar --format=ustar -h --numeric-owner --owner=0 --group=0 -cf "$www/q05b.tar" -C "$store" $Q05_FILES -C "$www/extra" rc-variant.txt
  sha_a='<sha256 of q05a.tar>' sha_b='<sha256 of q05b.tar>'
  if [ "$PLAN" = 0 ]; then
    sha_a=$(sha256sum "$www/q05a.tar" | cut -c1-64)
    sha_b=$(sha256sum "$www/q05b.tar" | cut -c1-64)
    python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 --directory "$www" >"$RES/b13-http-server.log" 2>&1 </dev/null &
    HTTP_PID=$!
    echo "$HTTP_PID" >"$WORK/http.pid"
    sleep 2
  else
    show "\$ python3 -m http.server $HTTP_PORT --bind 127.0.0.1 --directory $www >res/b13-http-server.log 2>&1 &"
  fi
  role_up s3 --model-sources-path "$WORK/src3" --model-sources-plain-http allowed \
    --set "host.model_sources.http=allowed" --set "host.model_sources.allowed_hosts=[\"127.0.0.1:$HTTP_PORT\"]" ||
    { fail_ids "role" "standalone did not become ready" B13; role_down; stop_http; return; }
  profile v30 vllm "$V30" vllm || { fail_ids "profile" "engine add failed" B13; role_down; stop_http; return; }

  check B13 "plain HTTP" "an http source http://127.0.0.1:$HTTP_PORT/q05a.tar (archive tar, its sha256) is fetched by the role (the server logs the GET), verified (model_sources[].state verified), starts Ready and answers"
  dep_vllm_small q05-http "{source: {http: {url: \"http://127.0.0.1:$HTTP_PORT/q05a.tar\", sha256: $sha_a, archive: tar}}}"
  if deploy q05-http; then
    st=$(poll_sources q05-http 900)
    if [ "$st" = verified ] && startd q05-http plain-http; then
      r=$(helper chat --cred "$CRED" --endpoint "$EP" --model q05-http --out "$RES/b13-http-chat.json" 2>>"$RES/cli.log")
      [ "$PLAN" = 1 ] || { if [ "${r%% *}" = 200 ] && grep -q 'GET /q05a.tar' "$RES/b13-http-server.log"; then
                             result B13 "plain HTTP" pass "fetched from the loopback mirror, verified, Ready in $START_S s, request ${r%% *}"
                           else result B13 "plain HTTP" fail "request ${r:-none}; server GET lines $(grep -c 'GET /q05a.tar' "$RES/b13-http-server.log")"; fi; }
    else
      [ "$PLAN" = 1 ] || result B13 "plain HTTP" fail "sources $st or start failed; see cli.log"
    fi
    stopd q05-http
  else
    [ "$PLAN" = 1 ] || result B13 "plain HTTP" fail "deploy refused; see cli.log"
  fi

  check B13 "sha256 mismatch" "the same mirror's q05a.tar declared with q05b.tar's sha256 is fetched and refused: model_sources[].state failed with a reason, and the deployment never starts"
  dep_vllm_small q05-bad "{source: {http: {url: \"http://127.0.0.1:$HTTP_PORT/q05a.tar\", sha256: $sha_b, archive: tar}}}"
  if deploy q05-bad; then
    st=$(poll_sources q05-bad 900)
    status_json q05-bad "$RES/b13-bad-status.json"
    reason=$(ev get "$RES/b13-bad-status.json" 'model_sources[0].reason')
    [ "$PLAN" = 1 ] || { if [ "$st" = failed ]; then result B13 "sha256 mismatch" pass "state failed, reason ${reason:-none}"
                         else result B13 "sha256 mismatch" fail "state ${st}, reason ${reason:-none}"; fi; }
  else
    [ "$PLAN" = 1 ] || result B13 "sha256 mismatch" fail "deploy refused; see cli.log"
  fi

  check B13 "secret URL" "url_ref secret://q05url, the file <state>/secrets/q05url (0600) holding http://127.0.0.1:$HTTP_PORT/q05b.tar?sig=<marker>: fetched (the server logs the signed GET), verified, Ready, answers; the marker and 'q05b.tar' appear in no status, inspect or effective-config JSON, CLI output, role log or engine log"
  secret="rcsecret$(tag)"
  run mkdir -m 700 -p "$SD/secrets"
  if [ "$PLAN" = 0 ]; then
    (umask 077 && printf 'http://127.0.0.1:%s/q05b.tar?sig=%s\n' "$HTTP_PORT" "$secret" >"$SD/secrets/q05url")
    printf '%s\n' "$secret" >"$WORK/needle-sig"
    printf 'q05b.tar\n' >"$WORK/needle-path"
  else
    show "write <state>/secrets/q05url (umask 077): http://127.0.0.1:$HTTP_PORT/q05b.tar?sig=rcsecret<random>"
  fi
  dep_vllm_small q05-secret "{source: {http: {url_ref: \"secret://q05url\", sha256: $sha_b, archive: tar}}}"
  if deploy q05-secret; then
    st=$(poll_sources q05-secret 900)
    if [ "$st" = verified ] && startd q05-secret secret-url; then
      r=$(helper chat --cred "$CRED" --endpoint "$EP" --model q05-secret --out "$RES/b13-secret-chat.json" 2>>"$RES/cli.log")
      status_json q05-secret "$RES/b13-secret-status.json"
      c status deployment q05-secret >"$RES/b13-secret-status.txt" 2>&1
      c inspect deployment q05-secret --json >"$RES/b13-secret-inspect.json" 2>>"$RES/cli.log"
      c inspect deployment q05-secret --effective-config --json >"$RES/b13-secret-effective.json" 2>>"$RES/cli.log"
      if [ "$PLAN" = 0 ]; then
        local files a1 a2
        # CapyCTL's outputs and its whole role state (ledger, logs, config), the
        # secrets directory itself excepted; the http.server's own log is expected to hold it.
        mapfile -t files < <(find "$RES" -maxdepth 1 -type f \( -name 'b13-secret-*' -o -name 'cli.log' -o -name 'role-s3.log' \); find "$SD" -path "$SD/secrets" -prune -o -type f -print 2>/dev/null)
        a1=$(ev absent --needle-file "$WORK/needle-sig" "${files[@]}")
        a2=$(ev absent --needle-file "$WORK/needle-path" "${files[@]}")
        if [ "${r%% *}" = 200 ] && [ "${a1%%$'\t'*}" = pass ] && [ "${a2%%$'\t'*}" = pass ] &&
           grep -q 'GET /q05b.tar?sig=' "$RES/b13-http-server.log"; then
          result B13 "secret URL" pass "fetched with the signed URL, verified, Ready in $START_S s, request ${r%% *}; marker ${a1#*$'\t'}; path ${a2#*$'\t'}"
        else
          result B13 "secret URL" fail "request ${r:-none}; marker: $a1; path: $a2; signed GET lines $(grep -c 'GET /q05b.tar?sig=' "$RES/b13-http-server.log")"
        fi
      fi
    else
      [ "$PLAN" = 1 ] || result B13 "secret URL" fail "sources $st or start failed; see cli.log"
    fi
    stopd q05-secret
  else
    [ "$PLAN" = 1 ] || result B13 "secret URL" fail "deploy refused; see cli.log"
  fi
  role_down
  stop_http
}
stop_http() {
  if [ -z "$HTTP_PID" ] && [ -f "$WORK/http.pid" ]; then
    # A fresh `remote.sh down`: only the run's own server, by its argv.
    HTTP_PID=$(cat "$WORK/http.pid")
    tr '\0' ' ' <"/proc/$HTTP_PID/cmdline" 2>/dev/null | grep -qF -- "--directory $WORK/www" || HTTP_PID=""
  fi
  [ -n "$HTTP_PID" ] && kill "$HTTP_PID" 2>/dev/null
  rm -f "$WORK/http.pid"
  [ "$PLAN" = 1 ] && show "kill <the run's http.server pid>"
  HTTP_PID=""
}

# B10: Flash-Next with its n-gram table on disk; only with a complete checkpoint.
FN_SG="" FN_TF="" FN_WHY=""
fn_sources() {
  local loc fmt cmp
  FN_SG="" FN_TF="" FN_WHY=""
  if [ "$PLAN" = 1 ]; then
    show "rc.py ckpt ~/models/Qwen3.8-Flash-Next-hibrid48: complete (every shard of its index present at its header's length) and holding ple_embedding.ngram_embedding.shard_* tables?"
    show "else a complete sources-store copy ($FN_SG_HF, $FN_TF_HF) under ~/models/sources/huggingface?"
    show "else --download-flash-next ($DOWNLOAD_FLASH_NEXT): CapyCTL downloads them into ~/models/sources (about 135.3 GB plus the MLX export; 48 GiB more disk for SGLang's table file); else SKIP"
    FN_SG="{hf: $FN_SG_HF}" FN_TF="{hf: $FN_TF_HF}"
    return 0
  fi
  loc=$(ev ckpt "$FN_LOCAL")
  printf '%s\n' "$loc" >"$RES/b10-local-checkpoint.json"
  cmp=$(ev get "$RES/b10-local-checkpoint.json" complete)
  fmt=$(ev get "$RES/b10-local-checkpoint.json" format)
  # Complete, and holding a Flash-Next-sized table (47.7 GiB NVFP4, 29.8 GiB MLX): not a partial copy.
  if [ "$cmp" = true ] && [ "$(ev get "$RES/b10-local-checkpoint.json" table_bytes)" -gt $((20 << 30)) ] 2>/dev/null; then
    [ "$fmt" = mlx ] && FN_TF=$FN_LOCAL || FN_SG=$FN_LOCAL
  fi
  FN_WHY="the host copy ~/models/Qwen3.8-Flash-Next-hibrid48: $(ev get "$RES/b10-local-checkpoint.json" gib) GiB in $(ev get "$RES/b10-local-checkpoint.json" files) files, $(ev get "$RES/b10-local-checkpoint.json" missing_count) shards of its index missing, complete $cmp, table $(ev get "$RES/b10-local-checkpoint.json" table_gib) GiB (the full checkpoint is 125.9 GiB with a 47.7 GiB table)"
  # A copy CapyCTL already downloaded (its sources store keeps verified files only).
  [ -z "$FN_SG" ] && [ "$(ev ckpt "$FN_SG_STORE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["complete"])' 2>/dev/null)" = True ] && FN_SG="{hf: $FN_SG_HF}"
  [ -z "$FN_TF" ] && [ "$(ev ckpt "$FN_TF_STORE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["complete"])' 2>/dev/null)" = True ] && FN_TF="{hf: $FN_TF_HF}"
  if [ "$DOWNLOAD_FLASH_NEXT" = 1 ]; then
    local free
    free=$(df -B1G --output=avail "$MODELS" 2>/dev/null | tail -1 | tr -d ' ')
    if [ "${free:-0}" -lt 250 ]; then
      FN_WHY="$FN_WHY; --download-flash-next needs about 250 GiB free under ~/models (both checkpoints and SGLang's table file), ${free:-?} GiB free"
    else
      [ -z "$FN_SG" ] && FN_SG="{hf: $FN_SG_HF}"
      [ -z "$FN_TF" ] && FN_TF="{hf: $FN_TF_HF}"
    fi
  else
    FN_WHY="$FN_WHY; no complete sources-store copy, and --download-flash-next not given"
  fi
}

step_b10() {
  want B10 || return 0
  phase "B10 Qwen3.8-Flash-Next with the n-gram table on disk: own role (managed 110GiB, reserve 8GiB), SGLang profile without MAX_JOBS and with empty kernel caches"
  fn_sources
  if [ -z "$FN_SG" ] && [ -z "$FN_TF" ]; then skip B10 "Flash-Next" "$FN_WHY"; return 0; fi
  role_up s4 --set "$SYS.managed_limit=110GiB" --set "$SYS.free_reserve=8GiB" --set "$SYS.parked_limit=45GiB" ||
    { fail_ids "role" "standalone did not become ready" B10; role_down; return; }
  if [ -n "$FN_SG" ]; then b10_sglang; else skip B10 SGLang "$FN_WHY"; fi
  if [ -n "$FN_TF" ]; then b10_tensorfold; else skip B10 TensorFold "$FN_WHY"; fi
  role_down
}

b10_sglang() {
  local st jobs id logs tb wb cw r ok
  run mkdir -p "$WORK/ple"
  profile s21fn sglang "$S21" sglang-fn --approve-option=--ple-offload-dir --approve-path "$WORK/ple" ||
    { fail_ids "SGLang" "engine add failed" B10; return; }
  yaml fn-sg <<EOF
schema_version: 1
kind: deployment
name: fn-sg
engine: s21fn
model: $FN_SG
# The catalog recipe (capyctl-recipes sglang/qwen3.8-flash-next-nvfp4-gb10) with
# this run's table directory, a 2700 s first-start bound (the recipe measured
# 1745 s), and no MAX_JOBS anywhere: CapyCTL derives it.
residency: restart_only
timeouts: {initialize: 2700s}
engine_config:
  quantization: modelopt_fp4
  context_length: 262144
  max_concurrent_requests: 8
  memory: {startup: 108GiB}
  sglang: {reasoning_parser: qwen3, chunked_prefill_size: 4096}
  accept_extra_args: true
  extra_args: [--fp4-gemm-backend, flashinfer_cutlass, --ple-offload-embedding, --ple-offload-backend, file,
    --ple-offload-dir, $WORK/ple, --page-size, "64", --speculative-algorithm, NEXTN, --speculative-num-steps, "3",
    --speculative-eagle-topk, "1", --speculative-num-draft-tokens, "4", --max-mamba-cache-size, "40"]
EOF
  deploy fn-sg || { fail_ids "SGLang" "deploy refused; see cli.log" B10; return; }
  st=$(poll_sources fn-sg 14400)
  check B10 "SGLang MAX_JOBS" "with empty kernel caches and no MAX_JOBS set, the role log's 'engine build limits: MAX_JOBS=N (MemAvailable ... less the ... Ready charge on unified memory ...)' has N <= 2, and the first start becomes Ready without an out-of-memory line in the engine log"
  if ! startd fn-sg first-start; then
    jobs=$(grep -o 'engine build limits: MAX_JOBS=[0-9]*[^"]*' "$RES/role-s4.log" 2>/dev/null | tail -1)
    fail_ids "SGLang" "first start failed (sources $st; ${jobs:-no MAX_JOBS line}); see cli.log" B10
    stopd fn-sg
    return
  fi
  id=$(dep_id fn-sg)
  mapfile -t logs < <(launch_logs "$id")
  ev facts "${logs[@]}" --out "$RES/b10-sglang-facts.json" >/dev/null
  if [ "$PLAN" = 0 ]; then
    jobs=$(grep -o 'engine build limits: MAX_JOBS=[0-9]*[^"]*' "$RES/role-s4.log" | tail -1)
    ok=fail
    [[ $jobs =~ MAX_JOBS=([12])\ \(.*Ready\ charge ]] && [ "$(ev get "$RES/b10-sglang-facts.json" out_of_memory_lines)" = 0 ] && ok=pass
    result B10 "SGLang MAX_JOBS" "$ok" "${jobs:-no MAX_JOBS line}; Ready in $START_S s; out-of-memory lines $(ev get "$RES/b10-sglang-facts.json" out_of_memory_lines)"
  fi
  check B10 "SGLang disk tables" "inspect --effective-config: engine_config.memory.disk_tables records the table (tables.bytes about 47.7 GiB) and memory.weights_bytes is the checkpoint less the table plus 8 GiB; started under the 110 GiB managed limit; answers"
  c inspect deployment fn-sg --effective-config --json >"$RES/b10-sglang-effective.json" 2>>"$RES/cli.log"
  status_json fn-sg "$RES/b10-sglang-status.json"
  r=$(helper chat --cred "$CRED" --endpoint "$EP" --model fn-sg --prompt "What is 17+25? Answer with only the number." --out "$RES/b10-sglang-chat.json" 2>>"$RES/cli.log")
  if [ "$PLAN" = 0 ]; then
    tb=$(ev get "$RES/b10-sglang-effective.json" 'engine_config.memory.disk_tables.tables.bytes')
    cw=$(ev get "$RES/b10-sglang-effective.json" 'engine_config.memory.disk_tables.checkpoint_weights_bytes')
    wb=$(ev get "$RES/b10-sglang-effective.json" 'engine_config.memory.weights_bytes')
    ok=fail
    [ "${tb:-0}" -gt $((40 << 30)) ] && [ "${wb:-0}" -gt 0 ] && [ "${wb:-0}" -lt "${cw:-0}" ] && [ "${r%% *}" = 200 ] && ok=pass
    result B10 "SGLang disk tables" "$ok" "table $(gib "$tb") GiB, checkpoint $(gib "$cw") GiB, memory weights $(gib "$wb") GiB; request ${r:-none}"
  fi
  stopd fn-sg
}

b10_tensorfold() {
  local st tb wb cw r ok
  profile tf66fn tensorfold "$TF66" tensorfold || { fail_ids "TensorFold" "engine add failed" B10; return; }
  yaml fn-tf <<EOF
schema_version: 1
kind: deployment
name: fn-tf
engine: tf66fn
model: $FN_TF
# The catalog recipe (capyctl-recipes tensorfold/qwen3.8-flash-next-mlx-4bit-gb10) on 0.6.6.
residency: restart_only
timeouts: {initialize: 2700s}
$(phases_yaml 94GiB 94GiB)
engine_config:
  context_length: 262144
  max_concurrent_requests: 8
  accept_extra_args: true
  extra_args: [--ple-on-ssd]
EOF
  deploy fn-tf || { fail_ids "TensorFold" "deploy refused; see cli.log" B10; return; }
  st=$(poll_sources fn-tf 14400)
  check B10 "TensorFold disk tables" "inspect --effective-config: engine_config.memory.disk_tables records the table and memory.weights_bytes is the checkpoint less the table plus 1 GiB; Ready under the 110 GiB managed limit; answers"
  startd fn-tf first-start || { fail_ids "TensorFold" "start failed (sources $st); see cli.log" B10; stopd fn-tf; return; }
  c inspect deployment fn-tf --effective-config --json >"$RES/b10-tf-effective.json" 2>>"$RES/cli.log"
  r=$(helper chat --cred "$CRED" --endpoint "$EP" --model fn-tf --prompt "What is 17+25? Answer with only the number." --out "$RES/b10-tf-chat.json" 2>>"$RES/cli.log")
  if [ "$PLAN" = 0 ]; then
    tb=$(ev get "$RES/b10-tf-effective.json" 'engine_config.memory.disk_tables.tables.bytes')
    cw=$(ev get "$RES/b10-tf-effective.json" 'engine_config.memory.disk_tables.checkpoint_weights_bytes')
    wb=$(ev get "$RES/b10-tf-effective.json" 'engine_config.memory.weights_bytes')
    ok=fail
    [ "${tb:-0}" -gt 0 ] && [ "${wb:-0}" -gt 0 ] && [ "${wb:-0}" -lt "${cw:-0}" ] && [ "${r%% *}" = 200 ] && ok=pass
    result B10 "TensorFold disk tables" "$ok" "table $(gib "$tb") GiB, checkpoint $(gib "$cw") GiB, memory weights $(gib "$wb") GiB; Ready in $START_S s; request ${r:-none}"
  fi
  stopd fn-tf
}

# B17: a standalone role as PID 1 of a container (docs/operations/install.md,
# "Running in a container"): the binary in an image, the venv, its interpreter,
# the models and CUDA mounted read-only, the state directory read-write.
step_b17() {
  want B17 || return 0
  if [ "$CONTAINER" != 1 ]; then
    skip B17 container "opt-in: needs an image built on the host (ubuntu:24.04 pulled, python3 and gcc installed, the capyctl binary copied in); run with --container"
    return 0
  fi
  phase "B17 container: a standalone role as PID 1, --memory 100g, vLLM 0.30.0 Qwen3-4B"
  if [ "$PLAN" = 0 ] && ! docker info >/dev/null 2>&1; then skip B17 container "docker is not usable by this user on the host"; return 0; fi
  local img=$WORK/img sd=$WORK/s5 py pyroot cuda mounts name r pid1 ms rl st z left ok
  run mkdir -m 700 -p "$sd" "$img"
  run cp "$CAPYCTL" "$img/capyctl"
  if [ "$PLAN" = 0 ]; then
    cat >"$img/Dockerfile" <<'EOF'
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends python3 gcc g++ libc6-dev procps ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY capyctl /usr/local/bin/capyctl
ENTRYPOINT ["/usr/local/bin/capyctl"]
EOF
  else
    show "write img/Dockerfile: FROM ubuntu:24.04; apt-get install python3 gcc g++ libc6-dev procps ca-certificates; COPY capyctl /usr/local/bin/capyctl; ENTRYPOINT capyctl"
  fi
  run docker build -t "$IMAGE" "$img" >>"$RES/docker.log" 2>&1
  py=$(readlink -f "$V30/bin/python" 2>/dev/null)
  pyroot=${py%/bin/*}
  cuda=$(readlink -f /usr/local/cuda 2>/dev/null)
  mounts=(-v "$sd:$sd" -v "$MODELS:$MODELS:ro" -v "$V30:$V30:ro")
  [ -n "$pyroot" ] && [ "$pyroot" != /usr ] && mounts+=(-v "$pyroot:$pyroot:ro")
  [ -n "$cuda" ] && [ -d "$cuda" ] && mounts+=(-v "$cuda:$cuda:ro" -v "$cuda:/usr/local/cuda:ro")
  run docker run -d --name "$CONTAINER_NAME" --gpus all --ipc=host --network host --memory 100g \
    --user "$(id -u):$(id -g)" "${mounts[@]}" -e "CAPYCTL_STATE_DIR=$sd" -e "HOME=$sd" \
    -e "CAPYCTL_MODELS_ROOT=$MODELS" -e CAPYCTL_MODEL_SOURCES=disabled "$IMAGE" \
    start standalone --listen "127.0.0.1:$CPORT" --management-listen "127.0.0.1:$((CPORT + 1))" \
    --engine-ports "$C_ENGINE_PORTS" --json >>"$RES/docker.log" 2>&1
  if [ "$PLAN" = 1 ]; then show "wait for '\"ready\":true' in docker logs (180 s)"
  else
    for _ in $(seq 180); do docker logs "$CONTAINER_NAME" 2>&1 | grep -q '"ready":true' && break; sleep 1; done
  fi
  d() { run docker exec "$CONTAINER_NAME" capyctl "$@"; }
  check B17 "PID 1" "the container's /proc/1/cmdline is capyctl start standalone (no init)"
  if [ "$PLAN" = 0 ]; then
    pid1=$(docker exec "$CONTAINER_NAME" cat /proc/1/cmdline 2>/dev/null | tr '\0' ' ')
    if [[ $pid1 == *capyctl*standalone* ]]; then result B17 "PID 1" pass "PID 1: ${pid1/#$HOME/~}"; else result B17 "PID 1" fail "PID 1: '${pid1}'"; fi
  fi
  check B17 "memory source" "inspect host --json: session.domains[].memory_source starts cgroup_v2: and the role log says 'Host memory is bounded by the cgroup v2 memory.max'"
  d list hosts --json >"$RES/b17-hosts.json" 2>>"$RES/cli.log"
  name=$(ev get "$RES/b17-hosts.json" 'hosts[0].name')
  d inspect host "${name:-<host name>}" --json >"$RES/b17-host.json" 2>>"$RES/cli.log"
  if [ "$PLAN" = 0 ]; then
    ms=$(ev get "$RES/b17-host.json" memory_source)
    rl=$(docker logs "$CONTAINER_NAME" 2>&1 | grep -c 'bounded by the cgroup v2 memory.max')
    if [[ $ms == cgroup_v2:* ]] && [ "$rl" -gt 0 ]; then result B17 "memory source" pass "memory_source $ms; role log line present"
    else result B17 "memory source" fail "memory_source '${ms}', role log lines $rl"; fi
  fi
  check B17 serve "engine add, deploy and start --wait a vLLM 0.30.0 Qwen3-4B deployment inside the container; a request through 127.0.0.1:$CPORT answers 42; stop leaves it stopped with no engine process and no zombie in the container"
  d engine add "$V30" --name v30c >>"$RES/cli.log" 2>&1
  dep_vllm_small c-4b "$Q4B" v30c
  run cp "$WORK/dep/c-4b.yaml" "$sd/c-4b.yaml"
  d deploy model --file "$sd/c-4b.yaml" >>"$RES/cli.log" 2>&1
  ok=fail
  if d start deployment c-4b --wait >>"$RES/cli.log" 2>&1; then
    r=$(helper chat --cred "$sd/identity/credentials" --endpoint "http://127.0.0.1:$CPORT" --model c-4b --out "$RES/b17-chat.json" 2>>"$RES/cli.log")
    d stop deployment c-4b >>"$RES/cli.log" 2>&1
    if [ "$PLAN" = 0 ]; then
      for _ in $(seq 300); do
        docker exec "$CONTAINER_NAME" capyctl status deployment c-4b --json >"$RES/b17-status.json" 2>/dev/null
        st=$(ev get "$RES/b17-status.json" observed_state)
        [ "$st" = stopped ] && break
        sleep 1
      done
      sleep 5
      docker exec "$CONTAINER_NAME" ps -eo pid=,stat=,comm= >"$RES/b17-ps.txt" 2>&1
      z=$(awk '$2 ~ /^Z/' "$RES/b17-ps.txt" | wc -l)
      left=$(grep -ciE 'python|vllm|VLLM' "$RES/b17-ps.txt")
      [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ] && [ "$st" = stopped ] && [ "$z" = 0 ] && [ "$left" = 0 ] && ok=pass
      result B17 serve "$ok" "request ${r:-none}; after stop: state $st, zombies $z, engine processes $left ($(wc -l <"$RES/b17-ps.txt") processes in the container)"
    fi
  else
    [ "$PLAN" = 1 ] || result B17 serve fail "start --wait failed inside the container; see cli.log and docker.log"
  fi
  container_down
}
container_down() {
  if [ "$PLAN" = 1 ]; then
    show "\$ docker exec $CONTAINER_NAME capyctl delete deployment c-4b --stop; capyctl engine remove v30c"
    show "\$ docker stop -t 60 $CONTAINER_NAME (SIGTERM to PID 1: the role drains); docker logs >res/role-s5.log; docker rm; docker rmi $IMAGE"
    return 0
  fi
  docker inspect "$CONTAINER_NAME" >/dev/null 2>&1 || return 0
  docker exec "$CONTAINER_NAME" capyctl delete deployment c-4b --stop >>"$RES/cli.log" 2>&1
  docker exec "$CONTAINER_NAME" capyctl engine remove v30c >>"$RES/cli.log" 2>&1
  docker stop -t 60 "$CONTAINER_NAME" >>"$RES/docker.log" 2>&1
  docker logs "$CONTAINER_NAME" >"$RES/role-s5.log" 2>&1
  docker rm "$CONTAINER_NAME" >>"$RES/docker.log" 2>&1
  docker rmi "$IMAGE" >>"$RES/docker.log" 2>&1
}

down() {
  role_down
  stop_http
  [ "$CONTAINER" = 1 ] && container_down
  return 0
}

finish() { local rc=$?; down; echo "$rc" >"$WORK/done"; }

phases() {
  preflight || exit 1
  record_versions
  step_b7
  if want B1 B2 B3 B5 B6 B8 B9 B11 B12 B13 B14 B15; then
    phase "main role: managed limit 100GiB, free reserve 10GiB, parked limit 45GiB, this run's own model-sources directory"
    if role_up s1 --model-sources-path "$WORK/src1" --set "$SYS.managed_limit=100GiB" \
      --set "$SYS.free_reserve=10GiB" --set "$SYS.parked_limit=45GiB"; then
      step_vllm_default
      step_vllm_lazy
      step_vllm_salt
      step_tf
      step_sglang_4b
      step_sglang_explicit
      step_sglang_27b
      step_draft
      step_q05
    else
      fail_ids "role" "standalone did not become ready" B1 B2 B3 B5 B6 B8 B9 B11 B12 B13 B14 B15
    fi
    role_down
  fi
  if want B4; then
    phase "B4 role: the main role's memory policy, resource_policy.parked_growth_limit 0B"
    if role_up s4 --set "$SYS.managed_limit=100GiB" --set "$SYS.free_reserve=10GiB" --set "$SYS.parked_limit=45GiB" \
      --set "host.resource_policy.parked_growth_limit=0B"; then
      step_vllm_eager
    else
      fail_ids "role" "standalone did not become ready" B4
    fi
    role_down
  fi
  step_b13
  step_b10
  step_b17
  want B16 && skip B16 GB10 "no gpt-oss-120b on the host (the check needs vLLM gpt-oss-120b, kv 8 GiB, startup 90 GiB, managed limit 96 GiB)"
  say "phases done"
}

case $cmd in
  run)
    trap finish EXIT
    : >"$RES/results.tsv"
    say "0.1.3 release checks, run $RID, checks: $ONLY"
    phases ;;
  plan)
    printf 'Host phases (remote.sh run, detached on the host; checks: %s):\n' "$ONLY"
    phases
    printf '\n  cleanup on exit (also after a failure): the current role down, the HTTP server stopped, the container removed\n'
    ;;
  down)
    # From a fresh process (run.sh interrupted): the role named in role.tag, every
    # deployment this run wrote a file for, every profile name it uses.
    if [ -f "$WORK/role.tag" ]; then
      role_vars "$(cat "$WORK/role.tag")"
      DEPLOYS=$(find "$WORK/dep" -maxdepth 1 -name '*.yaml' -printf '%f\n' 2>/dev/null | sed 's/\.yaml$//' | tr '\n' ' ')
      PROFILES="v30 tf66 s21 s21fn tf66fn"
    fi
    down ;;
  purge) cd / && rm -rf -- "$WORK" ;;
  *) sed -n '2,15p' "$0"; exit 2 ;;
esac
