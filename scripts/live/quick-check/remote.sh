#!/usr/bin/env bash
# Host side of scripts/live/engine-quick-check.sh: model presets, engine venvs,
# the CapyCTL lifecycle checks and the benchmark passes, on one GB10 host.
#
#   remote.sh preset   print the preset's defaults for ENGINE and MODEL (runs anywhere)
#   remote.sh run      every phase, in $WORK, from the settings in $WORK/qc.env
#   remote.sh down     stop and delete what `run` created (also run on exit)
#   remote.sh purge    remove $WORK (after the driver copied the results back)
#
# Everything runs through one CapyCTL standalone role with its own state and
# config directories and its own port, so an existing CapyCTL install on the host
# is not touched. Only the role this script started is ever signalled.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
cmd=${1:-}

# ---------------------------------------------------------------------------
# Presets: model x engine. Settings follow the capyctl-recipes GB10 recipes and
# the 0.6.x quick checks (greedy, 8 decoded together, stated KV for vLLM/SGLang).
# ---------------------------------------------------------------------------
preset() {
  : "${ENGINE:?ENGINE not set}" "${MODEL:?MODEL not set}"
  DRAFTER_REF="" DRAFTER_NAME="" CTX_SEPARATE=1 CTX_LEN=32768 CTX_LEN_SWEEP=262144
  CONC_DEFAULT=1,4,8 SWEEP_DEFAULT=2k,32k,128k TF_GIB=60 THINKING=0
  case $MODEL in
    qwen38-27b)
      TITLE=Qwen3.8-27B ENGINES="tensorfold vllm sglang"
      MODEL_REF=nvidia/Qwen3.8-27B-NVFP4@482ca0f3832238542f8f5295dde86b5f22711d80
      DRAFTER_REF=z-lab/Qwen3.8-27B-DFlash2@50307d4c4cde6860d4eee73e2547cd786fe8e8a4
      DRAFTER_NAME=Qwen3.8-27B-DFlash2 ;;
    nemotron)
      TITLE="Nemotron 3.5 Lightning" ENGINES=tensorfold
      MODEL_REF=Vontra/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit@d9d758fb83953437f7263256b0d96157e2a348b8
      # TensorFold decodes this family one request at a time on CUDA; built-in MTP.
      CONC_DEFAULT=1 SWEEP_DEFAULT="" TF_GIB=40 THINKING=1 ;;
    qwen3-4b)
      TITLE=Qwen3-4B ENGINES="vllm sglang"
      MODEL_REF=Qwen/Qwen3-4B@1cfa9a7208912126459214e8b04321603b3df60c
      # The model's own 40960-token window: the main deployment serves the sweep.
      CTX_SEPARATE=0 SWEEP_DEFAULT=2k,8k,32k ;;
    *) echo "unknown model preset $MODEL (qwen38-27b, nemotron, qwen3-4b)" >&2; return 2 ;;
  esac
  case " $ENGINES " in *" $ENGINE "*) ;; *)
    echo "no $ENGINE preset for $MODEL (it has: $ENGINES)" >&2; return 2 ;;
  esac
  # A variant without speculative decoding: TensorFold always (--no-drafts also
  # turns off built-in MTP), vLLM and SGLang when the preset has a drafter.
  NODRAFT=0
  if [ "$ENGINE" = tensorfold ] || [ -n "$DRAFTER_REF" ]; then NODRAFT=1; fi
  case $ENGINE in
    tensorfold) SHORT=tf APPROVE=--drafter ;;
    vllm) SHORT=vllm APPROVE=--speculative-config ;;
    sglang) SHORT=sglang APPROVE=--speculative-draft-model-path ;;
    *) echo "unknown engine $ENGINE (tensorfold, vllm, sglang)" >&2; return 2 ;;
  esac
}

# Deployment file for one version. variant: main, ctx or nodraft.
deployment() { # name profile variant
  local name=$1 prof=$2 var=$3 ctx=$CTX_LEN drafter="$HOME/drafters/$DRAFTER_NAME"
  [ "$var" = ctx ] && ctx=$CTX_LEN_SWEEP
  printf 'schema_version: 1\nkind: deployment\nname: %s\nengine: %s\nmodel: {hf: %s}\n' "$name" "$prof" "$MODEL_REF"
  case $ENGINE in
    tensorfold)
      local cold=$((TF_GIB))GiB ready=$((TF_GIB - 2))GiB phase args
      printf 'residency: restart_only\ndevices: [{id: gpu0}]\nresources:\n'
      for phase in cold:$cold ready:$ready parking:$ready parked:0B wake:$cold; do
        printf '  %s:\n    allocations: [{domain: unified, bytes: %s, host_kv_bytes: 0B}]\n' "${phase%%:*}" "${phase#*:}"
        if [ "${phase%%:*}" = parked ]; then printf '    devices: []\n'; else printf '    devices: [{id: gpu0}]\n'; fi
      done
      printf 'engine_config:\n  context_length: %s\n  kv_cache_dtype: bf16\n' "$ctx"
      [ "$THINKING" = 1 ] && printf '  tensorfold:\n    thinking: true\n'
      args='--parallel, "8", --temperature, "0"'
      if [ "$var" = nodraft ]; then args="--no-drafts, $args"
      elif [ -n "$DRAFTER_REF" ]; then args="--drafter, $drafter, $args"; fi
      printf '  accept_extra_args: true\n  extra_args: [%s]\n' "$args" ;;
    vllm)
      printf 'timeouts: {initialize: 900s}\n'
      if [ "$MODEL" = qwen38-27b ]; then
        printf 'engine_config:\n  kv_cache_dtype: fp8\n  language_model_only: true\n  context_length: %s\n' "$ctx"
        printf '  max_concurrent_requests: 8\n  memory: {request: 48GiB, kv_cache: 16GiB}\n'
        [ "$var" != nodraft ] && printf '  accept_extra_args: true\n  extra_args:\n    - --speculative-config\n    - %s\n' \
          "'{\"method\":\"dflash\",\"model\":\"$drafter\",\"num_speculative_tokens\":7}'"
      fi ;;
    sglang)
      printf 'timeouts: {initialize: 900s}\n'
      if [ "$MODEL" = qwen38-27b ]; then
        printf 'engine_config:\n  quantization: modelopt\n  kv_cache_dtype: fp8_e4m3\n  context_length: %s\n' "$ctx"
        printf '  max_concurrent_requests: 8\n  memory: {request: 65GiB, kv_cache: 16GiB}\n'
        [ "$var" != nodraft ] && printf '  accept_extra_args: true\n  extra_args: [--speculative-algorithm, DFLASH, --speculative-draft-model-path, %s, --speculative-num-draft-tokens, "8", --speculative-draft-model-quantization, unquant]\n' "$drafter"
      fi ;;
  esac
  return 0
}

if [ "$cmd" = preset ]; then
  preset || exit 2
  printf 'TITLE=%q\nCONC_DEFAULT=%q\nSWEEP_DEFAULT=%q\nNODRAFT=%q\nMODEL_REF=%q\nDRAFTER_REF=%q\n' \
    "$TITLE" "$CONC_DEFAULT" "$SWEEP_DEFAULT" "$NODRAFT" "$MODEL_REF" "$DRAFTER_REF"
  exit 0
fi

# ---------------------------------------------------------------------------
# Host phases
# ---------------------------------------------------------------------------
WORK=${WORK:-$HERE}
# shellcheck disable=SC1091
. "$WORK/qc.env"
preset || exit 2
: "${PORT:?qc.env sets PORT}" "${OLD:?}" "${NEW:?}"
SD=$WORK/state
RES=$WORK/res
export XDG_CONFIG_HOME=$WORK/config
CRED=$SD/identity/credentials
CAPYCTL=${CAPYCTL_BIN:-capyctl}
UV=$HOME/.local/bin/uv; [ -x "$UV" ] || UV=uv
VENV_OLD=$HOME/$ENGINE-$OLD-venv VENV_NEW=$HOME/$ENGINE-$NEW-venv
P_OLD=$SHORT${OLD//./} P_NEW=$SHORT${NEW//./}
PIDFILE=$WORK/role.pid
mkdir -p "$SD" "$XDG_CONFIG_HOME" "$RES" "$WORK/deployments"
chmod 700 "$WORK" "$SD" "$XDG_CONFIG_HOME" "$RES"

c() { "$CAPYCTL" --state-dir "$SD" "$@"; }
say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RES/progress.log"; }
rec() { # step result detail
  printf '%s\t%s\t%s\n' "$1" "$2" "$3" >>"$RES/lifecycle.tsv"; say "[$2] $1: $3"; }
ver() { if [ "$1" = old ]; then echo "$OLD"; else echo "$NEW"; fi; }
prof() { if [ "$1" = old ]; then echo "$P_OLD"; else echo "$P_NEW"; fi; }
venv() { if [ "$1" = old ]; then echo "$VENV_OLD"; else echo "$VENV_NEW"; fi; }
dname() { echo "qc-$1-$(prof "$2")"; } # variant side
# Engine processes of this run: argv names one of the two venvs.
engine_re() { printf '%s/bin/|%s/bin/' "$VENV_OLD" "$VENV_NEW"; }
engine_port() { pgrep -af -- "$(engine_re)" | sed -n 's/.*--port[ =]\([0-9][0-9]*\).*/\1/p' | head -1; }
waitgone() { local _; for _ in $(seq 600); do pgrep -f -- "$(engine_re)" >/dev/null || return 0; sleep 1; done; return 1; }
health_code() { local p; p=$(engine_port); [ -n "$p" ] || { echo none; return; }
  curl -s -m 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$p/health"; }
healthpoll() { while :; do echo "$(date +%s.%N) $(health_code)"; sleep 0.5; done >>"$1"; }
elapsed() { printf '%.1f' "$(echo "$(date +%s.%N) - $1" | bc)"; }

startd() { # name label [start options]
  local n=$1 l=$2 s; shift 2; s=$(date +%s.%N)
  if c start deployment "$n" --wait "$@" >>"$RES/cli.log" 2>&1; then
    echo "$n $l $(elapsed "$s")" >>"$RES/startup.txt"; say "started $n ($l) in $(elapsed "$s") s"; return 0
  fi
  say "start $n ($l) failed; see cli.log"; return 1
}
stopd() { c stop deployment "$1" >>"$RES/cli.log" 2>&1; waitgone; sleep 5; }
stopall() { local d; for d in $DEPLOYMENTS; do c stop deployment "$d" >/dev/null 2>&1; done; waitgone; sleep 5; }

chat() { # model -> prints "HTTP seconds"; reply in $RES/chat.json
  local key s code; key=$(sed -n 's/^api_key: //p' "$CRED"); s=$(date +%s.%N)
  code=$(curl -s -m 1800 -o "$RES/chat.json" -w '%{http_code}' -H "Authorization: Bearer $key" \
    -H 'Content-Type: application/json' "http://127.0.0.1:$PORT/v1/chat/completions" \
    -d "{\"model\":\"$1\",\"messages\":[{\"role\":\"user\",\"content\":\"What is 17+25? Answer with only the number.\"}],\"max_tokens\":2048,\"temperature\":0}")
  echo "$code $(elapsed "$s")"
}
chat_ok() { python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); m=((d.get("choices") or [{}])[0].get("message") or {})
sys.exit(0 if "42" in (m.get("content") or "") else 1)' "$RES/chat.json" 2>/dev/null; }

MEM="awk '/^MemTotal:/ {t=\$2} /^MemAvailable:/ {a=\$2} END {print (t-a)/1048576}' /proc/meminfo"
bench() { # side out; rest: run options
  local side=$1 out=$2; shift 2
  local drafter=${DRAFTER_REF:-none}
  [ "$MODEL" = nemotron ] && drafter="none (built-in MTP)"
  python3 "$WORK/capyctl-bench/capyctl_bench.py" run --endpoint "http://127.0.0.1:$PORT/v1" \
    --api-key-file "$CRED" --label "$ENGINE_LABEL $(ver "$side")" --memory-cmd "$MEM" \
    --meta gpu="$GPU" --meta engine="$ENGINE" --meta engine_version="$(ver "$side")" \
    --meta capyctl="$CAPYCTL_COMMIT" --meta model="$MODEL_REF" --meta drafter="$drafter" \
    --meta parallel=8 --out "$out" "$@" >>"$RES/bench.log" 2>&1
}

install_venv() { # side
  local v d py
  v=$(ver "$1") d=$(venv "$1") py=$(venv "$1")/bin/python
  if [ -x "$py" ]; then say "venv $(basename "$d") exists, kept"; return 0; fi
  say "creating $(basename "$d")"
  "$UV" venv --python 3.12 --seed --managed-python "$d" || return 1
  case $ENGINE in
    tensorfold)
      "$UV" pip install --python "$py" "torch==2.13.0" triton --index-url https://download.pytorch.org/whl/cu130 &&
        "$UV" pip install --python "$py" "tensorfold @ git+https://github.com/ashhart/TensorFold.git@v$v" ninja || return 1
      if [ ! -x /usr/local/cuda/bin/nvcc ]; then
        "$UV" pip install --python "$py" "cuda-toolkit[nvcc,cccl]==13.0.*" || return 1
      fi ;;
    vllm) "$UV" pip install --python "$py" "vllm==$v" --torch-backend=cu130 || return 1 ;;
    sglang) "$UV" pip install --python "$py" --prerelease=allow "sglang==$v" || return 1 ;;
  esac
}

engine_version() { # side -> the version the venv reports
  local d; d=$(venv "$1")
  case $ENGINE in
    tensorfold) "$d/bin/tensorfold" --version 2>/dev/null | awk '{print $NF}' ;;
    vllm) "$d/bin/python" -c 'import vllm; print(vllm.__version__)' 2>/dev/null ;;
    sglang) "$d/bin/python" -c 'import sglang; print(sglang.__version__)' 2>/dev/null ;;
  esac
}

record_versions() {
  local side d
  {
    echo "capyctl_version=$(c --version 2>/dev/null | head -1)"
    echo "capyctl_commit=$CAPYCTL_COMMIT"
    for side in old new; do
      d=$(venv "$side")
      echo "${side}_reported=$(engine_version "$side")"
      echo "${side}_venv=~/$(basename "$d")"
      echo "${side}_python=$("$d/bin/python" -c 'import platform; print(platform.python_version())' 2>/dev/null)"
      echo "${side}_torch=$("$d/bin/python" -c 'import torch; print(torch.__version__)' 2>/dev/null)"
      echo "${side}_triton=$("$d/bin/python" -c 'import triton; print(triton.__version__)' 2>/dev/null)"
      echo "${side}_pkg=$("$UV" pip show --python "$d/bin/python" "$ENGINE" 2>/dev/null | sed -n 's/^Version: //p')"
      echo "${side}_source=$("$UV" pip freeze --python "$d/bin/python" 2>/dev/null | grep -i "^$ENGINE" | head -1)"
    done
    echo "gpu=$GPU"
    echo "driver=$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1)"
    echo "cuda=$(/usr/local/cuda/bin/nvcc --version 2>/dev/null | sed -n 's/.*release \([0-9.]*\).*/\1/p')"
    echo "kernel=$(uname -r)"
    echo "bench_python=$(python3 -c 'import platform; print(platform.python_version())')"
  } >"$RES/versions.txt"
}

setup() {
  local side opts
  for side in old new; do
    opts=()
    [ -n "$DRAFTER_REF" ] && opts=(--approve-option="$APPROVE" --approve-path "$HOME/drafters")
    if c engine add "$(venv "$side")" --name "$(prof "$side")" "${opts[@]}" >>"$RES/cli.log" 2>&1; then
      rec "engine add $(ver "$side")" pass "profile $(prof "$side")"
    else rec "engine add $(ver "$side")" fail "see cli.log"; return 1; fi
  done
  setsid nohup "$CAPYCTL" --state-dir "$SD" start standalone --listen "127.0.0.1:$PORT" --json \
    >"$RES/role.log" 2>&1 </dev/null &
  echo $! >"$PIDFILE"
  local _; for _ in $(seq 180); do grep -q '"ready":true' "$RES/role.log" 2>/dev/null && break; sleep 1; done
  if ! grep -q '"ready":true' "$RES/role.log"; then rec "standalone ready" fail "no ready line in 180 s"; return 1; fi
  rec "standalone ready" pass "own state dir, 127.0.0.1:$PORT"
  local ok=1 n
  for side in old new; do
    for var in main $([ "$CTX_SEPARATE" = 1 ] && [ -n "$SWEEP" ] && echo ctx) \
               $([ "$NODRAFT" = 1 ] && [ "$side" = new ] && echo nodraft); do
      n=$(dname "$var" "$side")
      deployment "$n" "$(prof "$side")" "$var" >"$WORK/deployments/$n.yaml"
      if c deploy model --file "$WORK/deployments/$n.yaml" >>"$RES/cli.log" 2>&1; then
        DEPLOYMENTS="$DEPLOYMENTS $n"; echo "$n" >>"$WORK/deployments.txt"
      else ok=0; say "deploy $n failed"; fi
    done
  done
  if [ $ok = 1 ]; then rec "deploy model" pass "$(echo "$DEPLOYMENTS" | wc -w) deployments"
  else rec "deploy model" fail "see cli.log"; return 1; fi
}

cold() { # side: first start of a version in this state dir, one C1 probe, stop
  local n; n=$(dname main "$1")
  if startd "$n" "cold-$1"; then
    rec "start cold $(ver "$1")" pass "$(tail -1 "$RES/startup.txt" | awk '{print $3}') s"
    bench "$1" "$RES/probe-$1.json" --model "$n" --concurrency 1 --rounds 1 --warmup 0 ||
      say "probe $1 failed; see bench.log"
    stopd "$n"
  else rec "start cold $(ver "$1")" fail "see cli.log"; fi
}

life() { # new version: warm start, status, health, reasoning stream, stop, request after stop
  local n r code; n=$(dname main new)
  startd "$n" warm-life || { rec "start warm" fail "see cli.log"; return; }
  rec "start warm" pass "$(tail -1 "$RES/startup.txt" | awk '{print $3}') s"
  c status deployment "$n" >"$RES/status-life.txt" 2>&1
  code=$(health_code)
  if [ "$code" = 200 ]; then rec "engine /health" pass "200"; else rec "engine /health" fail "$code"; fi
  python3 "$HERE/stream_check.py" "$CRED" "$n" 2048 "http://127.0.0.1:$PORT" >"$RES/stream-new.json" 2>>"$RES/cli.log"
  r=$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); u=bool(d["usage_chunks"])
ok=d.get("http_status")==200 and d["done"] and u and d["answer_ok"]
print(("pass" if ok else "fail")+"\t%d reasoning chunks, %d content chunks, finish %s, usage %s, answer %s, first token %s s"%(
 d["reasoning_chunks"],d["content_chunks"],d["finish_reason"],"yes" if u else "no","ok" if d["answer_ok"] else "wrong",d["first_token_s"]))' \
    "$RES/stream-new.json" 2>/dev/null) || r=$'fail\tno stream result'
  rec "streaming reasoning request" "${r%%$'\t'*}" "${r#*$'\t'}"
  stopd "$n"
  if c status deployment "$n" 2>/dev/null | grep -qi stopped; then rec "stop" pass "engine gone, stopped"
  else rec "stop" fail "state not stopped"; fi
  r=$(chat "$n")
  if [ "${r%% *}" = 409 ]; then rec "request after stop" pass "409 at once (${r#* } s)"
  else rec "request after stop" fail "HTTP ${r%% *}, expected 409"; fi
}

wake() { # new ready, old started (with --evict when both do not fit), then a request for new
  local a b r s; a=$(dname main new) b=$(dname main old)
  startd "$a" warm-wake || { rec "wake on request" fail "start failed"; return; }
  if c start deployment "$b" --wait >>"$RES/cli.log" 2>&1; then
    rec "wake on request" n/a "both deployments fit together; no pressure stop"
  else
    s=$(date +%s.%N)
    if c start deployment "$b" --wait --evict >>"$RES/cli.log" 2>&1; then
      rec "start --evict" pass "old ready in $(elapsed "$s") s after the plain start was refused"
      c list deployments >>"$RES/cli.log" 2>&1
      r=$(chat "$a"); cp "$RES/chat.json" "$RES/wake.json"
      if [ "${r%% *}" = 200 ] && chat_ok; then rec "wake on request" pass "request for new switched and answered in ${r#* } s"
      else rec "wake on request" fail "HTTP ${r%% *} in ${r#* } s"; fi
    else rec "start --evict" fail "see cli.log"; fi
  fi
  stopall
}

nodraft() {
  [ "$NODRAFT" = 1 ] || return 0
  local n r; n=$(dname nodraft new)
  startd "$n" nodraft || { rec "no-drafts variant" fail "start failed"; return; }
  r=$(chat "$n")
  if [ "${r%% *}" = 200 ] && chat_ok; then rec "no-drafts variant" pass "ready, answered in ${r#* } s"
  else rec "no-drafts variant" fail "HTTP ${r%% *}"; fi
  stopd "$n"
}

metrics() { # engine /metrics and /health on the loopback port CapyCTL gave it, no key
  local n port code lines; n=$(dname main new)
  startd "$n" warm-metrics || { rec "engine /metrics" fail "start failed"; return; }
  port=$(engine_port)
  pgrep -af -- "$(engine_re)" | grep -c -- "--api-key" | sed 's/^/api-key options on the engine command line: /' >>"$RES/metrics-notes.txt"
  code=$(curl -s -o "$RES/metrics-new.txt" -w '%{http_code}' "http://127.0.0.1:$port/metrics")
  lines=$(grep -cE "^(tensorfold|vllm|sglang)[:_]" "$RES/metrics-new.txt" 2>/dev/null || true)
  if [ "$code" = 200 ] && [ "${lines:-0}" -gt 0 ]; then rec "engine /metrics" pass "200, $lines engine samples"
  else rec "engine /metrics" fail "HTTP $code, ${lines:-0} engine samples"; fi
  code=$(health_code)
  if [ "$code" = 200 ]; then rec "engine /health under CapyCTL" pass 200; else rec "engine /health under CapyCTL" fail "$code"; fi
  chat "$n" >/dev/null & local req=$!
  sleep 1
  if c status deployment "$n" >"$RES/status-new.txt" 2>&1; then rec "capyctl status during a request" pass "read"
  else rec "capyctl status during a request" fail "see status-new.txt"; fi
  # Only the request: a bare wait would also wait for the role (a child of this shell).
  wait "$req"; stopd "$n"
}

conc() { # side pass
  local n; n=$(dname main "$1")
  startd "$n" "warm-conc-$1-pass$2" || { rec "bench conc $(ver "$1") pass $2" fail "start failed"; return; }
  healthpoll "$RES/health-$1-pass$2.log" & local hp=$!
  if bench "$1" "$RES/conc-$1-pass$2.json" --model "$n" --concurrency "$CONC" --rounds "$ROUNDS" \
    --concurrency-max-tokens 512 --record-chunks; then say "conc $1 pass $2 done"
  else rec "bench conc $(ver "$1") pass $2" fail "see bench.log"; fi
  kill "$hp" 2>/dev/null; wait "$hp" 2>/dev/null; stopd "$n"
}

ctx() { # side
  [ -n "$SWEEP" ] || return 0
  local n; n=$(dname main "$1"); [ "$CTX_SEPARATE" = 1 ] && n=$(dname ctx "$1")
  startd "$n" "warm-ctx-$1" || { rec "bench ctx $(ver "$1")" fail "start failed"; return; }
  bench "$1" "$RES/ctx-$1.json" --model "$n" --record-chunks --context-sweep "$SWEEP" --runs "$RUNS" \
    --max-tokens 128 --meta context="$([ "$CTX_SEPARATE" = 1 ] && echo "$CTX_LEN_SWEEP" || echo model)" ||
    rec "bench ctx $(ver "$1")" fail "see bench.log"
  stopd "$n"
}

down() {
  local d pid
  [ -f "$WORK/deployments.txt" ] && DEPLOYMENTS=$(tr '\n' ' ' <"$WORK/deployments.txt")
  if [ -f "$PIDFILE" ] && pid=$(cat "$PIDFILE") && [ -r "/proc/$pid/cmdline" ] &&
     tr '\0' ' ' <"/proc/$pid/cmdline" | grep -qF -- "$SD"; then
    for d in ${DEPLOYMENTS:-}; do c delete deployment "$d" --stop >>"$RES/cli.log" 2>&1; done
    waitgone
    for d in "$P_OLD" "$P_NEW"; do c engine remove "$d" >>"$RES/cli.log" 2>&1; done
    # A signal is the role's drained shutdown: admission closes, in-flight requests finish.
    kill -TERM "$pid"
    for _ in $(seq 60); do [ -d "/proc/$pid" ] || break; sleep 1; done
    grep -o '"drained":[a-z]*[^}]*' "$RES/role.log" | tail -1 >"$RES/drained.txt"
    say "role stopped: $(cat "$RES/drained.txt")"
  fi
  rm -f "$PIDFILE"
}

finish() { local rc=$?; down; echo "$rc" >"$WORK/done"; }

case $cmd in
  run)
    trap finish EXIT
    DEPLOYMENTS=""
    : >"$RES/lifecycle.tsv"
    GPU=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1 | sed 's/^NVIDIA //')
    case $ENGINE in tensorfold) ENGINE_LABEL=TensorFold ;; vllm) ENGINE_LABEL=vLLM ;; sglang) ENGINE_LABEL=SGLang ;; esac
    say "quick check: $ENGINE $OLD vs $NEW, $MODEL"
    if [ "$SKIP_VENV" != 1 ]; then
      for side in old new; do install_venv "$side" || { rec "venv $(ver "$side")" fail "install failed"; exit 1; }; done
    fi
    for side in old new; do
      [ -x "$(venv "$side")/bin/python" ] || { rec "venv $(ver "$side")" fail "$(basename "$(venv "$side")") missing in the home directory"; exit 1; }
      got=$(engine_version "$side")
      if [ "$got" = "$(ver "$side")" ]; then rec "venv $(ver "$side")" pass "reports $got"
      else rec "venv $(ver "$side")" fail "reports '${got}', expected $(ver "$side")"; fi
    done
    record_versions
    setup || exit 1
    if [ "$SKIP_COLD" != 1 ]; then cold old; cold new; fi
    life; wake; nodraft; metrics
    for p in $(seq "$PASSES"); do conc old "$p"; conc new "$p"; done
    ctx old; ctx new
    say "phases done" ;;
  down) down ;;
  purge) cd / && rm -rf -- "$WORK" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
