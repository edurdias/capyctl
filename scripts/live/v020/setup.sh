#!/usr/bin/env bash
# Source from remote.sh; sourcing this file has no host-side effects.
# Lab setup accepts read-only engine overrides; fresh engine installs remain VM-only.

setup_plan() {
  cat <<EOF
Setup (rented x86_64 VM only; approximately 60 minutes, downloads/installations parallel):
  1. Refuse FUNCTIONAL_ONLY=1: use --skip-setup and --engines on an existing host.
     Require C/C++ compilers, make, git, curl, tar, python3, flock and nproc;
     no sudo, system packages, driver changes, or lab environment modifications.
  2. Keep every writable tool, cache, model and engine below \$V020 (${V020:-<session root>}).
     Record each owned directory in created.txt BEFORE creating it; refuse existing
     unmarked engine directories. Supplied --engines paths are read-only overrides.
  3. Install session-local uv and managed Python 3.12; install cmake, ninja and
     huggingface_hub in a tools venv. Put uv/Python/HF/JIT/XDG/temp caches in V020.
  4. Use the image's nvcc, or install cuda-toolkit[nvcc,cccl,cudart,cublas,curand]==13.0.*
     into a session venv and assemble its lib/lib64 links there, never in /usr/local.
  5. Resolve missing HF revisions ONCE and persist models.lock.json BEFORE downloads:
     MODEL_CHAT: Qwen/Qwen3-4B-Instruct-2507, resolved main.
     MODEL_CHAT_GGUF: Qwen/Qwen3-4B-GGUF, resolved main, Qwen3-4B-Q4_K_M.gguf.
     MODEL_EMBED: Qwen/Qwen3-Embedding-4B @5cf2132.
     MODEL_EMBED_GGUF: Qwen/Qwen3-Embedding-4B-GGUF, resolved main, Q8_0.
     MODEL_EMBED8: Qwen/Qwen3-Embedding-8B @1d8ad4c.
     MODEL_CLEF: Cloudflare/clef-flash @8b2e5fd1.
     MODEL_CLEF_GGUF: ggml-org/Clef-Flash-GGUF @6b4fe4c5, Q8_0.
     MODEL_TENSORFOLD: TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit
       @d9d758fb83953437f7263256b0d96157e2a348b8.
  6. In parallel, install vLLM 0.32.0 and 0.33.0 with CUDA 13 torch;
     SGLang[tracing] 0.5.22 and 0.5.23 with torch-memory-saver==0.0.10;
     and a separate editable SGLang 0.5.23 checkout/environment for #235.
  7. Recreate separate vLLM 0.33 and SGLang 0.5.23 environments from their freezes
     using uv; source-build LMCache ${LMCACHE_VERSION:-0.5.7} twice against each engine's exact torch.
     Use no build isolation, the GPU's own CUDA arch and CUDA major 13; preserve engine
     constraints and disable built-wheel reuse across the different torch builds.
     Never copy a venv's absolute shebangs.
  8. Build llama.cpp v0.9.0, commit cd959bae99ff4bed22bef2604f45241c09bbb8c8
     (pin source: scripts/live/llamacpp-build.sh at 63f91fc8), for the GPU's arch;
     CUDA ON, Release, IS_DEV/CONNECT/BUILD_UI/USE_PREBUILT_UI OFF; llama-server -j nproc.
  9. Download native TensorFold 1.2.6 and 1.1.5 x86_64 archives; verify SHA256:
     1.2.6 a8abf9b8d6b1109e716996eba18d5c5ce1dbe013a7808b603f81f218e0bcff3d
     1.1.5 71ee0d792b94bbe5419332e75a4c2e3975c863235d1f66ae736938e7c1d91e8a
     Verify each archive's internal SHA256SUMS as well.
     Install Python TensorFold v0.6.6 in its own environment (torch 2.13.0
     cu130), from checksum-verified commit cb2ebf0540f42604e2759b2ddef497861e928248.
     Exact native 1.0.0 has no x86_64 archive: TENSORFOLD_100 is read-only on GB10.
     Install checksum-verified static strace 6.3 in the session for TF5/TF13;
     no system package or ptrace-policy change is made.
 10. Download published CapyCTL v0.1.12 x86_64 release, verify release SHA256SUMS
     and the archive's own SHA256SUMS; install only bin/capyctl as bin/capyctl-old.
 11. Download OpenTelemetry collector 0.164.0 x86_64; verify pinned archive SHA256.
     Setup never starts the collector; rows must bind every receiver to loopback.
 12. For all or explicit --only TR rows, install separate TensorRT-LLM 1.2.1 and
     1.4.0rc3 NVIDIA cp312 wheels, torch 2.9.1/cu130 and 2.14.0/cu132 respectively.
     Pin Open MPI 4.1.8, mpi4py and torch constraints. Install private CUDA Toolkit
     13.1/13.4 wheels; never change the driver or system CUDA. Probe MPI without
     inherited LD_LIBRARY_PATH, and run a tiny forced-PTX kernel for rc3. A driver/JIT
     incompatibility makes rc3 unavailable with diagnostic evidence, not a fake pass.
     TENSORRT_LLM/TRT_LLM name 1.2.1; version-specific engine/CUDA keys are exported.
 13. Download pinned models concurrently with engine jobs. Record sanitized package
     freezes, RECORD/binary fingerprints, source commits, model lock and availability.
     Never save credentials, environment/command-line dumps, or dependency URLs.
 14. Write all engine/model keys plus CUDA_HOME to engines.env; check installation
     artifacts exist. Setup does not claim live qualification or run inference.
EOF
}

v020_setup_error() { printf 'SETUP_FAILED: %s\n' "$*" >&2; return 1; }

# Lock the ledger for concurrent installers. Each child path is checked even if
# its parent is ours: an existing unmarked engine must never be adopted.
v020_claim() (
  local path=$1
  case "$path" in "$V020"/*) ;; *) v020_setup_error 'created path escapes session root'; exit 1 ;; esac
  [ ! -L "$path" ] || { v020_setup_error 'refusing a symlink at an installation path'; exit 1; }
  exec 8>>"$V020/created.txt"
  flock -x 8
  if ! grep -qxF -- "$path" "$V020/created.txt"; then
    [ ! -e "$path" ] || { v020_setup_error 'refusing a pre-existing unmarked installation path'; exit 1; }
    printf '%s\n' "$path" >&8
  fi
)

v020_mkdir() { v020_claim "$1" && mkdir -p -- "$1" && chmod 700 -- "$1"; }

v020_sanitize() {
  python3 -u -c 'import os,re,sys
secrets = [v for k,v in os.environ.items() if re.search(r"TOKEN|PASSWORD|SECRET|API_KEY|AUTH",k,re.I) and len(v)>3]
for line in sys.stdin:
    for value in secrets:
        line=line.replace(value,"[redacted]")
    line=re.sub(r"(?:git\+)?https?://[^\s\x1b]+", "[url omitted]", line)
    line=re.sub(r"\b(?:hf_|gh[pousr]_)[A-Za-z0-9_-]+", "[redacted]", line)
    line=re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", line)
    sys.stdout.write(line)
'
}

v020_cmd() { "$@" 2>&1 | v020_sanitize | tee -a "$EVIDENCE/setup.log"; }

v020_fetch() {
  local url=$1 destination=$2
  v020_cmd curl --proto '=https' --tlsv1.2 -fsSL -o "$destination.part" "$url"
  mv -- "$destination.part" "$destination"
}

v020_check_sha() {
  "$V020/tools/bin/python" - "$1" "$2" "$EVIDENCE/releases.sha256" <<'PY'
import hashlib,pathlib,re,sys
path=pathlib.Path(sys.argv[1]); expected=sys.argv[2]
if not re.fullmatch(r"[0-9a-f]{64}",expected):
    raise SystemExit("invalid release checksum")
h=hashlib.file_digest(path.open("rb"),"sha256").hexdigest()
if h != expected:
    raise SystemExit("release archive checksum mismatch")
print(path.name+" sha256 "+h)
with pathlib.Path(sys.argv[3]).open("a") as evidence:
    evidence.write(h+"  "+path.name+"\n")
PY
}

v020_unpack() {
  "$V020/tools/bin/python" - "$1" "$2" <<'PY'
import pathlib,sys,tarfile
with tarfile.open(sys.argv[1]) as archive:
    archive.extractall(pathlib.Path(sys.argv[2]),filter="data")
PY
}

v020_manifest() {
  "$V020/tools/bin/python" - "$1" <<'PY'
import hashlib,pathlib,re,sys
root=pathlib.Path(sys.argv[1]).resolve(); manifest=root/"SHA256SUMS"
if not manifest.is_file():
    raise SystemExit("archive has no internal SHA256SUMS")
count=0
for line in manifest.read_text().splitlines():
    if not line.strip():
        continue
    checksum,name=line.split(maxsplit=1); name=name.lstrip("*")
    path=(root/name).resolve()
    if not path.is_relative_to(root) or not re.fullmatch(r"[0-9a-f]{64}",checksum):
        raise SystemExit("invalid internal checksum manifest")
    if hashlib.file_digest(path.open("rb"),"sha256").hexdigest()!=checksum:
        raise SystemExit("internal release checksum mismatch")
    count+=1
if not count:
    raise SystemExit("empty internal checksum manifest")
print("internal checksums verified:",count)
PY
}

v020_venv() {
  local path=$1
  v020_claim "$path"
  if [ ! -x "$path/bin/python" ]; then
    # UV_PYTHON_PREFERENCE=only-managed (v020_setup_tools) already selects the
    # managed interpreter; uv 0.13 refuses --managed-python beside it.
    v020_cmd "$BIN/uv" venv --python 3.12 --seed "$path"
  fi
}

# A reproducible, URL-free package list (pip freeze can leak direct_url credentials).
# Hash only installation metadata, not an environment or process command line.
v020_freeze() {
  "$1/bin/python" - "$EVIDENCE/$2.freeze.txt" "$EVIDENCE/$2.fingerprint.json" <<'PY'
import hashlib,importlib.metadata as metadata,json,pathlib,re,sys
packages={}; records={}; editable=[]
for distribution in metadata.distributions():
    name=distribution.metadata.get("Name",""); version=distribution.version
    if not re.fullmatch(r"[A-Za-z0-9_.-]+",name) or not re.fullmatch(r"[A-Za-z0-9_.+!-]+",version):
        raise SystemExit("unsafe package metadata in freeze")
    packages[name]=version
    record=distribution.read_text("RECORD")
    if record is not None:
        records[name]=hashlib.sha256(record.encode()).hexdigest()
    direct=distribution.read_text("direct_url.json")
    if direct and json.loads(direct).get("dir_info",{}).get("editable"):
        editable.append(name)
pathlib.Path(sys.argv[1]).write_text("".join(f"{n}=={v}\n" for n,v in sorted(packages.items(),key=lambda p:p[0].lower())))
pathlib.Path(sys.argv[2]).write_text(json.dumps({"packages":packages,"record_sha256":records,"editable":sorted(editable)},sort_keys=True,indent=2)+"\n")
PY
}

v020_setup_tools() {
  local directory
  for directory in cache data config tmp home downloads engines models tools; do
    v020_mkdir "$V020/$directory"
  done
  export UV_CACHE_DIR=$V020/cache/uv UV_PYTHON_INSTALL_DIR=$V020/data/python
  export UV_PYTHON_CACHE_DIR=$V020/cache/python UV_TOOL_DIR=$V020/data/uv-tools
  export UV_TOOL_BIN_DIR=$BIN UV_NO_MODIFY_PATH=1 UV_PYTHON_PREFERENCE=only-managed
  # Large concurrent CUDA-wheel downloads can hold one cache lock beyond uv's
  # five-minute default; wait for the shared download instead of failing a peer.
  export UV_LOCK_TIMEOUT=1800
  export XDG_CACHE_HOME=$V020/cache XDG_DATA_HOME=$V020/data XDG_CONFIG_HOME=$V020/config
  export HF_HOME=$V020/cache/huggingface HUGGINGFACE_HUB_CACHE=$V020/cache/huggingface/hub
  export HF_HUB_DISABLE_TELEMETRY=1 HF_HUB_DISABLE_PROGRESS_BARS=1
  export TORCH_HOME=$V020/cache/torch TORCH_EXTENSIONS_DIR=$V020/cache/torch-extensions
  export TRITON_CACHE_DIR=$V020/cache/triton FLASHINFER_WORKSPACE_BASE=$V020/cache/flashinfer
  export VLLM_CACHE_ROOT=$V020/cache/vllm CUDA_CACHE_PATH=$V020/cache/cuda
  export PIP_CACHE_DIR=$V020/cache/pip TMPDIR=$V020/tmp PYTHONDONTWRITEBYTECODE=1
  export CARGO_HOME=$V020/data/cargo RUSTUP_HOME=$V020/data/rustup
  export PATH=$BIN:$V020/tools/bin:$PATH
  # Some dependencies ignore XDG and use ~/.nv or ~/.triton. Give all setup
  # subprocesses an isolated HOME; the real home was checked before this point.
  export HOME=$V020/home
  if [ ! -x "$BIN/uv" ]; then
    v020_fetch https://astral.sh/uv/install.sh "$V020/downloads/uv-install.sh"
    v020_cmd env UV_INSTALL_DIR="$BIN" UV_NO_MODIFY_PATH=1 sh "$V020/downloads/uv-install.sh"
  fi
  v020_cmd "$BIN/uv" --version
  v020_venv "$V020/tools"
  v020_cmd "$BIN/uv" pip install --python "$V020/tools/bin/python" cmake ninja huggingface_hub
  v020_freeze "$V020/tools" tools
}

v020_toolkit_compilers() {
  local toolkit=$1 cxx cc
  cxx=$(command -v c++)
  cc=$(command -v "gcc-$(c++ -dumpversion)") || {
    v020_setup_error 'missing C compiler matching the existing C++ compiler'; return 1;
  }
  # Managed engines have a closed PATH. Put the already-installed matched pair
  # beside this private nvcc so JIT builds do not pick the incomplete system gcc.
  [ -e "$toolkit/bin/gcc" ] || ln -s "$cc" "$toolkit/bin/gcc"
  [ -e "$toolkit/bin/g++" ] || ln -s "$cxx" "$toolkit/bin/g++"
}

v020_setup_cuda() {
  local nvcc toolkit library
  if [ -n "${V020_OVERRIDES[CUDA_HOME]:-}" ]; then
    toolkit=$CUDA_HOME
  elif [ -x /usr/local/cuda/bin/nvcc ]; then
    toolkit=/usr/local/cuda
  elif command -v nvcc >/dev/null 2>&1; then
    nvcc=$(command -v nvcc)
    toolkit=$(dirname "$(dirname "$(readlink -f "$nvcc")")")
  else
    v020_venv "$V020/cuda-venv"
    v020_cmd "$BIN/uv" pip install --python "$V020/cuda-venv/bin/python" \
      'cuda-toolkit[nvcc,cccl,cudart,cublas,curand,cusparse,cusolver]==13.0.*'
    toolkit=$("$V020/tools/bin/python" - "$V020/cuda-venv" <<'PY'
import pathlib,sys
paths=sorted(pathlib.Path(sys.argv[1]).rglob("bin/nvcc"))
if len(paths)!=1:
    raise SystemExit("CUDA wheel layout has no unique nvcc")
print(paths[0].parent.parent)
PY
)
    # Only the session's wheel toolkit may be modified.
    case "$toolkit" in "$V020/cuda-venv/"*) ;; *) v020_setup_error 'CUDA wheel escaped session'; return 1 ;; esac
    for library in "$toolkit"/lib/*.so.*; do
      [ -f "$library" ] || continue
      nvcc=${library%%.so.*}.so
      [ -e "$nvcc" ] || ln -s "$(basename "$library")" "$nvcc"
    done
    [ -e "$toolkit/lib64" ] || ln -s lib "$toolkit/lib64"
    v020_toolkit_compilers "$toolkit"
    v020_freeze "$V020/cuda-venv" cuda
  fi
  [ -x "$toolkit/bin/nvcc" ] || { v020_setup_error 'CUDA compiler missing'; return 1; }
  export CUDA_HOME=$toolkit CUDACXX=$toolkit/bin/nvcc
  # nvcc otherwise selects gcc, which may lack its matching cc1plus even when
  # the separately installed c++ compiler passed preflight.
  export NVCC_CCBIN
  NVCC_CCBIN=$(command -v c++)
  export PATH=$toolkit/bin:$PATH
  export LD_LIBRARY_PATH=$toolkit/lib64:$toolkit/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
  printf '%s\n' "$CUDA_HOME" >"$EVIDENCE/cuda-home.txt"
  v020_cmd "$CUDACXX" --version
}

v020_install_vllm() {
  local key=$1 version=$2 path=${!1}
  [ -n "$path" ] || return 0
  if [ -z "${V020_OVERRIDES[$key]:-}" ]; then
    v020_venv "$path"
    v020_cmd "$BIN/uv" pip install --python "$path/bin/python" "vllm==$version" --torch-backend=cu130
  fi
  v020_freeze "$path" "$key"
}

v020_install_sglang() {
  local key=$1 version=$2 path=${!1} source commit
  [ -n "$path" ] || return 0
  if [ -z "${V020_OVERRIDES[$key]:-}" ]; then
    v020_venv "$path"
    if [ "$key" = SGLANG_EDITABLE ]; then
      source=$V020/engines/sglang-0.5.23-source
      v020_claim "$source"
      [ -d "$source/.git" ] || v020_cmd git clone --depth 1 --branch v0.5.23 https://github.com/sgl-project/sglang.git "$source"
      commit=$(git -C "$source" rev-parse HEAD)
      [[ $commit =~ ^[0-9a-f]{40}$ ]] || return 1
      printf '%s\n' "$commit" >"$EVIDENCE/sglang-editable-commit.txt"
      # #235 rejects this distribution before launch; its optional Rust kernels
      # are unnecessary, and installing a new Rust toolchain is not authorized.
      v020_cmd env SGLANG_BUILD_RUST_EXTS=none "$BIN/uv" pip install --python "$path/bin/python" --prerelease=allow --torch-backend=cu130 \
        --editable "$source/python[tracing]" torch-memory-saver==0.0.10
    else
      v020_cmd "$BIN/uv" pip install --python "$path/bin/python" --prerelease=allow --torch-backend=cu130 \
        "sglang[tracing]==$version" torch-memory-saver==0.0.10
    fi
  fi
  v020_freeze "$path" "$key"
}

v020_prepare_lmcache_source() {
  local version=${LMCACHE_VERSION:-0.5.7}
  [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { v020_setup_error 'invalid LMCache version'; return 1; }
  local needed=0 key
  for key in LMCACHE LMCACHE_SGLANG; do
    if [ -n "${!key}" ] && [ -z "${V020_OVERRIDES[$key]:-}" ]; then needed=1; fi
  done
  [ "$needed" = 1 ] || return 0
  v020_fetch "https://github.com/LMCache/LMCache/archive/refs/tags/v$version.tar.gz" "$V020/downloads/lmcache-$version.tar.gz"
  "$V020/tools/bin/python" - "$V020/downloads/lmcache-$version.tar.gz" "$EVIDENCE/lmcache-source.fingerprint.json" "$version" <<'PY'
import hashlib,json,pathlib,sys
archive=pathlib.Path(sys.argv[1])
pathlib.Path(sys.argv[2]).write_text(json.dumps({"version":sys.argv[3],"archive_sha256":hashlib.file_digest(archive.open("rb"),"sha256").hexdigest()},sort_keys=True)+"\n")
PY
}

v020_install_lmcache() {
  local key=$1 base_key=$2 engine=$3 destination=${!1} baseline=${!2}
  [ -n "$destination" ] || return 0
  [ -z "${V020_OVERRIDES[$key]:-}" ] || { v020_freeze "$destination" "$key"; return; }
  [ -n "$baseline" ] || { v020_setup_error 'LMCache recreation requires its baseline engine'; return 1; }
  local version=${LMCACHE_VERSION:-0.5.7} constraints=$V020/downloads/$key-engine-constraints.txt
  v020_venv "$destination"
  # Reinstall instead of cp -a: copied console scripts retain old absolute shebangs.
  v020_cmd "$BIN/uv" pip install --python "$destination/bin/python" --prerelease=allow --torch-backend=cu130 \
    -r "$EVIDENCE/$base_key.freeze.txt"
  v020_freeze "$destination" "$key.before"
  "$baseline/bin/python" - "$constraints" "$engine" <<'PY'
import importlib.metadata as m,pathlib,sys
protected={"torch","torchvision","torchaudio",sys.argv[2],"sglang-kernel","sgl-kernel"}
pins={}
for distribution in m.distributions():
    name=distribution.metadata["Name"]
    normalized=name.lower().replace("_","-")
    if normalized in protected or normalized.startswith("flashinfer"):
        pins[name]=distribution.version
pathlib.Path(sys.argv[1]).write_text("".join(f"{n}=={v}\n" for n,v in sorted(pins.items())))
PY
  v020_cmd "$BIN/uv" pip install --python "$destination/bin/python" --constraint "$constraints" \
    'setuptools>=77.0.3,<81' setuptools_scm ninja 'packaging>=24.2' wheel grpcio==1.78.0 grpcio-tools==1.78.0
  # uv's source wheel cache is not keyed by the baseline torch ABI. Each target
  # must compile independently, even though both source archives have one version.
  v020_cmd env TORCH_CUDA_ARCH_LIST="$(v020_cuda_arch torch)" BUILD_WITH_CUDA=1 LMCACHE_CUDA_MAJOR=13 \
    SETUPTOOLS_SCM_PRETEND_VERSION="$version" "$BIN/uv" --no-cache pip install \
    --python "$destination/bin/python" --prerelease=allow --torch-backend=cu130 \
    --no-build-isolation --constraint "$constraints" \
    "$V020/downloads/lmcache-$version.tar.gz"
  v020_freeze "$destination" "$key"
  "$V020/tools/bin/python" - "$EVIDENCE/$key.before.fingerprint.json" \
    "$EVIDENCE/$key.fingerprint.json" "$EVIDENCE/$key.freeze-diff.json" "$constraints" <<'PY'
import json,pathlib,sys
before=json.loads(pathlib.Path(sys.argv[1]).read_text())["packages"]
after=json.loads(pathlib.Path(sys.argv[2]).read_text())["packages"]
normalize=lambda name:name.lower().replace("_","-")
actual={normalize(name):version for name,version in after.items()}
for line in pathlib.Path(sys.argv[4]).read_text().splitlines():
    name,version=line.split("==",1)
    if actual.get(normalize(name))!=version:
        raise SystemExit("LMCache changed a protected engine/torch/kernel package")
diff={
    "added":{name:version for name,version in after.items() if name not in before},
    "removed":{name:version for name,version in before.items() if name not in after},
    "changed":{name:{"before":before[name],"after":version} for name,version in after.items()
               if name in before and before[name]!=version},
}
pathlib.Path(sys.argv[3]).write_text(json.dumps(diff,sort_keys=True,indent=2)+"\n")
PY
}

v020_tensorfold_baseline() {
  if [ -z "$TENSORFOLD_066" ]; then
    printf '%s\n' 'not configured: TensorFold 0.6.6 supplied as explicitly unavailable' >"$EVIDENCE/tensorfold-066-availability.txt"
    return 0
  fi
  if [ -z "${V020_OVERRIDES[TENSORFOLD_066]:-}" ]; then
    # ADR 0039 TF9: a fresh legacy line beside the native environments.
    local archive=$V020/downloads/tensorfold-0.6.6.tar.gz
    v020_fetch https://codeload.github.com/ashhart/TensorFold/tar.gz/cb2ebf0540f42604e2759b2ddef497861e928248 "$archive"
    v020_check_sha "$archive" 575ad3745d4a92ef097f43c5796a78ec16a1fafb789265bf14301b12dfb2bd42
    v020_venv "$TENSORFOLD_066"
    # Torch pins its matching Triton dependency; do not select an unrelated
    # latest Triton after installing the pinned CUDA torch.
    v020_cmd "$BIN/uv" pip install --python "$TENSORFOLD_066/bin/python" \
      torch==2.13.0 --index-url https://download.pytorch.org/whl/cu130
    v020_cmd "$BIN/uv" pip install --python "$TENSORFOLD_066/bin/python" \
      "$archive" ninja==1.11.1.4 torch==2.13.0 --torch-backend=cu130
  fi
  v020_freeze "$TENSORFOLD_066" TENSORFOLD_066
  printf '%s\n' 'configured: TensorFold 0.6.6 comparison baseline; live qualification still required' >"$EVIDENCE/tensorfold-066-availability.txt"
}

v020_signal_observer() {
  [ -n "$STRACE" ] || return 0
  [ -z "${V020_OVERRIDES[STRACE]:-}" ] || return 0
  local arch expected
  arch=$(uname -m)
  case "$arch" in
    x86_64) expected=4722ad596f33f695c8428269d4164fa1fa3b50b65a44b4a2034842de259f50fa ;;
    aarch64) expected=c6e211c6e29311bc00457c876da53cc6c7299146f324a9b80700c34908055d6d ;;
    *) v020_setup_error "no pinned signal observer for $arch"; return 1 ;;
  esac
  v020_claim "$STRACE"
  v020_fetch "https://github.com/ernw/static-toolbox/releases/download/strace-v6.3/strace-6.3-$arch" "$STRACE"
  v020_check_sha "$STRACE" "$expected"
  chmod 700 "$STRACE"
}

# The GPU's compute capability decides the kernels built here (8.0 on an A100,
# 12.0 on an RTX PRO 6000): `torch` prints 8.0, `cmake` prints 80-real, or
# 120a-real where the architecture-specific features need the `a` variant.
v020_cuda_arch() {
  local cap major
  cap=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -n 1 | tr -d ' ')
  case "$cap" in [0-9]*.[0-9]) ;; *) v020_setup_error "unreadable compute capability: $cap"; return 1 ;; esac
  major=${cap%%.*}
  if [ "$1" = torch ]; then printf '%s\n' "$cap"
  elif [ "$major" -ge 9 ]; then printf '%sa-real\n' "${cap/./}"
  else printf '%s-real\n' "${cap/./}"; fi
}

v020_build_llamacpp() {
  [ -z "${V020_OVERRIDES[LLAMACPP]:-}" ] || return 0
  # Pin source: the v0.9.0 commit table in llamacpp-build.sh at 63f91fc8.
  local source=$V020/engines/llamacpp-v0.9.0 head
  v020_claim "$source"
  [ -d "$source/.git" ] || v020_cmd git clone --branch v0.9.0 --depth 1 https://github.com/ggml-org/llama.cpp "$source"
  head=$(git -C "$source" rev-parse HEAD)
  [ "$head" = cd959bae99ff4bed22bef2604f45241c09bbb8c8 ] || { v020_setup_error 'llama.cpp tag commit mismatch'; return 1; }
  v020_cmd cmake -S "$source" -B "$source/build" \
    -DGGML_CUDA=ON "-DCMAKE_CUDA_ARCHITECTURES=$(v020_cuda_arch cmake)" -DCMAKE_BUILD_TYPE=Release \
    -DLLAMA_BUILD_IS_DEV=OFF -DLLAMA_CONNECT=OFF -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF \
    "-DCUDAToolkit_ROOT=$CUDA_HOME" "-DCMAKE_CUDA_COMPILER=$CUDACXX" \
    "-DCMAKE_CXX_COMPILER=$(command -v c++)" "-DCMAKE_CUDA_HOST_COMPILER=$(command -v c++)" \
    "-DCMAKE_BUILD_RPATH=$CUDA_HOME/lib64;$CUDA_HOME/lib"
  v020_cmd cmake --build "$source/build" -j "$(nproc)" --target llama-server
  printf '%s\n' "$head" >"$EVIDENCE/llamacpp-commit.txt"
  v020_cmd "$LLAMACPP" --version
}

v020_native_tensorfold() {
  local key=$1 version=$2 expected=$3 destination=${!1} archive stage
  [ -z "${V020_OVERRIDES[$key]:-}" ] || return 0
  # Exports point to the release directory, as accepted by `engine add`.
  v020_mkdir "$destination"
  archive=$V020/downloads/tensorfold-$version-linux-x86_64.tar.gz
  v020_fetch "https://github.com/ashhart/TensorFold/releases/download/v$version/tensorfold-$version-linux-x86_64.tar.gz" "$archive"
  v020_check_sha "$archive" "$expected"
  stage=$V020/downloads/tensorfold-$version
  v020_mkdir "$stage"
  v020_unpack "$archive" "$stage"
  "$V020/tools/bin/python" - "$stage" "$destination" <<'PY'
import pathlib,shutil,sys
stage=pathlib.Path(sys.argv[1]); destination=pathlib.Path(sys.argv[2])
roots=[p.parent for p in stage.rglob("VERSION") if (p.parent/"bin/tensorfold-native").is_file()]
if len(roots)!=1:
    raise SystemExit("TensorFold archive has no unique native release")
shutil.copytree(roots[0],destination,dirs_exist_ok=True,symlinks=True)
PY
  v020_manifest "$destination"
  [ -x "$destination/bin/tensorfold-native" ] || { v020_setup_error 'TensorFold binary missing'; return 1; }
}

v020_old_binary() {
  local name base archive stage expected
  name=capyctl-0.1.12-linux-$(uname -m)
  base=https://github.com/edurdias/capyctl/releases/download/v0.1.12
  archive=$V020/downloads/$name.tar.gz
  v020_fetch "$base/$name.tar.gz" "$archive"
  v020_fetch "$base/SHA256SUMS" "$V020/downloads/capyctl-0.1.12-SHA256SUMS"
  expected=$("$V020/tools/bin/python" - "$V020/downloads/capyctl-0.1.12-SHA256SUMS" "$name.tar.gz" <<'PY'
import pathlib,re,sys
matches=[line.split()[0] for line in pathlib.Path(sys.argv[1]).read_text().splitlines()
         if len(line.split())==2 and line.split()[1].lstrip("*")==sys.argv[2]]
if len(matches)!=1 or not re.fullmatch(r"[0-9a-f]{64}",matches[0]):
    raise SystemExit("release SHA256SUMS has no unique archive digest")
print(matches[0])
PY
)
  v020_check_sha "$archive" "$expected"
  stage=$V020/downloads/capyctl-0.1.12
  v020_mkdir "$stage"
  v020_unpack "$archive" "$stage"
  v020_manifest "$stage/$name"
  install -m 700 "$stage/$name/bin/capyctl" "$BIN/capyctl-old"
  v020_cmd "$BIN/capyctl-old" --version
}

v020_collector() {
  [ -z "${V020_OVERRIDES[OTELCOL]:-}" ] || return 0
  local directory=$V020/engines/otelcol-0.164.0 archive=$V020/downloads/otelcol_0.164.0_linux_amd64.tar.gz
  v020_mkdir "$directory"
  v020_fetch https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v0.164.0/otelcol_0.164.0_linux_amd64.tar.gz "$archive"
  # SHA256 from the published v0.164.0 release asset digest.
  v020_check_sha "$archive" 5db7cf88c2e73a7fdce02b5fa967fcd5dc73bab00b871d947481918dc5093861
  v020_unpack "$archive" "$directory"
  [ -x "$OTELCOL" ] || { v020_setup_error 'collector binary missing'; return 1; }
  v020_cmd "$OTELCOL" --version
}

v020_trt_availability() {
  "$V020/tools/bin/python" - "$EVIDENCE/$1.availability.json" "$2" "$3" <<'PY'
import json,pathlib,sys
pathlib.Path(sys.argv[1]).write_text(json.dumps({"available":sys.argv[2]=="true","reason":sys.argv[3]},sort_keys=True)+"\n")
PY
}

v020_trt_toolkit() {
  local key=$1 version=$2 cuda_key=$3 toolkit nvcc library
  if [ -n "${V020_OVERRIDES[$cuda_key]:-}" ]; then
    toolkit=${!cuda_key}
  else
    local environment=$V020/engines/$key-cuda-venv
    v020_venv "$environment"
    v020_cmd "$BIN/uv" pip install --python "$environment/bin/python" \
      "cuda-toolkit[all]==$version.*"
    toolkit=$("$V020/tools/bin/python" - "$environment" <<'PY'
import pathlib,sys
paths=sorted(pathlib.Path(sys.argv[1]).rglob("bin/nvcc"))
if len(paths)!=1:
    raise SystemExit("TRT CUDA toolkit has no unique nvcc")
print(paths[0].parent.parent)
PY
)
    case "$toolkit" in "$environment/"*) ;; *) v020_setup_error 'TRT toolkit escaped session'; return 1 ;; esac
    for library in "$toolkit"/lib/*.so.*; do
      [ -f "$library" ] || continue
      nvcc=${library%%.so.*}.so
      [ -e "$nvcc" ] || ln -s "$(basename "$library")" "$nvcc"
    done
    [ -e "$toolkit/lib64" ] || ln -s lib "$toolkit/lib64"
    v020_toolkit_compilers "$toolkit"
    v020_freeze "$environment" "$key-cuda"
  fi
  [ -x "$toolkit/bin/nvcc" ] || { v020_setup_error 'TRT CUDA compiler missing'; return 1; }
  printf '%s\n' "$toolkit" >"$EVIDENCE/$key.cuda-home.txt"
  export CUDA_HOME=$toolkit CUDACXX=$toolkit/bin/nvcc
  export PATH=$toolkit/bin:$PATH LD_LIBRARY_PATH=$toolkit/lib64:$toolkit/lib
  v020_cmd "$CUDACXX" --version
}

v020_trt_driver_probe() {
  local key=$1 source=$V020/downloads/$1-driver-probe.cu binary=$V020/downloads/$1-driver-probe arch
  cat >"$source" <<'CU'
#include <cuda_runtime.h>
#include <cstdio>
__global__ void touch(int *value) { *value = 1; }
int main() {
  int *value = nullptr;
  cudaError_t result = cudaMalloc(&value, sizeof(int));
  if (result == cudaSuccess) {
    touch<<<1, 1>>>(value);
    result = cudaGetLastError();
    if (result == cudaSuccess) result = cudaDeviceSynchronize();
  }
  std::printf("cuda_runtime_result=%d %s\n", static_cast<int>(result), cudaGetErrorString(result));
  if (value != nullptr) cudaFree(value);
  if (result == cudaSuccess) return 0;
  return result == cudaErrorInsufficientDriver || result == cudaErrorUnsupportedPtxVersion ||
         result == cudaErrorSystemDriverMismatch || result == cudaErrorCompatNotSupportedOnDevice ? 42 : 1;
}
CU
  arch=$(v020_cuda_arch torch)
  arch=${arch/./}
  v020_cmd timeout 180 "$CUDACXX" -gencode "arch=compute_$arch,code=compute_$arch" \
    -ccbin "$(command -v c++)" "$source" -o "$binary"
  if v020_cmd timeout 60 env CUDA_FORCE_PTX_JIT=1 "$binary"; then
    v020_trt_availability "$key" true 'installed; toolkit13.4 forced-PTX kernel succeeded; model qualification still required'
  else
    local result=$?
    if [ "$result" = 42 ]; then
      v020_trt_availability "$key" false 'driver_unsupported: CUDA error35/222/803/804 from toolkit13.4 forced-PTX kernel on the unchanged driver; see setup.log'
    else
      v020_trt_availability "$key" false 'CUDA preflight failed for a reason other than explicit driver incompatibility; see setup.log'
      return 1
    fi
  fi
}

v020_setup_tensorrt() {
  local key=$1 version=$2 torch_version=$3 backend=$4 toolkit_version=$5 cuda_key=$6 destination=${!1} wheel constraints
  if [ -z "$destination" ]; then
    v020_trt_availability "$key" false 'not selected: TensorRT installs only for all or explicit TR rows'
    return 0
  fi
  if [ -n "${V020_OVERRIDES[$key]:-}" ]; then
    v020_freeze "$destination" "$key"
    v020_trt_availability "$key" true 'configured read-only engine override; live qualification still required'
    return 0
  fi
  v020_venv "$destination"
  v020_cmd "$BIN/uv" pip install --python "$destination/bin/python" \
    "torch==$torch_version" torchvision --index-url "https://download.pytorch.org/whl/$backend"
  v020_cmd "$BIN/uv" pip install --python "$destination/bin/python" openmpi==4.1.8 mpi4py
  constraints=$V020/downloads/$key-torch-constraints.txt
  printf 'torch==%s+%s\n' "$torch_version" "$backend" >"$constraints"
  if [ "$version" = 1.2.1 ]; then
    # TRT 1.2.1 requires NumPy<2; PyArrow 26's wheel imports only with NumPy>=2.
    # Keep a compatible pair instead of accepting pip's metadata-only solution.
    printf 'numpy==1.26.4\npyarrow==21.*\n' >>"$constraints"
  fi
  case "$version" in
    1.2.1) wheel=https://pypi.nvidia.com/tensorrt-llm/tensorrt_llm-1.2.1-cp312-cp312-linux_x86_64.whl ;;
    1.4.0rc3) wheel=https://pypi.nvidia.com/tensorrt-llm/tensorrt_llm-1.4.0rc3-cp312-cp312-manylinux_2_28_x86_64.whl ;;
    *) v020_setup_error 'unsupported TensorRT wheel'; return 1 ;;
  esac
  v020_cmd "$BIN/uv" pip install --python "$destination/bin/python" --prerelease=allow \
    --constraint "$constraints" --extra-index-url https://pypi.nvidia.com \
    --index-strategy unsafe-best-match "$wheel"
  v020_trt_toolkit "$key" "$toolkit_version" "$cuda_key"
  # The managed engine does not inherit arbitrary loader settings. Prove that
  # pip MPI resolves its own libraries without a shell LD_LIBRARY_PATH workaround.
  if ! v020_cmd timeout 60 env -u LD_LIBRARY_PATH \
    OMPI_MCA_ess_singleton_isolated=1 OMPI_MCA_btl=self OMPI_MCA_pml=ob1 \
    "$destination/bin/python" -c 'from mpi4py import MPI; print(MPI.Get_library_version().splitlines()[0])'; then
    v020_trt_availability "$key" false 'MPI loader/import failed without inherited LD_LIBRARY_PATH; see setup.log'
    return 1
  fi
  v020_freeze "$destination" "$key"
  if [ "$version" = 1.4.0rc3 ]; then
    v020_trt_driver_probe "$key"
  else
    v020_trt_availability "$key" true 'installed with pinned torch and MPI; model qualification still required'
  fi
}

v020_models() {
  v020_cmd "$V020/tools/bin/python" - "$V020" "$EVIDENCE" <<'PY'
import concurrent.futures,hashlib,json,os,pathlib,re,sys
from huggingface_hub import HfApi,hf_hub_download,snapshot_download
root=pathlib.Path(sys.argv[1]); evidence=pathlib.Path(sys.argv[2])
specs={
    "MODEL_CHAT":("Qwen/Qwen3-4B-Instruct-2507","main",None),
    "MODEL_CHAT_GGUF":("Qwen/Qwen3-4B-GGUF","main","Qwen3-4B-Q4_K_M.gguf"),
    "MODEL_EMBED":("Qwen/Qwen3-Embedding-4B","5cf2132",None),
    "MODEL_EMBED_GGUF":("Qwen/Qwen3-Embedding-4B-GGUF","main","Qwen3-Embedding-4B-Q8_0.gguf"),
    "MODEL_EMBED8":("Qwen/Qwen3-Embedding-8B","1d8ad4c",None),
    "MODEL_CLEF":("Cloudflare/clef-flash","8b2e5fd1",None),
    "MODEL_CLEF_GGUF":("ggml-org/Clef-Flash-GGUF","6b4fe4c5","Clef-Flash-Q8_0.gguf"),
    "MODEL_TENSORFOLD":("TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit","d9d758fb83953437f7263256b0d96157e2a348b8",None),
}
# Overrides are paths, never instructions to install into an existing environment.
overrides=set(os.environ.get("V020_MODEL_OVERRIDES","").split(","))
lock_path=root/"models.lock.json"
lock=json.loads(lock_path.read_text()) if lock_path.exists() else {}
api=HfApi()
for key,(repo,requested,filename) in specs.items():
    if key in overrides:
        continue
    if key in lock:
        item=lock[key]
        if item.get("repo")!=repo or item.get("requested")!=requested or item.get("filename")!=filename:
            raise SystemExit("model lock does not match setup recipe")
        if not re.fullmatch(r"[0-9a-f]{40}",item.get("revision","")):
            raise SystemExit("model lock has an invalid pinned revision")
    else:
        revision=api.model_info(repo,revision=requested).sha
        if not re.fullmatch(r"[0-9a-f]{40}",revision or ""):
            raise SystemExit("HF did not return a full immutable revision")
        lock[key]={"repo":repo,"requested":requested,"revision":revision,"filename":filename}
        # Persist every successful resolution immediately; retries never move its pin.
        temporary=lock_path.with_suffix(".json.part")
        temporary.write_text(json.dumps(lock,indent=2,sort_keys=True)+"\n")
        temporary.replace(lock_path)
# No download begins until all active repositories have immutable persisted pins.
(evidence/"models.lock.json").write_text(json.dumps(lock,indent=2,sort_keys=True)+"\n")
def download(key):
    item=lock[key]; destination=pathlib.Path(os.environ[key])
    if item["filename"]:
        hf_hub_download(repo_id=item["repo"],revision=item["revision"],filename=item["filename"],local_dir=destination.parent)
        if not destination.is_file() or not destination.stat().st_size:
            raise RuntimeError("GGUF download missing")
        digest=hashlib.file_digest(destination.open("rb"),"sha256").hexdigest()
        return key,{"revision":item["revision"],"bytes":destination.stat().st_size,"sha256":digest}
    snapshot_download(repo_id=item["repo"],revision=item["revision"],local_dir=destination,
        allow_patterns=["*.json","*.safetensors","*.model","*.txt","*.tiktoken","*.jinja","*.py"],max_workers=4)
    files=[p for p in destination.rglob("*") if p.is_file() and ".cache" not in p.parts]
    if not any(p.suffix==".safetensors" for p in files):
        raise RuntimeError("model snapshot has no safetensors weights")
    config=destination/"config.json"
    return key,{"revision":item["revision"],"files":len(files),"bytes":sum(p.stat().st_size for p in files),
                "config_sha256":hashlib.sha256(config.read_bytes()).hexdigest()}
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    fingerprints=dict(pool.map(download,[key for key in specs if key not in overrides]))
(evidence/"models.fingerprint.json").write_text(json.dumps(fingerprints,indent=2,sort_keys=True)+"\n")
print("Pinned model downloads complete:",len(fingerprints))
PY
}

setup() (
  set +x
  set -euo pipefail
  umask 077
  # This check deliberately precedes every write, including cache setup.
  [ "${FUNCTIONAL_ONLY:-0}" != 1 ] || { v020_setup_error 'functional-only hosts require --skip-setup and --engines; no installs permitted'; exit 1; }
  case "$(uname -m)" in x86_64|aarch64) ;; *) v020_setup_error 'unsupported setup architecture'; exit 1 ;; esac
  local tool
  for tool in python3 curl git tar flock grep nproc readlink install cc c++ make timeout; do
    command -v "$tool" >/dev/null 2>&1 || { v020_setup_error "missing prerequisite: $tool (do not install system packages)"; exit 1; }
  done
  : "${V020:?remote.sh must supply V020}" "${BIN:?remote.sh must supply BIN}" "${EVIDENCE:?remote.sh must supply EVIDENCE}"
  local real_home real_root
  real_home=$(readlink -f "$HOME")
  real_root=$(readlink -f "$V020")
  case "$real_root" in "$real_home"/*) ;; *) v020_setup_error 'V020 must be strictly inside the home directory'; exit 1 ;; esac
  [ "$real_root" = "$V020" ] && [ -d "$V020" ] && [ ! -L "$V020" ] || { v020_setup_error 'parent must create a real, absolute session root before setup'; exit 1; }
  case "$BIN" in "$V020"/*) ;; *) v020_setup_error 'BIN escapes session root'; exit 1 ;; esac
  case "$EVIDENCE" in "$V020"/*) ;; *) v020_setup_error 'EVIDENCE escapes session root'; exit 1 ;; esac
  # The parent creates these to upload capyctl and collect setup output. All
  # installation directories below them still go through the ownership ledger.
  [ ! -L "$BIN" ] && [ ! -L "$EVIDENCE" ] && [ ! -L "$V020/created.txt" ] || { v020_setup_error 'symlink at parent-owned output path'; exit 1; }
  if [ ! -d "$BIN" ]; then v020_mkdir "$BIN"; fi
  if [ ! -d "$EVIDENCE" ]; then v020_mkdir "$EVIDENCE"; fi
  local -A V020_OVERRIDES=()
  local -A V020_PATHS=(
    [VLLM_032]="$V020/engines/vllm-0.32.0-venv" [VLLM_033]="$V020/engines/vllm-0.33.0-venv"
    [SGLANG_0522]="$V020/engines/sglang-0.5.22-venv" [SGLANG_0523]="$V020/engines/sglang-0.5.23-venv"
    [SGLANG_EDITABLE]="$V020/engines/sglang-0.5.23-editable-venv" [LMCACHE]="$V020/engines/vllm-0.33.0-lmcache-venv"
    [LMCACHE_SGLANG]="$V020/engines/sglang-0.5.23-lmcache-venv"
    [LLAMACPP]="$V020/engines/llamacpp-v0.9.0/build/bin/llama-server"
    [TENSORFOLD_126]="$V020/engines/tensorfold-1.2.6" [TENSORFOLD_115]="$V020/engines/tensorfold-1.1.5"
    [TENSORFOLD_066]="$V020/engines/tensorfold-0.6.6-venv" [TENSORFOLD_100]=''
    [STRACE]="$V020/tools/strace-6.3"
    [TENSORRT_LLM]='' [TRT_LLM]='' [TENSORRT_LLM_121]='' [TENSORRT_LLM_140RC3]=''
    [OTELCOL]="$V020/engines/otelcol-0.164.0/otelcol"
    [CUDA_HOME]='' [CUDA_HOME_TRT121]='' [CUDA_HOME_TRT140RC3]=''
    [MODEL_CHAT]="$V020/models/chat" [MODEL_CHAT_GGUF]="$V020/models/chat-gguf/Qwen3-4B-Q4_K_M.gguf"
    [MODEL_EMBED]="$V020/models/embed" [MODEL_EMBED_GGUF]="$V020/models/embed-gguf/Qwen3-Embedding-4B-Q8_0.gguf"
    [MODEL_EMBED8]="$V020/models/embed8" [MODEL_CLEF]="$V020/models/clef"
    [MODEL_CLEF_GGUF]="$V020/models/clef-gguf/Clef-Flash-Q8_0.gguf" [MODEL_TENSORFOLD]="$V020/models/tensorfold"
  )
  local requested_rows=${ONLY:-all}
  local trt_row
  local -a trt_rows=()
  IFS=',' read -r -a trt_rows <<<"$requested_rows"
  for trt_row in "${trt_rows[@]}"; do
    case "${trt_row^^}" in
      ALL|TR|TR[0-9]*|TRT*|TENSORRT*|P3*TENSORRT*|P3*TRT*)
        V020_PATHS[TENSORRT_LLM_121]=$V020/engines/tensorrt-llm-1.2.1-venv
        V020_PATHS[TENSORRT_LLM_140RC3]=$V020/engines/tensorrt-llm-1.4.0rc3-venv
        break ;;
    esac
  done
  local key value overrides_file
  if [ -n "${ENGINES_FILE:-}" ]; then
    overrides_file=$V020/engine-overrides.tsv
    python3 - "$ENGINES_FILE" "$overrides_file" <<'PY'
import pathlib,re,shlex,sys
rows=[]
for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
    line=line.strip()
    if not line or line.startswith("#"):
        continue
    key,separator,value=line.partition("=")
    if not separator or not re.fullmatch(r"[A-Z][A-Z0-9_]*",key):
        raise SystemExit("invalid engine mapping")
    value=value.strip()
    if value.startswith(("'",'"')):
        parts=shlex.split(value)
        if len(parts)!=1:
            raise SystemExit("invalid quoted engine path")
        value=parts[0]
    if any(c in value for c in "\t\n\r") or (value and not pathlib.Path(value).is_absolute()):
        raise SystemExit("engine mappings must contain absolute paths")
    if value and not pathlib.Path(value).exists():
        raise SystemExit("supplied engine/model path does not exist")
    rows.append(key+"\t"+value+"\n")
pathlib.Path(sys.argv[2]).write_text("".join(rows))
PY
    while IFS=$'\t' read -r key value; do
      [ -n "${V020_PATHS[$key]+present}" ] || { v020_setup_error 'unknown engine/model mapping key'; exit 1; }
      V020_PATHS[$key]=$value
      V020_OVERRIDES[$key]=1
    done <"$overrides_file"
    rm -- "$overrides_file"
  fi
  # A supplied legacy primary name selects the stable environment, not an
  # additional installation. Explicit version-specific input takes precedence.
  if [ -z "${V020_OVERRIDES[TENSORRT_LLM_121]:-}" ]; then
    for key in TENSORRT_LLM TRT_LLM; do
      if [ -n "${V020_OVERRIDES[$key]:-}" ]; then
        V020_PATHS[TENSORRT_LLM_121]=${V020_PATHS[$key]}
        V020_OVERRIDES[TENSORRT_LLM_121]=1
        break
      fi
    done
  fi
  V020_PATHS[TENSORRT_LLM]=${V020_PATHS[TENSORRT_LLM_121]}
  V020_PATHS[TRT_LLM]=${V020_PATHS[TENSORRT_LLM_121]}
  if [ "$(uname -m)" = aarch64 ]; then
    # The owner provisions approved lab engines separately. Even an unavailable
    # engine must have an explicit empty mapping; setup never upgrades a lab venv.
    for key in VLLM_032 VLLM_033 SGLANG_0522 SGLANG_0523 SGLANG_EDITABLE LMCACHE LMCACHE_SGLANG \
      LLAMACPP TENSORFOLD_126 TENSORFOLD_115 TENSORFOLD_066 TENSORRT_LLM_121 TENSORRT_LLM_140RC3 OTELCOL CUDA_HOME; do
      [ -n "${V020_OVERRIDES[$key]:-}" ] || { v020_setup_error "aarch64 requires an explicit read-only mapping: $key"; exit 1; }
    done
  fi
  export V020_MODEL_OVERRIDES=''
  for key in "${!V020_PATHS[@]}"; do
    export "$key=${V020_PATHS[$key]}"
    if [[ $key = MODEL_* ]] && [ -n "${V020_OVERRIDES[$key]:-}" ]; then
      V020_MODEL_OVERRIDES+=,$key
    fi
  done
  v020_setup_tools
  v020_setup_cuda
  v020_prepare_lmcache_source
  # Reserve the actual model destination directories before HF creates them.
  for key in MODEL_CHAT MODEL_CHAT_GGUF MODEL_EMBED MODEL_EMBED_GGUF MODEL_EMBED8 MODEL_CLEF MODEL_CLEF_GGUF MODEL_TENSORFOLD; do
    [ -z "${V020_OVERRIDES[$key]:-}" ] || continue
    value=${!key}
    case "$key" in *_GGUF) value=$(dirname "$value") ;; esac
    v020_mkdir "$value"
  done
  local -a pids=() labels=()
  (v020_install_vllm VLLM_032 0.32.0) & pids+=("$!"); labels+=(VLLM_032)
  (v020_install_vllm VLLM_033 0.33.0; v020_install_lmcache LMCACHE VLLM_033 vllm) & pids+=("$!"); labels+=(VLLM_033_LMCACHE)
  (v020_install_sglang SGLANG_0522 0.5.22) & pids+=("$!"); labels+=(SGLANG_0522)
  (v020_install_sglang SGLANG_0523 0.5.23; v020_install_lmcache LMCACHE_SGLANG SGLANG_0523 sglang) & pids+=("$!"); labels+=(SGLANG_0523_LMCACHE)
  (v020_install_sglang SGLANG_EDITABLE 0.5.23) & pids+=("$!"); labels+=(SGLANG_EDITABLE)
  (v020_build_llamacpp) & pids+=("$!"); labels+=(LLAMACPP)
  (v020_native_tensorfold TENSORFOLD_126 1.2.6 a8abf9b8d6b1109e716996eba18d5c5ce1dbe013a7808b603f81f218e0bcff3d;
   v020_native_tensorfold TENSORFOLD_115 1.1.5 71ee0d792b94bbe5419332e75a4c2e3975c863235d1f66ae736938e7c1d91e8a) & pids+=("$!"); labels+=(TENSORFOLD)
  (v020_tensorfold_baseline) & pids+=("$!"); labels+=(TENSORFOLD_066)
  (v020_signal_observer) & pids+=("$!"); labels+=(STRACE)
  (v020_old_binary) & pids+=("$!"); labels+=(CAPYCTL_OLD)
  (v020_collector) & pids+=("$!"); labels+=(OTELCOL)
  (v020_models) & pids+=("$!"); labels+=(MODELS)
  (v020_setup_tensorrt TENSORRT_LLM_121 1.2.1 2.9.1 cu130 13.1 CUDA_HOME_TRT121) & pids+=("$!"); labels+=(TENSORRT_LLM_121)
  (v020_setup_tensorrt TENSORRT_LLM_140RC3 1.4.0rc3 2.14.0 cu132 13.4 CUDA_HOME_TRT140RC3) & pids+=("$!"); labels+=(TENSORRT_LLM_140RC3)
  local failed=0 index
  for index in "${!pids[@]}"; do
    if ! wait "${pids[$index]}"; then
      printf 'SETUP_JOB_FAILED: %s\n' "${labels[$index]}" >&2
      failed=1
    fi
  done
  [ "$failed" = 0 ] || exit 1
  # Background jobs cannot update the caller's mapping. Only an explicit
  # unavailable receipt clears a configured path; missing/malformed receipts fail.
  local available cuda_key
  for key in TENSORRT_LLM_121 TENSORRT_LLM_140RC3; do
    available=$("$V020/tools/bin/python" - "$EVIDENCE/$key.availability.json" <<'PY'
import json,pathlib,sys
receipt=json.loads(pathlib.Path(sys.argv[1]).read_text())
if type(receipt.get("available")) is not bool or not isinstance(receipt.get("reason"),str) or not receipt["reason"]:
    raise SystemExit("invalid TensorRT availability receipt")
print("yes" if receipt["available"] else "no")
PY
)
    if [ "$available" = no ]; then printf -v "$key" '%s' ''; fi
    case "$key" in
      TENSORRT_LLM_121) cuda_key=CUDA_HOME_TRT121 ;;
      TENSORRT_LLM_140RC3) cuda_key=CUDA_HOME_TRT140RC3 ;;
    esac
    if [ -f "$EVIDENCE/$key.cuda-home.txt" ]; then
      IFS= read -r value <"$EVIDENCE/$key.cuda-home.txt"
      printf -v "$cuda_key" '%s' "$value"
    fi
  done
  export TENSORRT_LLM=$TENSORRT_LLM_121 TRT_LLM=$TENSORRT_LLM_121
  "$V020/tools/bin/python" - "$EVIDENCE" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
lines=[]
for key in ("TENSORRT_LLM_121","TENSORRT_LLM_140RC3"):
    receipt=json.loads((root/(key+".availability.json")).read_text())
    lines.append(key+": "+receipt["reason"])
(root/"tensorrt-availability.txt").write_text("\n".join(lines)+"\n")
PY
  # Artifact checks only; no engine serving, inference, or qualification in setup.
  for key in "${!V020_PATHS[@]}"; do
    value=${!key}
    if [ -n "$value" ] && [ ! -e "$value" ]; then
      v020_setup_error "missing artifact: $key"; exit 1
    fi
  done
  [ -x "$BIN/capyctl-old" ] || { v020_setup_error 'old release binary missing'; exit 1; }
  "$V020/tools/bin/python" - "$EVIDENCE/binaries.fingerprint.json" \
    "$BIN/capyctl" "$BIN/capyctl-old" "$LLAMACPP" "$OTELCOL" "$STRACE" <<'PY'
import hashlib,json,os,pathlib,sys
fingerprints={}
paths=[pathlib.Path(name) for name in sys.argv[2:] if name]
for key in ("TENSORFOLD_126","TENSORFOLD_115","TENSORFOLD_100"):
    if os.environ[key]:
        path=pathlib.Path(os.environ[key])
        paths.append(path/"bin/tensorfold-native" if path.is_dir() else path)
for path in paths:
    name=str(path)
    if path.is_file():
        fingerprints[name]={"bytes":path.stat().st_size,"sha256":hashlib.file_digest(path.open("rb"),"sha256").hexdigest()}
        if path.name=="llama-server":
            for library in sorted(path.parent.glob("lib*.so*")):
                if library.is_file() and not library.is_symlink():
                    fingerprints[str(library)]={"bytes":library.stat().st_size,"sha256":hashlib.file_digest(library.open("rb"),"sha256").hexdigest()}
pathlib.Path(sys.argv[1]).write_text(json.dumps(fingerprints,indent=2,sort_keys=True)+"\n")
PY
  {
    for key in VLLM_032 VLLM_033 SGLANG_0522 SGLANG_0523 SGLANG_EDITABLE LMCACHE LMCACHE_SGLANG LLAMACPP TENSORFOLD_126 TENSORFOLD_115 TENSORFOLD_066 TENSORFOLD_100 \
      TENSORRT_LLM TRT_LLM TENSORRT_LLM_121 TENSORRT_LLM_140RC3 OTELCOL STRACE \
      MODEL_CHAT MODEL_CHAT_GGUF MODEL_EMBED MODEL_EMBED_GGUF MODEL_EMBED8 MODEL_CLEF MODEL_CLEF_GGUF MODEL_TENSORFOLD \
      CUDA_HOME CUDA_HOME_TRT121 CUDA_HOME_TRT140RC3; do
      printf '%s=%s\n' "$key" "${!key}"
    done
  } >"$V020/engines.env.part"
  mv -- "$V020/engines.env.part" "$V020/engines.env"
  cp -- "$V020/engines.env" "$EVIDENCE/engines.env"
  printf '%s\n' 'SETUP_OK (artifacts installed; not live qualification)'
)
