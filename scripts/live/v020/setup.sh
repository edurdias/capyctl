#!/usr/bin/env bash
# Source from remote.sh; sourcing this file has no host-side effects.
# Lab setup accepts read-only engine overrides; fresh engine installs remain VM-only.

setup_plan() {
  cat <<EOF
Setup (rented x86_64 VM only; approximately 60 minutes, downloads/installations parallel):
     Only runnable selected rows provision their engine/model/build prerequisites.
  1. Refuse FUNCTIONAL_ONLY=1: use --skip-setup and --engines on an existing host.
     Require C/C++ compilers, make, git, curl, tar, patch, python3, flock and nproc;
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
     EM6 alone also gets a fresh vLLM 0.33.0 observational environment with a
     hashed ASGI header-name/byte-count hook; existing environments stay unchanged.
     vLLM 0.34.0rc1, when needed alone, uses official architecture-specific wheels
     pinned to tag commit 52aeb580658571613f8ab28a075a06fa30279beb and SHA256.
  7. Recreate separate vLLM 0.33 (clearing guard) and SGLang 0.5.23 environments;
     source-build LMCache ${LMCACHE_VERSION:-0.5.7} against their exact torch.
     vLLM 0.34 + LMCache is unavailable: opentelemetry-api >=1.45.1 conflicts
     with LMCache <=1.40.0. Its arm skips; do not override declarations.
     Tracing fixtures additionally create separate vLLM 0.33.0 and SGLang 0.5.23
     environments with API/SDK/OTLP exporter 1.39.1 (within engine declarations),
     and a separate SGLang 0.5.23 without its tracing extra for OT10.
     Use no build isolation, the GPU's own CUDA arch and CUDA major 13; preserve engine
     constraints and disable built-wheel reuse across the different torch builds.
     Never copy a venv's absolute shebangs. Source recognition, not version alone,
     decides whether sleep preserves the external cache (ADR0034 amendment).
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
 11. Tracing rows run a loopback Python OTLP/HTTP protobuf receiver with the
     tracing environment's opentelemetry-proto; no collector binary is installed.
 12. TensorRT-LLM is held out of 0.2.0. TR/P3-TENSORRT rows skip without
     installing engines, CUDA 13.1/13.4, Open MPI or mpi4py.
 13. Download pinned models concurrently with engine jobs. Record sanitized package
     freezes, RECORD/binary fingerprints, source commits, model lock and availability.
     Never save credentials, environment/command-line dumps, or dependency URLs.
     Only selected qualification rows install session-private checksum-pinned
     protoc 29.3 and Rust 1.99.0: LC12 builds only on unified memory; LC18 on
     either architecture. Preflight protoc before long work.
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
packages={}; records={}; editable=[]; sources={}
for distribution in metadata.distributions():
    name=distribution.metadata.get("Name",""); version=distribution.version
    if not re.fullmatch(r"[A-Za-z0-9_.-]+",name) or not re.fullmatch(r"[A-Za-z0-9_.+!-]+",version):
        raise SystemExit("unsafe package metadata in freeze")
    packages[name]=version
    record=distribution.read_text("RECORD")
    if record is not None:
        records[name]=hashlib.sha256(record.encode()).hexdigest()
    for entry in distribution.files or ():
        relative=str(entry)
        if relative.endswith(".py") and relative.startswith((
            "vllm/entrypoints/serve/dev/sleep/", "vllm/entrypoints/api_server.py",
            "vllm/v1/engine/", "vllm/v1/core/sched/",
            "sglang/srt/managers/scheduler", "sglang/srt/mem_cache/storage/lmcache/",
            "lmcache/integration/vllm/", "lmcache/integration/sglang/")):
            path=distribution.locate_file(entry)
            if path.is_file():
                sources[relative]=hashlib.file_digest(path.open("rb"),"sha256").hexdigest()
    direct=distribution.read_text("direct_url.json")
    if direct and json.loads(direct).get("dir_info",{}).get("editable"):
        editable.append(name)
pathlib.Path(sys.argv[1]).write_text("".join(f"{n}=={v}\n" for n,v in sorted(packages.items(),key=lambda p:p[0].lower())))
pathlib.Path(sys.argv[2]).write_text(json.dumps({"packages":packages,"record_sha256":records,"source_sha256":sources,"editable":sorted(editable)},sort_keys=True,indent=2)+"\n")
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
  local key
  local -a packages=()
  if [ -n "$LLAMACPP" ] && [ -z "${V020_OVERRIDES[LLAMACPP]:-}" ]; then packages+=(cmake ninja); fi
  for key in "${!V020_PATHS[@]}"; do
    if [[ $key = MODEL_* ]] && [ -n "${!key}" ] && [ -z "${V020_OVERRIDES[$key]:-}" ]; then
      packages+=(huggingface_hub)
      break
    fi
  done
  if [ "${#packages[@]}" -gt 0 ]; then
    v020_cmd "$BIN/uv" pip install --python "$V020/tools/bin/python" "${packages[@]}"
  fi
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
  local key=$1 version=$2 path=${!1} reuse=0
  local -a tracing=()
  if [ "$key" = VLLM_033_TRACING ]; then
    tracing=(opentelemetry-api==1.39.1 opentelemetry-sdk==1.39.1 opentelemetry-exporter-otlp==1.39.1)
  fi
  [ -n "$path" ] || return 0
  if [ "$key" = VLLM_033_TRACING ] && [ -e "$path" ] && [ -z "${V020_OVERRIDES[$key]:-}" ]; then
    v020_claim "$path"
    reuse=1  # Immutable after creation, including a resumed setup.
  fi
  if [ -z "${V020_OVERRIDES[$key]:-}" ] && [ "$reuse" = 0 ]; then
    v020_venv "$path"
    if [ "$version" = 0.34.0rc1 ]; then
      local wheel
      wheel=$("$V020/tools/bin/python" "$HERE/release_pins.py" --arch "$(uname -m)" --destination "$V020/downloads")
      v020_cmd "$BIN/uv" pip install --python "$path/bin/python" --prerelease=allow --torch-backend=cu130 "$wheel" "${tracing[@]}"
      printf '%s\n' 52aeb580658571613f8ab28a075a06fa30279beb >"$EVIDENCE/$key.source-commit.txt"
    else
      v020_cmd "$BIN/uv" pip install --python "$path/bin/python" "vllm==$version" --torch-backend=cu130 "${tracing[@]}"
    fi
  fi
  if [ "$key" = VLLM_033_TRACING ]; then
    v020_cmd "$path/bin/python" -c 'import importlib.metadata as m; assert m.version("vllm")=="0.33.0"; assert all(m.version(n) for n in ("opentelemetry-api","opentelemetry-sdk","opentelemetry-exporter-otlp"))'
  fi
  v020_freeze "$path" "$key"
}

v020_install_em6_observer() {
  [ -n "$EM6_VLLM" ] || return 0
  v020_install_vllm EM6_VLLM 0.33.0
  if [ -n "${V020_OVERRIDES[EM6_VLLM]:-}" ]; then
    return 0 # Supplied installations are always read-only.
  fi
  "$EM6_VLLM/bin/python" - "$EM6_VLLM" "$HERE/em6_observer.py" "$EVIDENCE" <<'PY'
import hashlib,json,pathlib,sys,sysconfig
environment,source,evidence=map(pathlib.Path,sys.argv[1:])
site=pathlib.Path(sysconfig.get_path("purelib"))
if not site.is_relative_to(environment):
    raise SystemExit("EM6 hook destination escapes the fresh environment")
module=site/"capyctl_em6_observer.py"
output=evidence/"em6-engine-observation.jsonl"
body=source.read_text().replace("__EM6_OUTPUT__",str(output))
module.write_text(body)
hook=site/"capyctl_em6_observer.pth"
hook.write_text("import capyctl_em6_observer; capyctl_em6_observer.install()\n")
(evidence/"EM6_VLLM.observer.json").write_text(json.dumps({
    "engine":"vllm","version":"0.33.0","observational_environment":True,
    "source_sha256":hashlib.sha256(source.read_bytes()).hexdigest(),
    "installed_sha256":hashlib.sha256(module.read_bytes()).hexdigest(),
    "pth_sha256":hashlib.sha256(hook.read_bytes()).hexdigest(),
    "records":"header names, byte counts, monotonic timestamps; no values"}))
PY
}

v020_install_sglang() {
  local key=$1 version=$2 path=${!1} source commit package="sglang[tracing]==$2" reuse=0
  local -a tracing=()
  if [ "$key" = SGLANG_0523_TRACING ]; then
    tracing=(opentelemetry-api==1.39.1 opentelemetry-sdk==1.39.1 opentelemetry-exporter-otlp==1.39.1)
  elif [ "$key" = SGLANG_0523_SDK_FREE ]; then
    package="sglang==$version"
  fi
  [ -n "$path" ] || return 0
  if [[ $key = SGLANG_0523_TRACING || $key = SGLANG_0523_SDK_FREE ]] && [ -e "$path" ] && [ -z "${V020_OVERRIDES[$key]:-}" ]; then
    v020_claim "$path"
    reuse=1
  fi
  if [ -z "${V020_OVERRIDES[$key]:-}" ] && [ "$reuse" = 0 ]; then
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
        "$package" torch-memory-saver==0.0.10 "${tracing[@]}"
    fi
  fi
  if [ "$key" = SGLANG_0523_SDK_FREE ]; then
    # vLLM 0.33 requires the SDK in common requirements; OT10 uses SGLang.
    v020_cmd "$path/bin/python" -c 'import importlib.metadata as m; assert m.version("sglang")=="0.5.23"; names={d.metadata["Name"].lower().replace("_","-") for d in m.distributions()}; assert not any(n=="opentelemetry-sdk" or n.startswith("opentelemetry-exporter-otlp") for n in names), "OT10 fixture is not SDK-free"'
  fi
  if [ "$key" = SGLANG_0523_TRACING ]; then
    v020_cmd "$path/bin/python" -c 'import importlib.metadata as m; assert m.version("sglang")=="0.5.23"; assert all(m.version(n) for n in ("opentelemetry-api","opentelemetry-sdk","opentelemetry-exporter-otlp"))'
  fi
  v020_freeze "$path" "$key"
}

v020_prepare_lmcache_source() {
  local version=${LMCACHE_VERSION:-0.5.7}
  [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { v020_setup_error 'invalid LMCache version'; return 1; }
  local needed=0 key
  for key in LMCACHE LMCACHE_VLLM_034 LMCACHE_SGLANG; do
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
  if [ "$base_key" = VLLM_034 ]; then
    v020_setup_error "$(PYTHONPATH="$HERE" python3 -c 'from release_pins import LMCACHE_034_CONFLICT; print(LMCACHE_034_CONFLICT)')"
    return 1
  fi
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
  # must compile independently, even though the source archive has one version.
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
  [ -n "$LLAMACPP" ] || return 0
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
  [ -n "$destination" ] || return 0
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
  [ -n "${V020_NEEDED[CAPYCTL_OLD]:-}" ] || return 0
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

# ADR 0034 LC12/LC18 run separate qualification builds, never the release binary.
v020_lmcache_builds() {
  [ -n "${V020_NEEDED[CAPYCTL_LC12]:-}${V020_NEEDED[CAPYCTL_LC18]:-}" ] || return 0
  [ -f "$V020/source.tar" ] || { v020_setup_error 'qualification source archive missing'; return 1; }
  local stage=$V020/qualification-source
  v020_mkdir "$stage"
  v020_unpack "$V020/source.tar" "$stage"
  local row
  for row in LC12 LC18; do
    [ -n "${V020_NEEDED[CAPYCTL_$row]:-}" ] || continue
    v020_mkdir "$V020/qualification-${row,,}"
    # prepare() requires an absent destination; reserve ownership first.
    rmdir "$V020/qualification-${row,,}"
  done
  v020_fetch https://sh.rustup.rs "$V020/downloads/qualification-rustup.sh"
  v020_cmd sh "$V020/downloads/qualification-rustup.sh" -y --no-modify-path --profile minimal --default-toolchain 1.99.0
  v020_cmd env PATH="$CARGO_HOME/bin:$PATH" "$V020/tools/bin/python" \
    "$V020/live/lmcache_builds.py" --source "$stage" --root "$V020" --only "${ONLY:-all}" "${selection_flags[@]}"
}


v020_trt_availability() {
  "$V020/tools/bin/python" - "$EVIDENCE/$1.availability.json" "$2" "$3" <<'PY'
import json,pathlib,sys
pathlib.Path(sys.argv[1]).write_text(json.dumps({"available":sys.argv[2]=="true","reason":sys.argv[3]},sort_keys=True)+"\n")
PY
}

v020_models() {
  local key needed=0
  for key in "${!V020_PATHS[@]}"; do
    if [[ $key = MODEL_* ]] && [ -n "${!key}" ] && [ -z "${V020_OVERRIDES[$key]:-}" ]; then needed=1; fi
  done
  [ "$needed" = 1 ] || return 0
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
    if not os.environ.get(key):
        continue
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
    fingerprints=dict(pool.map(download,[key for key in specs if key not in overrides and os.environ.get(key)]))
(evidence/"models.fingerprint.json").write_text(json.dumps(fingerprints,indent=2,sort_keys=True)+"\n")
print("Pinned model downloads complete:",len(fingerprints))
PY
}

v020_preflight_protoc() {
  [ -n "${V020_NEEDED[CAPYCTL_LC12]:-}${V020_NEEDED[CAPYCTL_LC18]:-}" ] || return 0
  v020_mkdir "$V020/protoc-tools"
  PROTOC=$(python3 "$setup_here/protoc_pins.py" --root "$V020")
  export PROTOC
}

setup() (
  set +x
  set -euo pipefail
  umask 077
  local setup_here
  setup_here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  # This check deliberately precedes every write, including cache setup.
  [ "${FUNCTIONAL_ONLY:-0}" != 1 ] || { v020_setup_error 'functional-only hosts require --skip-setup and --engines; no installs permitted'; exit 1; }
  case "$(uname -m)" in x86_64|aarch64) ;; *) v020_setup_error 'unsupported setup architecture'; exit 1 ;; esac
  local tool
  for tool in python3 curl tar flock grep readlink; do
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
    [EM6_VLLM]=''
    [VLLM_034]='' [LMCACHE_VLLM_034]=''
    [SGLANG_0522]="$V020/engines/sglang-0.5.22-venv" [SGLANG_0523]="$V020/engines/sglang-0.5.23-venv"
    [VLLM_033_TRACING]="$V020/engines/vllm-0.33.0-tracing-venv"
    [SGLANG_0523_TRACING]="$V020/engines/sglang-0.5.23-tracing-venv"
    [SGLANG_0523_SDK_FREE]="$V020/engines/sglang-0.5.23-sdk-free-venv"
    [SGLANG_EDITABLE]="$V020/engines/sglang-0.5.23-editable-venv" [LMCACHE]="$V020/engines/vllm-0.33.0-lmcache-venv"
    [LMCACHE_SGLANG]="$V020/engines/sglang-0.5.23-lmcache-venv"
    [LLAMACPP]="$V020/engines/llamacpp-v0.9.0/build/bin/llama-server"
    [TENSORFOLD_126]="$V020/engines/tensorfold-1.2.6" [TENSORFOLD_115]="$V020/engines/tensorfold-1.1.5"
    [TENSORFOLD_066]="$V020/engines/tensorfold-0.6.6-venv" [TENSORFOLD_100]=''
    [STRACE]="$V020/tools/strace-6.3"
    [TENSORRT_LLM]='' [TRT_LLM]='' [TENSORRT_LLM_121]='' [TENSORRT_LLM_140RC3]=''
    [CUDA_HOME]='' [CUDA_HOME_TRT121]='' [CUDA_HOME_TRT140RC3]=''
    [MODEL_CHAT]="$V020/models/chat" [MODEL_CHAT_GGUF]="$V020/models/chat-gguf/Qwen3-4B-Q4_K_M.gguf"
    [MODEL_EMBED]="$V020/models/embed" [MODEL_EMBED_GGUF]="$V020/models/embed-gguf/Qwen3-Embedding-4B-Q8_0.gguf"
    [MODEL_EMBED8]="$V020/models/embed8" [MODEL_CLEF]="$V020/models/clef"
    [MODEL_CLEF_GGUF]="$V020/models/clef-gguf/Clef-Flash-Q8_0.gguf" [MODEL_TENSORFOLD]="$V020/models/tensorfold"
  )
  local requested_rows=${ONLY:-all}
  case ",${requested_rows^^}," in
    *,ALL,*|*,EM6,*) V020_PATHS[EM6_VLLM]=$V020/engines/vllm-0.33.0-em6-observer-venv ;;
  esac
  case ",${requested_rows^^}," in
    *,ALL,*|*,LC13,*|*,LC14,*|*,LC15,*|*,LC16,*|*,LC13U,*|*,LC14U,*|*,LC15U,*|*,LC16U,*)
      V020_PATHS[VLLM_034]=$V020/engines/vllm-0.34.0rc1-venv
      V020_PATHS[LMCACHE_VLLM_034]=$V020/engines/vllm-0.34.0rc1-lmcache057-venv ;;
  esac
  local -A V020_NEEDED=()
  local -a selection_flags=()
  [ "$(uname -m)" != aarch64 ] || selection_flags+=(--unified)
  local dependencies key value overrides_file
  dependencies=$(python3 "$setup_here/setup_selection.py" --only "$requested_rows" "${selection_flags[@]}")
  while IFS= read -r key; do
    [ -z "$key" ] || V020_NEEDED[$key]=1
  done <<<"$dependencies"
  for key in "${!V020_PATHS[@]}"; do
    if [ -z "${V020_NEEDED[$key]:-}" ]; then V020_PATHS[$key]=''; fi
  done
  if [ -z "$dependencies" ]; then
    # Selected rows all skip: do not bootstrap even uv/CUDA/model tools.
    for key in "${!V020_PATHS[@]}"; do printf '%s=\n' "$key"; done >"$V020/engines.env"
    cp -- "$V020/engines.env" "$EVIDENCE/engines.env"
    printf '%s\n' 'SETUP_OK (selected rows need no setup; held-out rows remain skips)'
    return 0
  fi
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
      [ -n "${V020_NEEDED[$key]:-}" ] || [ "$key" = CUDA_HOME ] || continue
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
    for key in "${!V020_NEEDED[@]}"; do
      case "$key" in MODEL_*|CAPYCTL_*) continue ;; esac
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
  local needs_compiler=0
  for key in LMCACHE LMCACHE_SGLANG LMCACHE_VLLM_034 LLAMACPP; do
    if [ -n "${!key}" ] && [ -z "${V020_OVERRIDES[$key]:-}" ]; then needs_compiler=1; fi
  done
  if [ "$needs_compiler" = 1 ] || [ -n "${V020_NEEDED[CAPYCTL_LC12]:-}${V020_NEEDED[CAPYCTL_LC18]:-}" ]; then
    for tool in git patch nproc install cc c++ make timeout; do
      command -v "$tool" >/dev/null 2>&1 || { v020_setup_error "missing prerequisite: $tool (do not install system packages)"; exit 1; }
    done
  fi
  v020_preflight_protoc
  v020_setup_tools
  local needs_cuda=0
  for key in LMCACHE LMCACHE_VLLM_034 LMCACHE_SGLANG LLAMACPP; do
    if [ -n "${!key}" ] && [ -z "${V020_OVERRIDES[$key]:-}" ]; then needs_cuda=1; fi
  done
  [ "$needs_cuda" = 0 ] || v020_setup_cuda
  v020_prepare_lmcache_source
  v020_lmcache_builds
  # Reserve the actual model destination directories before HF creates them.
  for key in MODEL_CHAT MODEL_CHAT_GGUF MODEL_EMBED MODEL_EMBED_GGUF MODEL_EMBED8 MODEL_CLEF MODEL_CLEF_GGUF MODEL_TENSORFOLD; do
    [ -z "${V020_OVERRIDES[$key]:-}" ] || continue
    [ -n "${!key}" ] || continue
    value=${!key}
    case "$key" in *_GGUF) value=$(dirname "$value") ;; esac
    v020_mkdir "$value"
  done
  local -a pids=() labels=()
  (v020_install_vllm VLLM_032 0.32.0) & pids+=("$!"); labels+=(VLLM_032)
  (v020_install_vllm VLLM_033 0.33.0; v020_install_lmcache LMCACHE VLLM_033 vllm) & pids+=("$!"); labels+=(VLLM_033_LMCACHE)
  (v020_install_em6_observer) & pids+=("$!"); labels+=(EM6_VLLM)
  (v020_install_vllm VLLM_034 0.34.0rc1; v020_install_lmcache LMCACHE_VLLM_034 VLLM_034 vllm) & pids+=("$!"); labels+=(VLLM_034_LMCACHE)
  (v020_install_sglang SGLANG_0522 0.5.22) & pids+=("$!"); labels+=(SGLANG_0522)
  (v020_install_sglang SGLANG_0523 0.5.23; v020_install_lmcache LMCACHE_SGLANG SGLANG_0523 sglang) & pids+=("$!"); labels+=(SGLANG_0523_LMCACHE)
  (v020_install_sglang SGLANG_EDITABLE 0.5.23) & pids+=("$!"); labels+=(SGLANG_EDITABLE)
  (v020_install_vllm VLLM_033_TRACING 0.33.0) & pids+=("$!"); labels+=(VLLM_033_TRACING)
  (v020_install_sglang SGLANG_0523_TRACING 0.5.23) & pids+=("$!"); labels+=(SGLANG_0523_TRACING)
  (v020_install_sglang SGLANG_0523_SDK_FREE 0.5.23) & pids+=("$!"); labels+=(SGLANG_0523_SDK_FREE)
  (v020_build_llamacpp) & pids+=("$!"); labels+=(LLAMACPP)
  (v020_native_tensorfold TENSORFOLD_126 1.2.6 a8abf9b8d6b1109e716996eba18d5c5ce1dbe013a7808b603f81f218e0bcff3d;
   v020_native_tensorfold TENSORFOLD_115 1.1.5 71ee0d792b94bbe5419332e75a4c2e3975c863235d1f66ae736938e7c1d91e8a) & pids+=("$!"); labels+=(TENSORFOLD)
  (v020_tensorfold_baseline) & pids+=("$!"); labels+=(TENSORFOLD_066)
  (v020_signal_observer) & pids+=("$!"); labels+=(STRACE)
  (v020_old_binary) & pids+=("$!"); labels+=(CAPYCTL_OLD)
  (v020_models) & pids+=("$!"); labels+=(MODELS)
  local failed=0 index
  for index in "${!pids[@]}"; do
    if ! wait "${pids[$index]}"; then
      printf 'SETUP_JOB_FAILED: %s\n' "${labels[$index]}" >&2
      failed=1
    fi
  done
  [ "$failed" = 0 ] || exit 1
  # TensorRT-LLM is held out of 0.2.0, including setup dependencies.
  for key in TENSORRT_LLM_121 TENSORRT_LLM_140RC3; do
    v020_trt_availability "$key" false 'TensorRT-LLM is held out of 0.2.0 (ADR 0038 owner decision 2026-11-11)'
  done
  printf '%s\n' 'TensorRT-LLM is held out of 0.2.0 (ADR 0038 owner decision 2026-11-11)' >"$EVIDENCE/tensorrt-availability.txt"
  # Artifact checks only; no engine serving, inference, or qualification in setup.
  for key in "${!V020_PATHS[@]}"; do
    value=${!key}
    if [ -n "$value" ] && [ ! -e "$value" ]; then
      v020_setup_error "missing artifact: $key"; exit 1
    fi
  done
  if [ -n "${V020_NEEDED[CAPYCTL_OLD]:-}" ]; then
    [ -x "$BIN/capyctl-old" ] || { v020_setup_error 'old release binary missing'; exit 1; }
  fi
  "$V020/tools/bin/python" - "$EVIDENCE/binaries.fingerprint.json" \
    "$BIN/capyctl" "$BIN/capyctl-old" "$LLAMACPP" "$STRACE" <<'PY'
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
    for key in VLLM_032 VLLM_033 EM6_VLLM VLLM_034 SGLANG_0522 SGLANG_0523 VLLM_033_TRACING SGLANG_0523_TRACING SGLANG_0523_SDK_FREE SGLANG_EDITABLE LMCACHE LMCACHE_VLLM_034 LMCACHE_SGLANG LLAMACPP TENSORFOLD_126 TENSORFOLD_115 TENSORFOLD_066 TENSORFOLD_100 \
      TENSORRT_LLM TRT_LLM TENSORRT_LLM_121 TENSORRT_LLM_140RC3 STRACE \
      MODEL_CHAT MODEL_CHAT_GGUF MODEL_EMBED MODEL_EMBED_GGUF MODEL_EMBED8 MODEL_CLEF MODEL_CLEF_GGUF MODEL_TENSORFOLD \
      CUDA_HOME CUDA_HOME_TRT121 CUDA_HOME_TRT140RC3; do
      printf '%s=%s\n' "$key" "${!key}"
    done
  } >"$V020/engines.env.part"
  mv -- "$V020/engines.env.part" "$V020/engines.env"
  cp -- "$V020/engines.env" "$EVIDENCE/engines.env"
  printf '%s\n' 'SETUP_OK (artifacts installed; not live qualification)'
)
