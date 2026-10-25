#!/usr/bin/env bash
# Host side of run.sh: the multi-GPU live rows (ADR 0035 MG1-MG14) and the
# download bandwidth test, on one rented x86_64 machine with two identical
# NVIDIA GPUs, through CapyCTL roles that each have their own state and config
# directories. Everything lives under $MG (default ~/mg) and in the three engine
# directories named below; `purge` removes exactly what `setup` created.
#
#   remote.sh preflight  machine facts; refuses a machine without two identical GPUs
#   remote.sh setup      toolchains, CapyCTL builds, the approved engines, the GGUF model
#   remote.sh run        the bandwidth test and the selected rows, from $MG/mg.env
#   remote.sh plan       print what `run` does, step by step, running nothing (any machine)
#   remote.sh down       stop every role this run started and delete its deployments
#   remote.sh purge      remove everything `setup` and `run` created (created.txt)
#
# Only processes this script started, or that CapyCTL recorded as a launch's own
# (ledger identities, checked against pid, start ticks and boot id), are ever
# signalled. Engine keys are never read; a role's inference key and admin token
# are read from its credentials file by mg.py only, never printed. Ledgers are
# read through read-only SELECTs.
# `test && result pass ... || result fail ...` is used on purpose: result and say always succeed.
# shellcheck disable=SC2015
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
cmd=${1:-}
MG=${MG:-$HOME/mg}
PLAN=0
[ "$cmd" = plan ] && PLAN=1
if [ -f "$MG/mg.env" ]; then
  # shellcheck disable=SC1091
  . "$MG/mg.env"
fi
PORT=${PORT:-18743} MPORT=${MPORT:-18744} ENGINE_PORTS=${ENGINE_PORTS:-8740-8759}
ONLY=${ONLY:-all} STALL=${STALL:-30} CYCLES=${CYCLES:-5} HB_CYCLES=${HB_CYCLES:-3}
OLD_TAG=${OLD_TAG:-v0.1.8}
RES=$MG/res
BIN=$MG/bin
export RUSTUP_HOME=$MG/rust CARGO_HOME=$MG/rust
export PATH=$BIN:$MG/rust/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
HELPER=(python3 "$HERE/mg.py")
EP=http://127.0.0.1:$PORT
SOURCES=$MG/sources
CREATED=$MG/created.txt

# The approved engine versions (ADR 0035 §14, owner decision 2): fresh x86_64 CUDA builds.
V32=$HOME/vllm-0.32.0-venv
S22=$HOME/sglang-0.5.22-venv
LC=$HOME/llamacpp-v0.7.0
LC_TAG=v0.7.0 LC_COMMIT=620d222
LCBIN=$LC/build/bin/llama-server
# Models: the bring-up model of ADR 0028 (BF16, about 61 GB) and Qwen3-4B, both
# downloaded by CapyCTL from Hugging Face (the bandwidth test), at the commit `main`
# names when the run starts; the GGUF at the revision the laptop rows used.
Q30_REPO=Qwen/Qwen3-30B-A3B
Q4_REPO=Qwen/Qwen3-4B
GGUF_REPO=Qwen/Qwen3-4B-GGUF GGUF_REV=bc640142c66e1fdd12af0bd68f40445458f3869b
GGUF_FILE=Qwen3-4B-Q4_K_M.gguf GGUF_BYTES=2497280256
GGUF_DIR=$MG/models/Qwen3-4B-GGUF
PROTOC_VERSION=29.3
ANY_ENGINE='sglang[.]launch_server|sglang_entr[y]|vllm_entr[y]|vllm[ ]serve|Engine[C]ore|sglang::schedule[r]|llama-serve[r]|VllmWorke[r]'

# ---------------------------------------------------------------------------
# Effects. In plan mode every effect is printed instead; waits are skipped and
# measured values read as placeholders.
# ---------------------------------------------------------------------------
exec 3>&1
show() { printf '    %s\n' "$*" | sed -e "s#$HOME#~#g" >&3; }
say() {
  if [ "$PLAN" = 1 ]; then printf '\n  %s\n' "$*" | sed -e "s#$HOME#~#g" >&3; return 0; fi
  mkdir -p "$RES"
  printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RES/progress.log"
}
phase() { if [ "$PLAN" = 1 ]; then printf '\n== %s\n' "$*" >&3; else say "== $*"; fi; }
run() { if [ "$PLAN" = 1 ]; then show "\$ $(printf '%q ' "$@")"; return 0; fi; "$@"; }
ev() { [ "$PLAN" = 1 ] && return 0; "${HELPER[@]}" "$@"; }
check() { if [ "$PLAN" = 1 ]; then printf '    => %s (%s) PASS when: %s\n' "$1" "$2" "$3" >&3; else say "checking $1 ($2)"; fi; }
result() { # id part pass|fail|skip detail
  [ "$PLAN" = 1 ] && return 0
  printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >>"$RES/results.tsv"
  say "[$3] $1 ($2): $4"
}
skip() { if [ "$PLAN" = 1 ]; then printf '    => %s (%s) SKIP: %s\n' "$1" "$2" "$3" >&3; else result "$1" "$2" skip "$3"; fi; }
want() { local id; [ "$ONLY" = all ] && return 0; for id in "$@"; do case ",$ONLY," in *",$id,"*) return 0 ;; esac; done; return 1; }
pause() { [ "$PLAN" = 1 ] || sleep "$1"; }
created() { [ "$PLAN" = 1 ] || grep -qxF -- "$1" "$CREATED" 2>/dev/null || echo "$1" >>"$CREATED"; }
gib() { awk -v b="${1:-0}" 'BEGIN {printf "%.2f", b / 1073741824}'; }
now() { date +%s.%N; }
since() { if [ "$PLAN" = 1 ]; then echo 0; else awk -v a="$1" -v b="$(now)" 'BEGIN {printf "%.1f", b - a}'; fi; }
engines_running() { pgrep -f -- "$ANY_ENGINE" >/dev/null; }
waitgone() {
  [ "$PLAN" = 1 ] && { show "wait until no engine process is left (600 s)"; return 0; }
  local _
  for _ in $(seq 600); do engines_running || return 0; sleep 1; done
  return 1
}

# ---------------------------------------------------------------------------
# preflight and setup
# ---------------------------------------------------------------------------
preflight() {
  local fail=0
  echo "arch: $(uname -m)"
  [ "$(uname -m)" = x86_64 ] || { echo "FAIL: not x86_64"; fail=1; }
  command -v nvidia-smi >/dev/null || { echo "FAIL: no nvidia-smi"; exit 1; }
  nvidia-smi --query-gpu=index,name,memory.total,compute_cap,driver_version,pci.bus_id --format=csv
  nvidia-smi | grep -m1 -o 'CUDA[ A-Za-z]* Version: [0-9.]*' || true
  local n distinct cuda
  n=$(nvidia-smi --query-gpu=index --format=csv,noheader | wc -l)
  distinct=$(nvidia-smi --query-gpu=name,memory.total --format=csv,noheader | sort -u | wc -l)
  [ "$n" -ge 2 ] || { echo "FAIL: $n GPU(s); the rows need two"; fail=1; }
  [ "$distinct" = 1 ] || { echo "FAIL: the GPUs differ (ADR 0035 §1: identical GPUs only)"; fail=1; }
  cuda=$(nvidia-smi | grep -m1 -o 'Version: [0-9.]*' | tail -1 | grep -o '[0-9.]*$' || true)
  case $cuda in 1[3-9].*|[2-9][0-9].*) ;; *) echo "FAIL: the driver supports CUDA ${cuda:-unknown}; the approved builds need 13.0"; fail=1 ;; esac
  nvidia-smi topo -m 2>/dev/null | head -n 4 || true
  echo "memory: $(awk '/^MemTotal:/ {printf "%.0f GiB", $2/1048576}' /proc/meminfo)"
  echo "disk free under ~: $(df -BG --output=avail "$HOME" | tail -1 | tr -d ' ')"
  [ "$(df -BG --output=avail "$HOME" | tail -1 | tr -dc 0-9)" -ge 250 ] || { echo "FAIL: under 250 GB free in ~ (models about 75 GB, engines about 20 GB, builds about 10 GB)"; fail=1; }
  for t in git curl python3 cc c++ make tar; do command -v "$t" >/dev/null || { echo "FAIL: missing $t"; fail=1; }; done
  command -v cmake >/dev/null || echo "note: no cmake; setup installs the cmake wheel from PyPI into $MG/cmake-venv for the llama.cpp build"
  if command -v nvcc >/dev/null || [ -x /usr/local/cuda/bin/nvcc ]; then
    echo "nvcc: $( (command -v nvcc || echo /usr/local/cuda/bin/nvcc) | head -1)"
  else
    echo "note: no nvcc; setup installs NVIDIA's CUDA 13.0 compiler wheel into $MG/cuda for the llama.cpp build"
  fi
  [ ! -e /etc/llama.cpp/config.ini ] || { echo "FAIL: /etc/llama.cpp/config.ini exists (CapyCTL refuses llama.cpp there)"; fail=1; }
  if engines_running; then echo "FAIL: an engine process is already running"; fail=1; fi
  [ "$fail" = 0 ] && echo PREFLIGHT_OK
  return "$fail"
}

nvcc_path() {
  if command -v nvcc >/dev/null; then command -v nvcc
  elif [ -x /usr/local/cuda/bin/nvcc ]; then echo /usr/local/cuda/bin/nvcc
  else echo "$MG/cuda/bin/nvcc"; fi
}

# The CUDA toolkit the vLLM and SGLang profiles name (FlashInfer JIT-compiles
# its sampling kernels at warm-up): the machine's, else the session's wheel
# toolkit (setup_engines). `capyctl engine add` records `CUDA_HOME` as the
# profile's cuda_home when it holds bin/nvcc.
cuda_dir() {
  if [ -x /usr/local/cuda/bin/nvcc ]; then echo /usr/local/cuda
  elif [ "$PLAN" = 1 ] || [ -x "$MG/cuda/bin/nvcc" ]; then echo "$MG/cuda"; fi
}

setup_tools() {
  phase "tools under $MG: uv, protoc $PROTOC_VERSION, Rust (rust-toolchain.toml)"
  run mkdir -m 700 -p "$MG" "$BIN" "$RES"
  created "$MG"
  # Caches the installs and engines fill in the home directory: removed by purge
  # only when this run is the one that created them (parents before children, so
  # purge, which goes from the last line up, removes a created parent last).
  local d
  for d in .cache .local .local/share .config .cache/uv .local/share/uv .cache/vllm .cache/flashinfer .cache/torch .cache/triton .triton \
    .cache/sglang .cache/huggingface .nv .cache/tvm-ffi .cache/torch_extensions .config/vllm .cache/pip; do
    [ -e "$HOME/$d" ] || created "$HOME/$d"
  done
  if [ ! -x "$BIN/uv" ]; then
    run sh -c "curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR=$BIN UV_NO_MODIFY_PATH=1 sh"
  fi
  if [ ! -x "$BIN/protoc" ]; then
    run curl -fsSL -o "$MG/protoc.zip" "https://github.com/protocolbuffers/protobuf/releases/download/v$PROTOC_VERSION/protoc-$PROTOC_VERSION-linux-x86_64.zip"
    run python3 -c "import zipfile,sys; z=zipfile.ZipFile(sys.argv[1]); open(sys.argv[2],'wb').write(z.read('bin/protoc')); [z.extract(n, sys.argv[3]) for n in z.namelist() if n.startswith('include/')]" "$MG/protoc.zip" "$BIN/protoc" "$MG"
    run chmod 755 "$BIN/protoc"
  fi
  if [ ! -x "$MG/rust/bin/rustup" ]; then
    run sh -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none"
  fi
  if ! command -v cmake >/dev/null; then
    run "$BIN/uv" venv "$MG/cmake-venv"
    run "$BIN/uv" pip install --python "$MG/cmake-venv/bin/python" cmake
    run ln -sfn "$MG/cmake-venv/bin/cmake" "$BIN/cmake"
  fi
  [ "$PLAN" = 1 ] || { "$BIN/uv" --version; "$BIN/protoc" --version; cmake --version | head -1; } | tee -a "$RES/versions.txt"
}

# The qualification build (ADR 0035 §9, owner decision 3): the shipped support
# data with `deep_park: true` for vLLM's and SGLang's local multi-GPU mode, which
# is exactly the change MG4 and MG5 make when they pass. Refuses unless it flips
# exactly those two entries.
QUALIFY_PY='
import re, sys
path = sys.argv[1]
text = open(path).read()
start = text.index("pub fn local_support")
end = text.index("\n}\n", start)
body = text[start:end]
flipped, n = re.subn(r"(\"(?:vllm|sglang)\"\s*=>\s*Some\(LocalSupport\s*\{[^}]*?deep_park:\s*)false", r"\1true", body)
if n != 2:
    sys.exit(f"qualify: expected to flip 2 deep_park entries in local_support, flipped {n}")
open(path, "w").write(text[:start] + flipped + text[end:])
print("qualify: vllm and sglang local deep_park set to true")
'

build_capyctl() {
  phase "CapyCTL builds: this checkout (shipped support data), its qualification build, and $OLD_TAG (MG11's older host)"
  local cargo=(cargo build --release --locked --bin capyctl)
  if [ ! -x "$BIN/capyctl" ]; then
    run sh -c "cd $MG/src && PROTOC=$BIN/protoc ${cargo[*]} 2>&1 | tail -n 3 && cp target/release/capyctl $BIN/capyctl"
  fi
  if [ ! -x "$BIN/capyctl-qualify" ]; then
    run rm -rf "$MG/src-qualify"
    run cp -a "$MG/src" "$MG/src-qualify"
    run python3 -c "$QUALIFY_PY" "$MG/src-qualify/crates/capyctl-config/src/group_support.rs"
    run sh -c "cd $MG/src-qualify && PROTOC=$BIN/protoc ${cargo[*]} 2>&1 | tail -n 3 && cp target/release/capyctl $BIN/capyctl-qualify"
  fi
  # A dry run makes the qualification edit on a copy of this checkout's support
  # data, so a plan that passes is one whose qualification build turns on deep
  # parking at N > 1 for vLLM and SGLang (and for nothing else).
  local support=$HERE/../../../crates/capyctl-config/src/group_support.rs
  if [ "$PLAN" = 1 ] && [ -f "$support" ]; then
    cp "$support" "$MG/group_support.rs"
    python3 -c "$QUALIFY_PY" "$MG/group_support.rs" >/dev/null ||
      { echo "qualify: this checkout's support data cannot make the qualification build" >&2; exit 1; }
    show "qualification edit checked on this checkout: $(diff "$support" "$MG/group_support.rs" | grep -c '^>') line(s) set deep_park: true"
    diff "$support" "$MG/group_support.rs" | grep '^>' | sed -e 's/^>/     +/' >&3
  fi
  if [ ! -x "$BIN/capyctl-old" ] && [ -d "$MG/src-old" ]; then
    run sh -c "cd $MG/src-old && PROTOC=$BIN/protoc ${cargo[*]} 2>&1 | tail -n 3 && cp target/release/capyctl $BIN/capyctl-old"
  fi
  [ "$PLAN" = 1 ] || for b in capyctl capyctl-qualify capyctl-old; do
    [ -x "$BIN/$b" ] && echo "$b $("$BIN/$b" --version 2>&1 | head -1) sha256 $(sha256sum "$BIN/$b" | cut -c1-64)"
  done | tee -a "$RES/versions.txt"
}

setup_engines() {
  phase "engines: vLLM 0.32.0 and SGLang 0.5.22 (uv, CUDA 13.0 PyTorch), llama.cpp $LC_TAG built for this GPU"
  if [ ! -x "$V32/bin/vllm" ]; then
    created "$V32"
    run "$BIN/uv" venv --python 3.12 --seed --managed-python "$V32"
    run "$BIN/uv" pip install --python "$V32/bin/python" vllm==0.32.0 --torch-backend=cu130
  fi
  if [ ! -x "$S22/bin/python" ] || ! "$S22/bin/python" -c 'import sglang' 2>/dev/null; then
    created "$S22"
    run "$BIN/uv" venv --python 3.12 --seed --managed-python "$S22"
    run "$BIN/uv" pip install --python "$S22/bin/python" --prerelease=allow "sglang==0.5.22"
  fi
  run chmod -R go-w "$V32" "$S22"
  local nvcc arch
  nvcc=$(nvcc_path)
  if [ "$nvcc" = "$MG/cuda/bin/nvcc" ]; then
    # The session's CUDA 13.0 toolkit, from NVIDIA's wheels: the llama.cpp
    # build, and the CUDA_HOME of the vLLM and SGLang profiles (FlashInfer
    # JIT-compiles its sampling kernels at warm-up; curand for those).
    created "$MG/cuda"
    [ -x "$MG/cuda-venv/bin/python" ] || run "$BIN/uv" venv --python 3.12 "$MG/cuda-venv"
    run "$BIN/uv" pip install --python "$MG/cuda-venv/bin/python" "cuda-toolkit[nvcc,cccl,cudart,cublas,curand]==13.0.*"
    run sh -c "ln -sfn \$(dirname \$(dirname \$(find $MG/cuda-venv -path '*/bin/nvcc' | head -1))) $MG/cuda"
    # The wheels keep versioned libraries in lib/: nvcc links from lib64, and
    # CMake's FindCUDAToolkit looks for the unversioned names.
    run sh -c "cd $MG/cuda/lib && for l in libcudart libcublas libcublasLt; do ln -sfn \$l.so.13 \$l.so; done && ln -sfn lib $MG/cuda/lib64"
    # nvcc runs `gcc` as its host compiler; where `gcc` is a newer major version
    # than the installed g++ (no cc1plus for it), the toolkit's bin directory,
    # which the engine PATH puts before the system's, names the pair g++ has.
    if ! gcc -print-prog-name=cc1plus | grep -q /; then
      local gxx
      gxx=$(c++ -dumpversion | cut -d. -f1)
      run ln -sfn "/usr/bin/gcc-$gxx" "$MG/cuda/bin/gcc"
      run ln -sfn "/usr/bin/g++-$gxx" "$MG/cuda/bin/g++"
    fi
  fi
  if [ "$PLAN" = 1 ]; then arch=86; else
    arch=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d '.')
  fi
  if ! { [ -x "$LCBIN" ] && "$LCBIN" --version 2>&1 | grep -q "commit $LC_COMMIT"; }; then
    created "$LC"
    [ -d "$LC/.git" ] || run git clone --branch "$LC_TAG" --depth 1 https://github.com/ggml-org/llama.cpp "$LC"
    # The host compiler is `c++` for CUDA as well: `gcc` can be a newer major
    # version than the installed g++ (no cc1plus for it). CMake leaves the
    # toolkit's library directory out of the runpath (nvcc reports it as
    # implicit), so llama-server would not find the wheels' libcudart.so.13.
    local cudaroot
    cudaroot=$(dirname "$(dirname "$nvcc")")
    run sh -c "cd $LC && rm -rf build && cmake -B build -DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF -DCUDAToolkit_ROOT=$cudaroot -DCMAKE_CUDA_COMPILER=$nvcc -DCMAKE_CXX_COMPILER=\$(command -v c++) -DCMAKE_CUDA_HOST_COMPILER=\$(command -v c++) -DCMAKE_BUILD_RPATH=$cudaroot/lib64 -DCMAKE_CUDA_ARCHITECTURES=${arch}-real -DCMAKE_BUILD_TYPE=Release >/dev/null && cmake --build build -j \$(nproc) --target llama-server 2>&1 | tail -n 2"
  fi
  [ "$PLAN" = 1 ] || {
    echo "vllm $("$V32/bin/python" -c 'import vllm, torch; print(vllm.__version__, torch.__version__, torch.version.cuda)')"
    echo "sglang $("$S22/bin/python" -c 'import sglang, torch; print(sglang.__version__, torch.__version__, torch.version.cuda)')"
    echo "llama.cpp $("$LCBIN" --version 2>&1 | grep -m1 '^version')"
    # The fingerprints the rows are recorded with (not byte copies of the lab hosts').
    (cd "$V32" && find . -name RECORD -path '*vllm-0.32.0*' -exec sha256sum {} +)
    (cd "$S22" && find . -name RECORD -path '*sglang-0.5.22*' -exec sha256sum {} +)
    (cd "$(dirname "$LCBIN")" && find . -maxdepth 1 -type f \( -name llama-server -o -name 'lib*.so*' \) -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
  } | tee -a "$RES/versions.txt"
}

setup_gguf() {
  phase "GGUF model for MG3: $GGUF_REPO $GGUF_FILE at $GGUF_REV"
  run mkdir -p "$GGUF_DIR"
  if [ "$PLAN" = 1 ] || [ "$(stat -c %s "$GGUF_DIR/$GGUF_FILE" 2>/dev/null || echo 0)" != "$GGUF_BYTES" ]; then
    run curl -fL --retry 3 -C - -o "$GGUF_DIR/$GGUF_FILE.part" "https://huggingface.co/$GGUF_REPO/resolve/$GGUF_REV/$GGUF_FILE"
    run mv "$GGUF_DIR/$GGUF_FILE.part" "$GGUF_DIR/$GGUF_FILE"
  fi
}

setup() {
  setup_tools
  build_capyctl
  setup_engines
  setup_gguf
  if [ "$PLAN" = 0 ]; then
    { echo "Q30_REV=$("${HELPER[@]}" hf-rev "$Q30_REPO")"; echo "Q4_REV=$("${HELPER[@]}" hf-rev "$Q4_REPO")"; } >"$MG/revs.env"
    tee -a "$RES/versions.txt" <"$MG/revs.env"
    local missing="" f
    for f in "$BIN/capyctl" "$BIN/capyctl-qualify" "$V32/bin/vllm" "$S22/bin/python" "$LCBIN"; do
      [ -x "$f" ] || missing="$missing $f"
    done
    [ -s "$GGUF_DIR/$GGUF_FILE" ] || missing="$missing $GGUF_DIR/$GGUF_FILE"
    grep -q '^Q30_REV=[0-9a-f]\{40\}$' "$MG/revs.env" || missing="$missing Q30_REV"
    [ -z "$missing" ] || { say "SETUP_FAILED: missing$missing"; return 1; }
  fi
  say "SETUP_OK"
}

# ---------------------------------------------------------------------------
# Roles, profiles, deployments
# ---------------------------------------------------------------------------
ROLE="" SD="" CRED="" DB="" ROLE_LOG="" CAPYCTL="" DEPLOYS="" PROFILES=""
c() { run env XDG_CONFIG_HOME="$MG/cfg-$ROLE" "$CAPYCTL" --state-dir "$SD" "$@"; }

role_up() { # tag binary [start standalone options]
  local tag=$1 bin=$2 _
  shift 2
  ROLE=$tag SD=$MG/$tag CAPYCTL=$BIN/$bin DEPLOYS="" PROFILES=""
  CRED=$SD/identity/credentials DB=$SD/server/srv.sqlite3 ROLE_LOG=$RES/role-$tag.log
  say "role $tag ($bin): standalone, state $SD, 127.0.0.1:$PORT, engine ports $ENGINE_PORTS, stall timeout ${STALL}s"
  run mkdir -m 700 -p "$SD" "$MG/cfg-$tag"
  if [ "$PLAN" = 1 ]; then
    show "\$ CAPYCTL_GROUP_STALL_TIMEOUT=${STALL}s setsid capyctl --state-dir $SD start standalone --listen 127.0.0.1:$PORT --management-listen 127.0.0.1:$MPORT --engine-ports $ENGINE_PORTS --model-sources-path $SOURCES $* --json &"
    return 0
  fi
  XDG_CONFIG_HOME="$MG/cfg-$tag" CAPYCTL_GROUP_STALL_TIMEOUT="${STALL}s" setsid nohup "$CAPYCTL" --state-dir "$SD" \
    start standalone --listen "127.0.0.1:$PORT" --management-listen "127.0.0.1:$MPORT" \
    --engine-ports "$ENGINE_PORTS" --model-sources-path "$SOURCES" "$@" --json >"$ROLE_LOG" 2>&1 </dev/null &
  echo $! >"$MG/role.pid"
  for _ in $(seq 180); do grep -q '"ready":true' "$ROLE_LOG" 2>/dev/null && return 0; sleep 1; done
  say "role $tag: no ready line in 180 s"
  return 1
}

role_down() {
  [ -n "$ROLE" ] || return 0
  local d pid _
  say "role $ROLE down: delete its deployments (--stop), remove its engines, TERM its own PID"
  for d in $DEPLOYS; do c delete deployment "$d" --stop >>"$RES/cli.log" 2>&1; done
  waitgone
  for d in $PROFILES; do c engine remove "$d" >>"$RES/cli.log" 2>&1; done
  if [ "$PLAN" = 0 ] && [ -f "$MG/role.pid" ] && pid=$(cat "$MG/role.pid") && [ -r "/proc/$pid/cmdline" ] &&
     tr '\0' ' ' <"/proc/$pid/cmdline" | grep -qF -- "$SD"; then
    kill -TERM "$pid"
    for _ in $(seq 60); do [ -d "/proc/$pid" ] || break; sleep 1; done
  fi
  [ "$PLAN" = 1 ] || { rm -f "$MG/role.pid"; [ -d "$SD/logs" ] && cp -r "$SD/logs" "$RES/engine-logs-$ROLE"; }
  ROLE=""
}

profiles() { # the three engines, once per role
  local cuda=()
  [ -z "$(cuda_dir)" ] || cuda=(CUDA_HOME="$(cuda_dir)")
  run env "${cuda[@]}" XDG_CONFIG_HOME="$MG/cfg-$ROLE" "$CAPYCTL" --state-dir "$SD" engine add "$V32" --name v32 >>"$RES/cli.log" 2>&1 && PROFILES="$PROFILES v32"
  run env "${cuda[@]}" XDG_CONFIG_HOME="$MG/cfg-$ROLE" "$CAPYCTL" --state-dir "$SD" engine add "$S22" --name s22 >>"$RES/cli.log" 2>&1 && PROFILES="$PROFILES s22"
  c engine add "$LCBIN" --name lc >>"$RES/cli.log" 2>&1 && PROFILES="$PROFILES lc"
  [ "$PLAN" = 1 ] || c engine list >>"$RES/cli.log" 2>&1
}

yaml() { # name: stdin -> $MG/dep/<name>.yaml, shown in a plan
  mkdir -p "$MG/dep"
  cat >"$MG/dep/$1.yaml"
  if [ "$PLAN" = 1 ]; then show "deployment file dep/$1.yaml:"; sed -e 's/^/        /' "$MG/dep/$1.yaml" >&3; fi
}

dep() { # name engine model-line extra-yaml-lines
  yaml "$1" <<EOF
schema_version: 1
kind: deployment
name: $1
engine: $2
model: $3
timeouts: {initialize: 1800s}
$4
EOF
}

q30() { echo "{hf: $Q30_REPO@${Q30_REV:-<rev>}}"; }
q4() { echo "{hf: $Q4_REPO@${Q4_REV:-<rev>}}"; }

deploy() { # name: prints the CLI's exit code and keeps its output in res/deploy-<name>.out
  local rc
  c deploy model --file "$MG/dep/$1.yaml" >"$RES/deploy-$1.out" 2>&1
  rc=$?
  [ "$PLAN" = 1 ] && rc=0
  [ "$rc" = 0 ] && DEPLOYS="$DEPLOYS $1"
  return "$rc"
}

START_S=""
startd() { # name [start options]: Ready by --wait
  local n=$1 s rc
  shift
  s=$(now)
  c start deployment "$n" --wait "$@" >"$RES/start-$n.out" 2>&1
  rc=$?
  [ "$PLAN" = 1 ] && rc=0
  START_S=$(since "$s")
  [ "$rc" = 0 ] && say "started $n in $START_S s" || say "start $n failed (exit $rc); see start-$n.out"
  return "$rc"
}
stopd() { c stop deployment "$1" >>"$RES/cli.log" 2>&1; waitgone; pause 5; }
status_json() { c status deployment "$1" --json >"$2" 2>>"$RES/cli.log"; }
state_of() {
  [ "$PLAN" = 1 ] && { echo "${2:-ready}"; return 0; }
  c status deployment "$1" --json >"$RES/state.json" 2>/dev/null
  ev get "$RES/state.json" observed_state
}
poll_state() { # name timeout state...
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
poll_sources() { # name timeout: every model source verified
  local n=$1 t=$2 st _
  [ "$PLAN" = 1 ] && { show "poll status until every model_sources[].state is verified (${t} s)"; echo verified; return 0; }
  for _ in $(seq $((t / 5))); do
    status_json "$n" "$RES/sources-$n.json"
    st=$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); s=[x.get("state") for x in d.get("model_sources") or []]
print("failed" if "failed" in s else "verified" if s and all(x=="verified" for x in s) else "pending")' "$RES/sources-$n.json" 2>/dev/null)
    case $st in verified|failed) echo "$st"; return 0 ;; esac
    sleep 5
  done
  echo timeout
}
chat() { # name tag -> "HTTP seconds ok|wrong|-"
  [ "$PLAN" = 1 ] && { show "chat through the router: model $1, the default prompt (17+25), expect 200 and 42"; echo "200 0 ok"; return 0; }
  "${HELPER[@]}" chat --cred "$CRED" --endpoint "$EP" --model "$1" --out "$RES/chat-$2.json" 2>>"$RES/cli.log"
}
owned_pids() { # name [--rank N] -> comma list of recorded PIDs
  [ "$PLAN" = 1 ] && { echo 0; return 0; }
  "${HELPER[@]}" owned "$DB" "$@" 2>/dev/null | python3 -c 'import json,sys; print(",".join(str(json.loads(l)["pid"]) for l in sys.stdin if l.strip()))'
}
rank_identity() { # name rank -> "pid ticks boot" of that rank process
  [ "$PLAN" = 1 ] && { echo "0 0 boot"; return 0; }
  "${HELPER[@]}" owned "$DB" "$1" --rank "$2" | head -1 | python3 -c 'import json,sys
d=json.loads(sys.stdin.read() or "{}"); print(d.get("pid",""), d.get("start_ticks", d.get("ticks","")), d.get("boot_id", d.get("boot","")))'
}
signal_rank() { # name rank signal: only the recorded identity, checked live
  local id pid ticks boot live
  id=$(rank_identity "$1" "$2")
  read -r pid ticks boot <<<"$id"
  if [ "$PLAN" = 1 ]; then show "kill -$3 <pid of rank $2 from the ledger's identities, checked against start ticks and boot id>"; return 0; fi
  [ -n "$pid" ] && [ -r "/proc/$pid/stat" ] || { say "no live rank $2 recorded for $1"; return 1; }
  live=$(awk '{print $22}' "/proc/$pid/stat")
  [ "$live" = "$ticks" ] && [ "$boot" = "$(cat /proc/sys/kernel/random/boot_id)" ] || { say "rank $2 identity mismatch; not signalled"; return 1; }
  kill "-$3" "$pid"
  say "sent SIG$3 to rank $2 of $1 (pid $pid)"
}
event_seq() { [ "$PLAN" = 1 ] && { echo 0; return 0; }; "${HELPER[@]}" sql "$DB" "SELECT coalesce(max(sequence),0) FROM management_events"; }
events_since() { # after kinds... -> JSON lines
  [ "$PLAN" = 1 ] && return 0
  local after=$1
  shift
  "${HELPER[@]}" events "$DB" --after "$after" --kinds "$(IFS=,; echo "$*")"
}

# ---------------------------------------------------------------------------
# Row pieces
# ---------------------------------------------------------------------------

# MG1, MG2, MG3: Ready, the probe answers, per-GPU charge against the cards' use,
# peaks recorded, listeners on loopback only, `collectives: loopback` in status.
serve_check() { # row name engine
  local row=$1 n=$2 e=$3 r pids gpus lis charge tp coll
  check "$row" ready "deploy and start $n; one chat through the router answers 42"
  if ! startd "$n"; then result "$row" ready fail "start failed; see start-$n.out"; return 1; fi
  r=$(chat "$n" "$row")
  [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ] && result "$row" ready pass "Ready in $START_S s; chat ${r}" || result "$row" ready fail "Ready in $START_S s; chat ${r:-none}"
  status_json "$n" "$RES/$row-status.json"
  c status deployment "$n" >"$RES/$row-status.txt" 2>&1
  pids=$(owned_pids "$n")
  check "$row" charge "each GPU's ledger charge covers the launch's own bytes on that card (nvidia-smi per process and GPU), and both GPUs hold part"
  if [ "$PLAN" = 0 ]; then
    gpus=$("${HELPER[@]}" gpus --pids "$pids"); echo "$gpus" >"$RES/$row-gpus.json"
    charge=$("${HELPER[@]}" charge "$DB" "$n"); echo "$charge" >"$RES/$row-charge.json"
    # The whole ledger beside it, so a charge that reads empty can be explained.
    "${HELPER[@]}" sql "$DB" "SELECT owner_id,domain_id,bytes,phase FROM reservations ORDER BY owner_id" >"$RES/$row-reservations.tsv"
    "${HELPER[@]}" sql "$DB" "SELECT id,name FROM deployments" >"$RES/$row-deployments.tsv"
    r=$(python3 - "$RES/$row-gpus.json" "$RES/$row-charge.json" <<'PY'
import json, re, sys
g = json.load(open(sys.argv[1])); c = json.load(open(sys.argv[2]))
used = g["owned_mib"]
charged = {}
for domain, rs in c.items():
    m = re.search(r"gpu(\d+)", domain)
    if m:
        charged[f"gpu{m.group(1)}"] = max(r["bytes"] for r in rs) / 2**20
ok = len(used) >= 2 and all(charged.get(k, 0) >= v for k, v in used.items())
print(("pass" if ok else "fail") + "\t" + "; ".join(
    f"{k}: used {used.get(k, 0)} MiB, charged {charged.get(k, 0):.0f} MiB" for k in sorted(set(used) | set(charged))))
PY
)
    result "$row" charge "${r%%$'\t'*}" "${r#*$'\t'}"
  fi
  check "$row" listeners "no recorded process of the launch listens off 127.0.0.1/::1, by /proc and by ss -ltnp alike; status shows collectives: loopback"
  if [ "$PLAN" = 0 ]; then
    lis=$("${HELPER[@]}" listeners --pids "$pids"); echo "$lis" >"$RES/$row-listeners.json"
    coll=$("${HELPER[@]}" get "$RES/$row-status.json" 'instances[0].collectives')
    tp=$("${HELPER[@]}" get "$RES/$row-status.json" 'instances[0].topology.tensor_parallel')
    r=$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(len(d["off_loopback"]), len(d["ss_off_loopback"]), len(d["proc"]))' "$RES/$row-listeners.json")
    read -r off ssoff total <<<"$r"
    if [ "$off" = 0 ] && [ "$ssoff" = 0 ] && [ "$coll" = loopback ] && [ "$tp" = 2 ]; then
      result "$row" listeners pass "$total listeners, all loopback; collectives $coll; tensor_parallel $tp"
    else
      result "$row" listeners fail "off loopback: proc $off, ss $ssoff; collectives '${coll}'; tensor_parallel '${tp}'"
    fi
  fi
  check "$row" peaks "after the start the role holds a measured startup figure for this revision (status startup.measured; per GPU at N = 2, ADR 0035 §3)"
  if [ "$PLAN" = 0 ]; then
    "${HELPER[@]}" get "$RES/$row-status.json" startup >"$RES/$row-peaks.json"
    if [ "$("${HELPER[@]}" get "$RES/$row-status.json" 'startup.measured[0]')" != "" ]; then
      result "$row" peaks pass "$(tr '\n' ' ' <"$RES/$row-peaks.json" | cut -c1-300)"
    else result "$row" peaks fail "no measured startup figure in status: $(cut -c1-200 "$RES/$row-peaks.json")"; fi
  fi
  [ "$e" = lc ] || return 0
  check "$row" fit "llama.cpp's per-device model buffers each fit that GPU's grant; both CUDA0 and CUDA1 hold a part"
  if [ "$PLAN" = 0 ]; then
    find "$SD/logs" -type f -name '*.log' -newer "$RES/deploy-$n.out" -exec "${HELPER[@]}" fit {} + >"$RES/$row-fit.txt" 2>/dev/null
    if grep -q 'CUDA0 model buffer' "$RES/$row-fit.txt" && grep -q 'CUDA1 model buffer' "$RES/$row-fit.txt"; then
      result "$row" fit pass "$(grep -c buffer "$RES/$row-fit.txt") buffer lines; see $row-fit.txt beside $row-charge.json"
    else
      # llama.cpp v0.7.0 logs no buffer sizes at its default verbosity: the
      # launch's own per-GPU readings stand in (both cards hold a part, each
      # within its grant, as the charge part reads them).
      r=$(python3 -c 'import json,sys; u=json.load(open(sys.argv[1]))["owned_mib"]; print(" ".join(f"{k}={v}MiB" for k, v in sorted(u.items())) if len(u) >= 2 and all(v > 0 for v in u.values()) else "")' "$RES/$row-gpus.json")
      if [ -n "$r" ] && grep -q "^$row"$'\t'"charge"$'\t'"pass" "$RES/results.tsv"; then
        result "$row" fit pass "no buffer lines in llama.cpp's log (default verbosity); the launch's own readings: $r, each within its grant"
      else
        result "$row" fit fail "no per-device buffer lines; readings '${r}'; the charge part did not pass"
      fi
    fi
  fi
}

# MG7: rank 1 killed; the instance fails naming rank 1, no release before every process is gone.
rank_kill() { # name label
  local n=$1 l=$2 seq0 pids t_gone="" t_rel="" i alive charged
  check MG7 "$l kill" "SIGKILL to rank 1 (ledger identity): failed with engine_rank_exited naming rank 1; the reservations stay until every recorded process is gone"
  startd "$n" || { result MG7 "$l kill" fail "start failed"; return; }
  seq0=$(event_seq)
  pids=$(owned_pids "$n")
  signal_rank "$n" 1 KILL || { result MG7 "$l kill" fail "no rank 1 identity recorded"; stopd "$n"; return; }
  if [ "$PLAN" = 0 ]; then
    for i in $(seq 1200); do
      alive=0
      for p in ${pids//,/ }; do [ -d "/proc/$p" ] && alive=1; done
      charged=$("${HELPER[@]}" charge "$DB" "$n" | python3 -c 'import json,sys; print(1 if json.load(sys.stdin) else 0)')
      [ "$alive" = 0 ] && [ -z "$t_gone" ] && t_gone=$i
      [ "$charged" = 0 ] && [ -z "$t_rel" ] && t_rel=$i
      [ -n "$t_gone" ] && [ -n "$t_rel" ] && break
      sleep 0.5
    done
    events_since "$seq0" engine_rank_exited >"$RES/mg7-$l-events.jsonl"
    st=$(state_of "$n")
    if [ -n "$t_rel" ] && [ -n "$t_gone" ] && [ "$t_rel" -ge "$t_gone" ] && grep -q '1' "$RES/mg7-$l-events.jsonl" && [ "$st" = failed ]; then
      result MG7 "$l kill" pass "state $st; engine_rank_exited recorded; processes gone at tick $t_gone, charge released at tick $t_rel (0.5 s ticks)"
    else
      result MG7 "$l kill" fail "state '$st'; events $(wc -l <"$RES/mg7-$l-events.jsonl"); gone tick '${t_gone}', release tick '${t_rel}'"
    fi
    "${HELPER[@]}" shm --out "$RES/mg7-$l-shm-after-kill.txt"
  fi
  stopd "$n"
}

# MG7: a normal stop leaves nothing of the launch in /dev/shm.
shm_stop() { # name label
  local n=$1 l=$2
  check MG7 "$l stop" "after a normal stop, /dev/shm holds no name that was not there before the start"
  ev shm --out "$RES/mg7-$l-shm-before.txt"
  startd "$n" || { result MG7 "$l stop" fail "start failed"; return; }
  stopd "$n"
  if [ "$PLAN" = 0 ]; then
    "${HELPER[@]}" shm --out "$RES/mg7-$l-shm-after.txt"
    local extra
    extra=$(comm -13 "$RES/mg7-$l-shm-before.txt" "$RES/mg7-$l-shm-after.txt" | tr '\n' ' ')
    [ -z "$extra" ] && result MG7 "$l stop" pass "no new /dev/shm names" || result MG7 "$l stop" fail "left in /dev/shm: $extra"
  fi
}

# MG8: rank 1 stopped, then a request; tp_stalled after the stall timeout plus the probe.
rank_hang() { # name label
  local n=$1 l=$2 seq0 hold st
  check MG8 "$l" "SIGSTOP to rank 1, then a streamed request: no first token, so tp_stalled within stall timeout ($STALL s) + probe (60 s); the instance stopped; charge kept until every process is gone"
  startd "$n" || { result MG8 "$l" fail "start failed"; return; }
  seq0=$(event_seq)
  signal_rank "$n" 1 STOP || { result MG8 "$l" fail "no rank 1 identity recorded"; stopd "$n"; return; }
  if [ "$PLAN" = 0 ]; then
    "${HELPER[@]}" hold --cred "$CRED" --endpoint "$EP" --model "$n" --max-tokens 256 --out "$RES/mg8-$l-hold.json" &
    hold=$!
    st=$(poll_state "$n" $((STALL + 180)) failed stopped)
    events_since "$seq0" tp_stalled >"$RES/mg8-$l-events.jsonl"
    wait "$hold" 2>/dev/null
    if [ -s "$RES/mg8-$l-events.jsonl" ] && waitgone; then
      result MG8 "$l" pass "state $st; tp_stalled recorded; every engine process gone"
    else
      result MG8 "$l" fail "state '$st'; tp_stalled events $(wc -l <"$RES/mg8-$l-events.jsonl")"
    fi
  fi
  stopd "$n"
}

# MG9: throughput through the router against a hand launch with the same arguments.
bench_pair() { # name engine label
  local n=$1 e=$2 l=$3 log args uuids port=18799 hp
  check MG9 "$l" "router tokens/s within 10 % of the same build launched by hand with CapyCTL's rendered arguments and environment, at C1, C4, C8 x 256 tokens x 3 rounds"
  startd "$n" || { result MG9 "$l" fail "start failed"; return; }
  for cc in 1 4 8; do
    run "${HELPER[@]}" bench --cred "$CRED" --endpoint "$EP" --model "$n" --concurrency "$cc" --tokens 256 --rounds 3 --out "$RES/mg9-$l-capyctl-c$cc.json" >>"$RES/cli.log"
  done
  if [ "$PLAN" = 0 ]; then
    log=$(find "$SD/logs" -type f -name '*.log' -newer "$RES/start-$n.out" -printf '%T@ %p\n' 2>/dev/null | sort -n | tail -1 | cut -d' ' -f2-)
    [ -n "$log" ] || log=$(find "$SD/logs" -type f -name '*.log' -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2-)
    args=$("${HELPER[@]}" engine-args --engine "$([ "$e" = v32 ] && echo vllm || echo sglang)" "$log")
    echo "$args" >"$RES/mg9-$l-args.json"
  fi
  stopd "$n"
  if [ "$PLAN" = 1 ]; then uuids='<uuid of gpu0>,<uuid of gpu1>'; else
    uuids=$(nvidia-smi --query-gpu=uuid --format=csv,noheader | head -2 | paste -sd,)
  fi
  # The loopback pin CapyCTL renders for every single-host launch (runtime/loopback_rendezvous.py),
  # and the toolchain part of its engine environment: the engine's bin and the
  # profile's <cuda_home>/bin on PATH, CUDA_HOME (adapters' engine_env.rs).
  local envs=(CUDA_VISIBLE_DEVICES="$uuids" NCCL_SOCKET_IFNAME=lo GLOO_SOCKET_IFNAME=lo VLLM_HOST_IP=127.0.0.1 HF_HUB_OFFLINE=1)
  local venv=$V32 cuda
  [ "$e" = v32 ] || venv=$S22
  cuda=$(cuda_dir)
  if [ -n "$cuda" ]; then envs+=(CUDA_HOME="$cuda" PATH="$venv/bin:$cuda/bin:$PATH"); else envs+=(PATH="$venv/bin:$PATH"); fi
  local model
  model=$(find "$SOURCES" -maxdepth 3 -type d -name "${Q30_REPO//\//--}@*" 2>/dev/null | head -1)
  if [ "$e" = v32 ]; then
    hp=(env "${envs[@]}" "$V32/bin/python" -c "$HAND_VLLM" "$model" "$port" "$RES/mg9-$l-args.json")
  else
    hp=(env "${envs[@]}" "$S22/bin/python" -c "$HAND_SGLANG" "$model" "$port" "$RES/mg9-$l-args.json")
  fi
  if [ "$PLAN" = 1 ]; then show "\$ ${hp[*]} &   (by hand; then wait for /v1/models on 127.0.0.1:$port)"; else
    setsid nohup "${hp[@]}" >"$RES/mg9-$l-hand.log" 2>&1 </dev/null &
    local hand=$! _
    for _ in $(seq 1800); do curl -fs "http://127.0.0.1:$port/v1/models" >/dev/null 2>&1 && break; sleep 1; done
    for cc in 1 4 8; do
      "${HELPER[@]}" bench --no-key --endpoint "http://127.0.0.1:$port" --model "$model" --concurrency "$cc" --tokens 256 --rounds 3 --out "$RES/mg9-$l-hand-c$cc.json" >>"$RES/cli.log"
    done
    kill -TERM -- "-$hand" 2>/dev/null
    waitgone
    local r
    r=$(python3 - "$RES" "$l" <<'PY'
import json, sys
res, label = sys.argv[1:3]
parts, ok = [], True
for c in (1, 4, 8):
    try:
        a = json.load(open(f"{res}/mg9-{label}-capyctl-c{c}.json"))["tokens_per_s"]
        b = json.load(open(f"{res}/mg9-{label}-hand-c{c}.json"))["tokens_per_s"]
    except (OSError, KeyError, ValueError):
        print("fail\tmissing benchmark results"); sys.exit()
    ratio = a / b if b else 0
    ok = ok and ratio >= 0.9
    parts.append(f"C{c} {a:.1f} vs {b:.1f} tok/s ({ratio:.0%})")
print(("pass" if ok else "fail") + "\t" + "; ".join(parts))
PY
)
    result MG9 "$l" "${r%%$'\t'*}" "${r#*$'\t'}"
  fi
}
# The hand launches: the engine's own arguments as CapyCTL's launch recorded them.
HAND_VLLM='
import json, os, sys
model, port, args = sys.argv[1], sys.argv[2], json.load(open(sys.argv[3]))
argv = ["vllm", "serve", model, "--host", "127.0.0.1", "--port", port]
for k, v in sorted(args.items()):
    flag = "--" + k.replace("_", "-")
    if v is True: argv.append(flag)
    elif v is False or v is None: continue
    elif isinstance(v, (dict, list)): argv += [flag, json.dumps(v)]
    else: argv += [flag, str(v)]
os.execv(os.path.join(os.path.dirname(sys.executable), "vllm"), argv)
'
HAND_SGLANG='
import json, os, sys
model, port, args = sys.argv[1], sys.argv[2], json.load(open(sys.argv[3]))
argv = [sys.executable, "-m", "sglang.launch_server", "--model-path", model, "--host", "127.0.0.1", "--port", port]
keep = ("tp_size", "mem_fraction_static", "context_length", "max_running_requests", "chunked_prefill_size",
        "max_total_tokens", "kv_cache_dtype", "attention_backend", "cuda_graph_max_bs", "disable_cuda_graph",
        "page_size", "schedule_policy", "dtype", "trust_remote_code")
for k in keep:
    v = args.get(k)
    flag = "--" + k.replace("_", "-")
    if v is True: argv.append(flag)
    elif v is False or v is None: continue
    else: argv += [flag, str(v)]
os.execv(sys.executable, argv)
'

# MG4, MG5, MG6: park and wake cycles; per-GPU readings after each.
cycles() { # row name count label
  local row=$1 n=$2 k=$3 l=$4 i st r pids ok=0
  check "$row" "$l" "$k park and wake cycles: each park settles (parked) with both GPUs' owned bytes down; each wake Ready with the canary request answering 42 and both GPUs' bytes back"
  startd "$n" || { result "$row" "$l" fail "start failed"; return; }
  for i in $(seq "$k"); do
    c park deployment "$n" >>"$RES/cli.log" 2>&1
    st=$(poll_state "$n" 600 parked failed)
    pids=$(owned_pids "$n")
    ev gpus --pids "$pids" >"$RES/$row-$l-park$i-gpus.json"
    ev charge "$DB" "$n" >"$RES/$row-$l-park$i-charge.json"
    [ "$st" = parked ] || { result "$row" "$l" fail "cycle $i: park reached '$st'"; stopd "$n"; return; }
    c start deployment "$n" --wait >>"$RES/cli.log" 2>&1
    r=$(chat "$n" "$row-$l-wake$i")
    ev gpus --pids "$(owned_pids "$n")" >"$RES/$row-$l-wake$i-gpus.json"
    [ "${r%% *}" = 200 ] && [ "${r##* }" = ok ] || { result "$row" "$l" fail "cycle $i: wake canary ${r:-none}"; stopd "$n"; return; }
    ok=$((ok + 1))
  done
  if [ "$PLAN" = 0 ]; then
    r=$(python3 - "$RES" "$row" "$l" "$k" <<'PY'
import json, sys
res, row, label, k = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
lines = []
ok = True
for i in range(1, k + 1):
    p = json.load(open(f"{res}/{row}-{label}-park{i}-gpus.json"))["owned_mib"]
    w = json.load(open(f"{res}/{row}-{label}-wake{i}-gpus.json"))["owned_mib"]
    ok = ok and len(w) >= 2 and all(p.get(g, 0) < w[g] for g in w)
    lines.append(f"{i}: parked {p} woken {w}")
print(("pass" if ok else "fail") + "\t" + " | ".join(lines))
PY
)
    result "$row" "$l" "${r%%$'\t'*}" "${r#*$'\t'} ($ok/$k cycles)"
  fi
  stopd "$n"
}

refusal() { # row part file expected-code expected-exit: refused at deploy, or at start when deploy accepts it
  local row=$1 part=$2 f=$3 code=$4 want=$5 rc out
  check "$row" "$part" "refused $code (exit $want), at deploy or, when deploy accepts it, at start"
  c deploy model --file "$MG/dep/$f.yaml" >"$RES/$row-$part.out" 2>&1
  rc=$?
  if [ "$rc" = 0 ] && [ "$PLAN" = 0 ]; then
    DEPLOYS="$DEPLOYS $f"
    c start deployment "$f" --wait >>"$RES/$row-$part.out" 2>&1
    rc=$?
  fi
  [ "$PLAN" = 1 ] && return 0
  out=$(tr '\n' ' ' <"$RES/$row-$part.out" | cut -c1-400)
  if [ "$rc" = "$want" ] && tr '\n' ' ' <"$RES/$row-$part.out" | grep -qE -- "$code"; then
    result "$row" "$part" pass "exit $rc: $out"
  else
    result "$row" "$part" fail "exit $rc (wanted $want): $out"
  fi
}

# ---------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------
bandwidth() {
  want BW MG1 MG2 MG4 MG5 MG6 MG7 MG8 MG9 MG10 MG13 MG14 || return 0
  phase "download bandwidth test: $Q30_REPO and $Q4_REPO from Hugging Face through CapyCTL into $SOURCES"
  dep bw-q30 v32 "$(q30)" "residency: restart_only"
  dep bw-q4 v32 "$(q4)" "residency: restart_only"
  local s0 s1 bytes st
  bytes=$(du -sb "$SOURCES" 2>/dev/null | cut -f1)
  s0=$(now)
  deploy bw-q30 || { want BW && result BW download fail "deploy refused; see deploy-bw-q30.out"; say "the source deploy was refused; see deploy-bw-q30.out"; return 1; }
  st=$(poll_sources bw-q30 7200)
  s1=$(since "$s0")
  if [ "$PLAN" = 0 ]; then
    local got
    got=$(( $(du -sb "$SOURCES" | cut -f1) - ${bytes:-0} ))
    # Run only for the sources of other rows (--only without BW), the
    # download is not the bandwidth test and records nothing.
    if ! want BW; then :
    elif [ "$st" = verified ]; then
      result BW q30 pass "$(gib "$got") GiB in $s1 s = $(awk -v b="$got" -v s="$s1" 'BEGIN {printf "%.1f MB/s", b / s / 1e6}')"
    else
      result BW q30 fail "sources $st after $s1 s"
    fi
  fi
  deploy bw-q4 && poll_sources bw-q4 3600 >/dev/null
  c delete deployment bw-q30 >>"$RES/cli.log" 2>&1
  c delete deployment bw-q4 >>"$RES/cli.log" 2>&1
  DEPLOYS=""
}

rows_shipped() {
  want BW MG1 MG2 MG3 MG7 MG8 MG9 MG12 MG14 || return 0
  phase "role s1: the shipped build (deep parking at N > 1 off for every engine)"
  role_up s1 capyctl || { say "role s1 did not start"; return; }
  profiles
  bandwidth

  if want MG12; then
    phase "MG12 refusals"
    dep mg12-tp3 v32 "$(q30)" "topology: {tensor_parallel: 3}"
    refusal MG12 tp3 mg12-tp3 insufficient_gpus 4
    dep mg12-pin v32 "$(q30)" "topology: {tensor_parallel: 2}
devices: [{id: gpu0}]"
    refusal MG12 pin mg12-pin topology_devices_mismatch 2
  fi
  if want MG14; then
    phase "MG14 refusals name what would fit"
    dep mg14-tp1 v32 "$(q30)" "topology: {tensor_parallel: 1}"
    refusal MG14 tp1 mg14-tp1 'insufficient_device_memory.*tensor_parallel: 2' 4
    dep mg14-auto v32 "$(q30)" "topology: auto
engine_config: {memory: {kv_cache: 40GiB}}"
    refusal MG14 auto mg14-auto 'insufficient_device_memory.*tensor_parallel: 2 each GPU is charged' 4
  fi

  dep mg-v v32 "$(q30)" "topology: {tensor_parallel: 2}
engine_config: {context_length: 32768}"
  dep mg-s s22 "$(q30)" "topology: {tensor_parallel: 2}
engine_config: {context_length: 32768}"
  dep mg-l lc "$GGUF_DIR" "topology: {tensor_parallel: 2}
engine_config: {context_length: 16384}"
  if want MG1 MG7 MG8 MG9; then
    deploy mg-v || result MG1 deploy fail "refused: $(tr '\n' ' ' <"$RES/deploy-mg-v.out" | cut -c1-300)"
    if want MG1; then phase "MG1 vLLM 0.32.0 TP 2"; serve_check MG1 mg-v v32; stopd mg-v; fi
    if want MG7; then phase "MG7 vLLM"; shm_stop mg-v vllm; rank_kill mg-v vllm; fi
    if want MG8; then phase "MG8 vLLM"; rank_hang mg-v vllm; fi
    if want MG9; then phase "MG9 vLLM"; bench_pair mg-v v32 vllm; fi
  fi
  if want MG2 MG7 MG8 MG9; then
    deploy mg-s || result MG2 deploy fail "refused: $(tr '\n' ' ' <"$RES/deploy-mg-s.out" | cut -c1-300)"
    if want MG2; then phase "MG2 SGLang 0.5.22 TP 2"; serve_check MG2 mg-s s22; stopd mg-s; fi
    if want MG7; then phase "MG7 SGLang"; shm_stop mg-s sglang; rank_kill mg-s sglang; fi
    if want MG8; then phase "MG8 SGLang"; rank_hang mg-s sglang; fi
    if want MG9; then phase "MG9 SGLang"; bench_pair mg-s s22 sglang; fi
  fi
  if want MG3; then
    phase "MG3 llama.cpp $LC_TAG, two GPUs, layer split"
    deploy mg-l || result MG3 deploy fail "refused: $(tr '\n' ' ' <"$RES/deploy-mg-l.out" | cut -c1-300)"
    serve_check MG3 mg-l lc
    stopd mg-l
  fi
  role_down
}

rows_qualify() {
  want MG4 MG5 MG6 MG10 MG13 || return 0
  phase "role s2: the qualification build (vLLM and SGLang deep parking at N > 1 on, as MG4/MG5 would set it)"
  role_up s2 capyctl-qualify || { say "role s2 did not start"; return; }
  profiles
  if want MG4; then
    phase "MG4 vLLM TP 2 deep"
    dep mg4 v32 "$(q30)" "topology: {tensor_parallel: 2}
residency: deep
engine_config: {context_length: 32768}"
    deploy mg4 && cycles MG4 mg4 "$CYCLES" deep
  fi
  if want MG5; then
    phase "MG5 SGLang TP 2 deep"
    dep mg5 s22 "$(q30)" "topology: {tensor_parallel: 2}
residency: deep
engine_config: {context_length: 32768}"
    deploy mg5 && cycles MG5 mg5 "$CYCLES" deep
  fi
  if want MG6; then
    phase "MG6 host_backed, vLLM and SGLang TP 2"
    if [ "$PLAN" = 0 ] && [ "$(awk '/^MemTotal:/ {print int($2/1048576)}' /proc/meminfo)" -lt 120 ]; then
      skip MG6 both "under 120 GiB of host RAM: the host_backed copy of the bring-up model does not fit"
    else
      dep mg6-v v32 "$(q30)" "topology: {tensor_parallel: 2}
residency: host_backed
engine_config: {context_length: 32768}"
      dep mg6-s s22 "$(q30)" "topology: {tensor_parallel: 2}
residency: host_backed
engine_config: {context_length: 32768}"
      deploy mg6-v && cycles MG6 mg6-v "$HB_CYCLES" vllm
      ev charge "$DB" mg6-v >"$RES/mg6-vllm-charge-after.json"
      deploy mg6-s && cycles MG6 mg6-s "$HB_CYCLES" sglang
      ev charge "$DB" mg6-s >"$RES/mg6-sglang-charge-after.json"
      check MG6 released "after each stop the ledger holds no reservation of the deployment (host copy included)"
      if [ "$PLAN" = 0 ]; then
        local left
        left=$(cat "$RES/mg6-vllm-charge-after.json" "$RES/mg6-sglang-charge-after.json" | tr -d ' \n')
        [ "$left" = "{}{}" ] && result MG6 released pass "no reservation left after either stop" ||
          result MG6 released fail "left: $left"
      fi
    fi
  fi
  if want MG10; then
    phase "MG10 switching: a TP 2 model parked, a single-GPU model on gpu0, then the TP 2 model requested"
    dep mg10-tp2 s22 "$(q30)" "topology: {tensor_parallel: 2}
residency: deep
engine_config: {context_length: 32768}"
    dep mg10-one v32 "$(q4)" "devices: [{id: gpu0}]
residency: restart_only
engine_config: {context_length: 32768, memory: {kv_cache: 30GiB}}"
    local seq0 st r
    if deploy mg10-tp2 && deploy mg10-one && startd mg10-tp2; then
      c park deployment mg10-tp2 >>"$RES/cli.log" 2>&1
      poll_state mg10-tp2 600 parked >/dev/null
      startd mg10-one
      seq0=$(event_seq)
      check MG10 switch "a request to the parked TP 2 model wakes it: the planner evicts the single-GPU model; the switch record names gpu0 and gpu1"
      r=$(chat mg10-tp2 mg10)
      st=$(state_of mg10-one stopped)
      if [ "$PLAN" = 0 ]; then
        events_since "$seq0" >"$RES/mg10-events.jsonl"
        "${HELPER[@]}" sql "$DB" "SELECT switch_id,phase,victims_json FROM active_switches" >"$RES/mg10-switches.tsv"
        if [ "${r%% *}" = 200 ] && [ "$st" != ready ] && grep -q gpu0 "$RES/mg10-events.jsonl" && grep -q gpu1 "$RES/mg10-events.jsonl"; then
          result MG10 switch pass "request ${r}; single-GPU model $st; events name gpu0 and gpu1"
        else
          result MG10 switch fail "request ${r:-none}; single-GPU model '$st'; see mg10-events.jsonl"
        fi
      fi
      stopd mg10-tp2
      stopd mg10-one
    else
      result MG10 switch fail "deploy or start failed; see cli.log"
    fi
  fi
  if want MG13; then
    phase "MG13 topology: auto"
    dep mg13-4b v32 "$(q4)" "topology: auto
engine_config: {context_length: 32768}"
    dep mg13-4b-x v32 "$(q4)" "devices: [{id: gpu0}]
engine_config: {context_length: 32768}"
    dep mg13-30b s22 "$(q30)" "topology: auto
residency: deep
engine_config: {context_length: 32768}"
    local tp devs devs2 a b txt
    if deploy mg13-4b && deploy mg13-4b-x && deploy mg13-30b; then
      check MG13 n1 "the 4B model chooses N = 1, status 'TP auto (1)', and its vLLM arguments equal an explicit single-GPU deployment's"
      startd mg13-4b-x
      a=$(latest_args vllm); stopd mg13-4b-x
      startd mg13-4b
      b=$(latest_args vllm)
      status_json mg13-4b "$RES/mg13-4b.json"; c status deployment mg13-4b >"$RES/mg13-4b.txt" 2>&1
      if [ "$PLAN" = 0 ]; then
        tp=$("${HELPER[@]}" get "$RES/mg13-4b.json" 'instances[0].topology.tensor_parallel')
        txt=$(grep -c 'TP auto (1)' "$RES/mg13-4b.txt")
        [ "$tp" = 1 ] && [ "$txt" -gt 0 ] && [ "$a" = "$b" ] && [ -n "$a" ] &&
          result MG13 n1 pass "tensor_parallel 1; 'TP auto (1)'; arguments equal" ||
          result MG13 n1 fail "tensor_parallel '$tp'; TP auto lines $txt; arguments equal: $([ "$a" = "$b" ] && echo yes || echo no)"
      fi
      check MG13 n2 "with the 4B model serving on gpu0, the 30B model started with --evict chooses N = 2 (status 'TP auto (2)'), its SGLang arguments equal MG2's, and the 4B model is evicted"
      startd mg13-30b --evict
      status_json mg13-30b "$RES/mg13-30b.json"; c status deployment mg13-30b >"$RES/mg13-30b.txt" 2>&1
      b=$(latest_args sglang)
      if [ "$PLAN" = 0 ]; then
        tp=$("${HELPER[@]}" get "$RES/mg13-30b.json" 'instances[0].topology.tensor_parallel')
        devs=$("${HELPER[@]}" get "$RES/mg13-30b.json" 'instances[0].topology.devices')
        a=$(cat "$RES/mg9-sglang-args.json" 2>/dev/null)
        st=$(state_of mg13-4b)
        [ "$tp" = 2 ] && grep -q 'TP auto (2)' "$RES/mg13-30b.txt" && [ "$st" != ready ] && { [ -z "$a" ] || [ "$a" = "$b" ]; } &&
          result MG13 n2 pass "tensor_parallel 2 on $devs; 4B model $st; arguments $([ -z "$a" ] && echo "not compared (no MG9 SGLang record)" || echo "equal MG2's")" ||
          result MG13 n2 fail "tensor_parallel '$tp'; 4B model '$st'; arguments equal MG2's: $([ "$a" = "$b" ] && echo yes || echo no)"
      fi
      check MG13 wake "the 30B model parked and woken keeps N = 2 and the same GPUs"
      c park deployment mg13-30b >>"$RES/cli.log" 2>&1
      poll_state mg13-30b 600 parked >/dev/null
      c start deployment mg13-30b --wait >>"$RES/cli.log" 2>&1
      status_json mg13-30b "$RES/mg13-30b-woken.json"
      if [ "$PLAN" = 0 ]; then
        devs2=$("${HELPER[@]}" get "$RES/mg13-30b-woken.json" 'instances[0].topology.devices')
        tp=$("${HELPER[@]}" get "$RES/mg13-30b-woken.json" 'instances[0].topology.tensor_parallel')
        r=$(chat mg13-30b mg13-wake)
        [ "$tp" = 2 ] && [ "$devs" = "$devs2" ] && [ "${r%% *}" = 200 ] &&
          result MG13 wake pass "tensor_parallel 2 on $devs2; chat $r" ||
          result MG13 wake fail "tensor_parallel '$tp'; devices '$devs' then '$devs2'; chat ${r:-none}"
      fi
      stopd mg13-30b
      stopd mg13-4b
    else
      result MG13 deploy fail "deploy refused; see deploy-mg13-*.out"
    fi
  fi
  role_down
}
latest_args() { # engine: the newest launch's own argument record (mg.py engine-args)
  [ "$PLAN" = 1 ] && { echo "{}"; return 0; }
  local log
  log=$(find "$SD/logs" -type f -name '*.log' -printf '%T@ %p\n' 2>/dev/null | sort -n | tail -1 | cut -d' ' -f2-)
  [ -n "$log" ] && "${HELPER[@]}" engine-args --engine "$1" "$log"
}

# MG11: the machine enrolled as a host of a control-plane server on loopback, and
# an older host (OLD_TAG, without local_tensor_parallel) beside it.
rows_enrolled() {
  want MG11 || return 0
  phase "MG11 enrolled host: server, host (this build) and an older host ($OLD_TAG) on loopback"
  if [ "$PLAN" = 0 ] && [ ! -x "$BIN/capyctl-old" ]; then skip MG11 old "no $OLD_TAG build"; fi
  local srv=$MG/srv h1=$MG/h1 h0=$MG/h0 r out rc
  ROLE=srv SD=$srv CAPYCTL=$BIN/capyctl CRED=$srv/identity/server-credentials.json DB=$srv/server/srv.sqlite3
  run mkdir -m 700 -p "$srv" "$h1" "$h0" "$MG/cfg-srv"
  run env CAPYCTL_STATE_DIR="$srv" "$CAPYCTL" init server --output "$srv/init.json"
  run python3 - "$srv/init.json" "$srv/server.yaml" "$PORT" "$MPORT" <<'PY'
import json, os, sys
init, path, infer, mgmt = sys.argv[1:5]
doc = json.load(open(init))
doc["listeners"]["bootstrap"]["bind"] = "127.0.0.1:18747"
doc["listeners"]["control"]["bind"] = "127.0.0.1:18748"
doc["listeners"]["management"]["bind"] = f"127.0.0.1:{mgmt}"
doc["listeners"]["inference"]["bind"] = f"127.0.0.1:{infer}"
doc["enrollment"] = {"bootstrap_address": "https://127.0.0.1:18747", "control_address": "https://127.0.0.1:18748"}
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as handle:
    json.dump(doc, handle, indent=2)
PY
  if [ "$PLAN" = 1 ]; then show "\$ setsid capyctl start server --config $srv/server.yaml &"; else
    setsid nohup "$CAPYCTL" start server --config "$srv/server.yaml" >"$RES/role-srv.log" 2>&1 </dev/null &
    echo $! >"$MG/srv.pid"
    sleep 10
  fi
  enroll_host() { # name dir binary ingress-port
    local name=$1 dir=$2 bin=$3 iport=$4
    run "$CAPYCTL" invite host "$name" --config "$srv/server.yaml" --output "$dir/$name.join"
    run chmod 600 "$dir/$name.join"
    run env CAPYCTL_STATE_DIR="$dir/state" XDG_CONFIG_HOME="$dir/cfg" "$bin" init host --output "$dir/host.yaml"
    run python3 - "$dir/host.yaml" "$name" "$iport" <<'PY'
import sys, re
path, name, port = sys.argv[1:4]
text = open(path).read()
text = re.sub(r"(?m)^name:.*$", f"name: {name}", text)
text += f'\ningress:\n  transport: trusted_private_link\n  address: "http://127.0.0.1:{port}"\n  bind: "127.0.0.1:{port}"\n'
open(path, "w").write(text)
PY
    run env XDG_CONFIG_HOME="$dir/cfg" "$bin" join host --join-file "$dir/$name.join" --config "$dir/host.yaml"
    if [ "$PLAN" = 1 ]; then show "\$ setsid $bin start host --config $dir/host.yaml &"; else
      XDG_CONFIG_HOME="$dir/cfg" setsid nohup "$bin" start host --config "$dir/host.yaml" >"$RES/role-$name.log" 2>&1 </dev/null &
      echo $! >"$dir/host.pid"
      sleep 10
    fi
    local cuda=()
    [ -z "$(cuda_dir)" ] || cuda=(CUDA_HOME="$(cuda_dir)")
    run env "${cuda[@]}" XDG_CONFIG_HOME="$dir/cfg" "$bin" engine add "$V32" --name v32 --config "$dir/host.yaml"
  }
  enroll_host mg-new "$h1" "$BIN/capyctl" 18749
  [ -x "$BIN/capyctl-old" ] || [ "$PLAN" = 1 ] && enroll_host mg-old "$h0" "$BIN/capyctl-old" 18750
  pause 20
  dep mg11-new v32 "$(q30)" "host: mg-new
topology: {tensor_parallel: 2}
engine_config: {context_length: 32768}"
  dep mg11-old v32 "$(q30)" "host: mg-old
topology: {tensor_parallel: 2}
engine_config: {context_length: 32768}"
  check MG11 remote "MG1's deployment on the enrolled host: Ready and answering through the server's router"
  srvc() { run env XDG_CONFIG_HOME="$MG/cfg-srv" "$CAPYCTL" --config "$srv/server.yaml" "$@"; }
  srvc deploy model --file "$MG/dep/mg11-new.yaml" >"$RES/mg11-new-deploy.out" 2>&1
  srvc start deployment mg11-new --wait >"$RES/mg11-new-start.out" 2>&1
  rc=$?
  r=$(chat mg11-new mg11)
  if [ "$PLAN" = 0 ]; then
    [ "$rc" = 0 ] && [ "${r%% *}" = 200 ] && result MG11 remote pass "Ready; chat $r" || result MG11 remote fail "start exit $rc; chat ${r:-none}"
  fi
  srvc stop deployment mg11-new >>"$RES/cli.log" 2>&1
  waitgone
  check MG11 old "the same deployment on the older host is refused host_capability_missing:local_tensor_parallel and no engine starts"
  srvc deploy model --file "$MG/dep/mg11-old.yaml" >"$RES/mg11-old-deploy.out" 2>&1
  srvc start deployment mg11-old --wait >"$RES/mg11-old-start.out" 2>&1
  rc=$?
  if [ "$PLAN" = 0 ]; then
    out=$(cat "$RES/mg11-old-deploy.out" "$RES/mg11-old-start.out" | tr '\n' ' ' | cut -c1-400)
    if [ "$rc" != 0 ] && grep -q 'host_capability_missing:local_tensor_parallel' "$RES/mg11-old-deploy.out" "$RES/mg11-old-start.out" && ! engines_running; then
      result MG11 old pass "refused (exit $rc): $out"
    else
      result MG11 old fail "exit $rc: $out"
    fi
  fi
  srvc delete deployment mg11-new --stop >>"$RES/cli.log" 2>&1
  srvc delete deployment mg11-old --stop >>"$RES/cli.log" 2>&1
  waitgone
  local pf pid
  for pf in "$h1/host.pid" "$h0/host.pid" "$MG/srv.pid"; do
    [ "$PLAN" = 0 ] && [ -f "$pf" ] && pid=$(cat "$pf") && kill -TERM "$pid" 2>/dev/null
  done
  pause 10
  ROLE=""
}

run_rows() {
  # A follow-up run (--only) replaces the results of its own rows and keeps
  # the others', so verdict.md covers the whole session.
  if [ "$PLAN" = 0 ]; then
    mkdir -p "$RES"
    if [ "$ONLY" = all ] || [ ! -f "$RES/results.tsv" ]; then : >"$RES/results.tsv"; else
      awk -F'\t' -v only=",$ONLY," 'index(only, "," $1 ",") == 0' "$RES/results.tsv" >"$RES/results.tsv.keep"
      mv "$RES/results.tsv.keep" "$RES/results.tsv"
    fi
  fi
  if [ -f "$MG/revs.env" ]; then
    # shellcheck disable=SC1091
    . "$MG/revs.env"
  fi
  if [ "$PLAN" = 0 ] && engines_running; then say "an engine process is already running; one live session at a time"; exit 1; fi
  rows_shipped
  rows_qualify
  rows_enrolled
  say "RUN_DONE"
}

down() {
  local pf pid
  for pf in "$MG/role.pid" "$MG/srv.pid" "$MG/h1/host.pid" "$MG/h0/host.pid"; do
    [ -f "$pf" ] || continue
    pid=$(cat "$pf")
    if [ -r "/proc/$pid/cmdline" ] && tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q capyctl; then
      kill -TERM "$pid" 2>/dev/null
    fi
    rm -f "$pf"
  done
  waitgone || echo "warning: an engine process is still running"
}

purge() {
  down
  [ -f "$CREATED" ] || { echo "nothing recorded in $CREATED"; return 0; }
  local p
  # The engine directories, then the work directory itself (the last line).
  tac "$CREATED" | while read -r p; do
    case $p in "$HOME"/*) ;; *) echo "refusing to remove $p (outside ~)"; continue ;; esac
    echo "remove $p"
    rm -rf -- "$p"
  done
  echo PURGED
}

case $cmd in
  preflight) preflight ;;
  setup) setup ;;
  run) run_rows ;;
  plan) mkdir -p "$RES" "$MG/dep"; setup; run_rows ;;
  # run.sh starts this one detached: setup unless SKIP_SETUP=1, then the rows; the
  # exit status goes to $MG/done, which run.sh waits for.
  all)
    rm -f "$MG/done"
    if [ "${SKIP_SETUP:-0}" = 1 ] || setup; then (run_rows); rc=$?; else rc=3; fi
    down
    echo "$rc" >"$MG/done"
    ;;
  down) down ;;
  purge) purge ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
