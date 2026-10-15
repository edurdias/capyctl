#!/usr/bin/env bash
# llama.cpp on a lab host for the GB10 rows LC1-LC6 (owner approval 2026-10-14:
# a llama.cpp build on host A, in the home directory, no system package, driver
# or CUDA change).
#
#   scripts/live/llamacpp-build.sh [--host a|b] [--dry-run] <step>...
#
# Steps (each idempotent; run in this order):
#   source    clone tag v0.6.0 into ~/llamacpp-v0.6.0 and build llama-server with
#             CUDA for GB10 (sm_121, as 121a-real: the tag's ggml-cuda CMake turns
#             a plain 12X into 12Xa, and 121a-real works with any CMake), then print
#             its --version line, the build fingerprint <v>+<commit> CapyCTL reads,
#             and the sha256 of the binary and the libraries beside it. A build
#             whose --version already matches is not rebuilt.
#   model     download Qwen/Qwen3-4B-GGUF Qwen3-4B-Q4_K_M.gguf at the revision the
#             laptop rows used into <models root>/Qwen3-4B-GGUF and check its size
#             and digest.
#   prebuilt  download llama.cpp's prebuilt b11429 Ubuntu CUDA 13.4 arm64 archive
#             and its CUDA runtime into ~/llamacpp-b11429-prebuilt and print its
#             --version line only (design note live check 9). The rows never run
#             it. The owner's approval names a build; confirm that this download is
#             covered before running it.
#   check     the --version line, fingerprint and digests of what is there.
#
# Every remote effect goes through the matrix harness's rsh (lib.sh), which logs it
# and, with DRY_RUN=1 or --dry-run, only prints and parses it.
# shellcheck source=scripts/live/matrix/lib.sh
. "$(dirname "$0")/matrix/lib.sh"

LLAMACPP_TAG=${LLAMACPP_TAG:-v0.6.0}
LLAMACPP_VERSION=${LLAMACPP_TAG#v}
# The tag's commit (design note: d812350; the laptop build printed d81235049).
LLAMACPP_COMMIT=d812350
LLAMACPP_PREBUILT=b11429
CUDA_ARCH=${LLAMACPP_CUDA_ARCH:-121a-real}
LC_SRC=$REMOTE_HOME/llamacpp-$LLAMACPP_TAG
LC_BIN=$LC_SRC/build/bin/llama-server
LC_PREBUILT=$REMOTE_HOME/llamacpp-$LLAMACPP_PREBUILT-prebuilt
MODEL_REPO=Qwen/Qwen3-4B-GGUF
MODEL_REV=bc640142c66e1fdd12af0bd68f40445458f3869b
MODEL_FILE=Qwen3-4B-Q4_K_M.gguf
MODEL_DIR=$MODELS_ROOT/Qwen3-4B-GGUF
MODEL_BYTES=2497280256
# The laptop row recorded the digest as 7485fe6f…fdf5.
MODEL_SHA_HEAD=7485fe6f
MODEL_SHA_TAIL=fdf5

HOST=$HOST_A
STEPS=()
while [ $# -gt 0 ]; do
  case $1 in
    --host) HOST=$(resolve_host "$2"); shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    source|model|prebuilt|check) STEPS+=("$1"); shift ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) die "unknown argument $1 (steps: source model prebuilt check)" ;;
  esac
done
[ "${#STEPS[@]}" -gt 0 ] || { sed -n '2,28p' "$0"; exit 2; }

# The version line, the fingerprint and the installation's digests (ADR 0029: the
# binary and the lib*.so* regular files in its directory).
report_sh() { # report_sh <llama-server path>
  local bin=$1
  printf '%s' "bin=$bin; dir=\$(dirname \"\$bin\")
line=\$(timeout 60 \"\$bin\" --version 2>&1 >/dev/null | grep -m1 '^version: ')
echo \"version_line: \$line\"
v=\$(sed -nE 's/^version: ([^ ]+) \\(build [0-9]+, commit ([0-9a-f]+)\\).*/\\1+\\2/p' <<<\"\$line\")
echo \"build_fingerprint: \${v:-unparsed}\"
(cd \"\$dir\" && find . -maxdepth 1 -type f \\( -name llama-server -o -name 'lib*.so*' \\) -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
case \$line in 'version: $LLAMACPP_VERSION ('*'commit $LLAMACPP_COMMIT'*) echo VERSION_OK ;; *) echo VERSION_MISMATCH; exit 1 ;; esac"
}

step_source() {
  rsh "$HOST" "set -euo pipefail
for t in git cmake /usr/local/cuda/bin/nvcc; do command -v \$t >/dev/null || { echo \"missing \$t\" >&2; exit 2; }; done
[ ! -e /etc/llama.cpp/config.ini ] || { echo '/etc/llama.cpp/config.ini exists: CapyCTL refuses llama.cpp there' >&2; exit 2; }
/usr/local/cuda/bin/nvcc --version | tail -n 2
if [ -x $LC_BIN ] && timeout 60 $LC_BIN --version 2>&1 | grep -q '^version: $LLAMACPP_VERSION (.*commit $LLAMACPP_COMMIT'; then
  echo 'already built'
else
  [ -d $LC_SRC/.git ] || git clone --branch $LLAMACPP_TAG --depth 1 https://github.com/ggml-org/llama.cpp $LC_SRC
  head=\$(git -C $LC_SRC rev-parse HEAD)
  case \$head in $LLAMACPP_COMMIT*) ;; *) echo \"$LC_SRC is at \$head, not $LLAMACPP_TAG ($LLAMACPP_COMMIT)\" >&2; exit 2 ;; esac
  cd $LC_SRC
  cmake -B build -DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF \\
    -DCMAKE_CUDA_COMPILER=/usr/local/cuda/bin/nvcc -DCMAKE_CUDA_ARCHITECTURES=$CUDA_ARCH \\
    -DCMAKE_BUILD_TYPE=Release 2>&1 | grep -E 'CUDA|CMAKE_CUDA_ARCHITECTURES|Configuring done|Error' || true
  time cmake --build build -j \$(nproc) --target llama-server 2>&1 | tail -n 5
fi
$(report_sh "$LC_BIN")"
}

step_model() {
  rsh "$HOST" "set -euo pipefail
mkdir -p $MODEL_DIR
f=$MODEL_DIR/$MODEL_FILE
if [ \"\$(stat -c %s \$f 2>/dev/null || echo 0)\" != $MODEL_BYTES ]; then
  curl -fL --retry 3 -C - -o \$f.part https://huggingface.co/$MODEL_REPO/resolve/$MODEL_REV/$MODEL_FILE
  mv \$f.part \$f
fi
sum=\$(sha256sum \$f | cut -c1-64)
echo \"$MODEL_FILE \$(stat -c %s \$f) bytes sha256 \$sum\"
ls -la $MODEL_DIR
case \$sum in $MODEL_SHA_HEAD*$MODEL_SHA_TAIL) echo MODEL_OK ;; *) echo MODEL_DIGEST_MISMATCH; exit 1 ;; esac
[ \"\$(ls $MODEL_DIR | wc -l)\" = 1 ] || { echo 'the model directory must hold one GGUF only' >&2; exit 1; }"
}

step_prebuilt() {
  local base=https://github.com/ggml-org/llama.cpp/releases/download/$LLAMACPP_PREBUILT
  rsh "$HOST" "set -euo pipefail
mkdir -p $LC_PREBUILT && cd $LC_PREBUILT
for a in llama-$LLAMACPP_PREBUILT-bin-ubuntu-cuda-13.4-arm64.tar.gz cudart-llama-$LLAMACPP_PREBUILT-bin-ubuntu-cuda-13.4-arm64.tar.gz; do
  if [ ! -s \$a ]; then curl -fL --retry 3 -o \$a.part $base/\$a; mv \$a.part \$a; fi
  sha256sum \$a
  tar -xzf \$a
done
bin=\$(find $LC_PREBUILT -type f -name llama-server | head -n 1)
[ -n \"\$bin\" ] || { echo 'no llama-server in the archive' >&2; exit 1; }
echo \"binary: \$bin\"
line=\$(cd \"\$(dirname \"\$bin\")\" && timeout 60 \"\$bin\" --version 2>&1 >/dev/null | grep -m1 '^version: ' || true)
echo \"prebuilt_version_line: \${line:-none}\""
}

step_check() { rsh "$HOST" "$(report_sh "$LC_BIN")"; }

for s in "${STEPS[@]}"; do
  echo "== $s on $HOST"
  "step_$s"
done
