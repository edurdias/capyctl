#!/usr/bin/env bash
# Run the CI "CPU checks" job locally, in CI's order and with CI's shape
# (4 cores, 16 GB when systemd-run is usable, pinned shellcheck, fixture archive).
#
# Usage: scripts/ci-local.sh [--no-limits] [--only STEP] [--list]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

SHELLCHECK_VERSION=v0.11.0
SHELLCHECK_SHA_X86_64=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198
SHELLCHECK_SHA_AARCH64=12b331c1d2db6b9eb13cfca64306b1b157a86eb69db83023e261eaa7e7c14588
TMS_URL=https://files.pythonhosted.org/packages/81/fd/42aad783d433fd69dc108b1b2ee5860fcf33e20e5440b899bc004ff97d70/torch_memory_saver-0.0.9.post1.tar.gz
TMS_SHA=25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43

STEPS=(fmt name core workspace clippy runtime shellcheck installer)
CORE_PKGS=(-p capyctl-adapters -p capyctl-store -p capyctl-controller -p capyctl-management -p harness)
SHELL_FILES=(packaging/release.sh packaging/install.sh scripts/verify-packaging.sh scripts/check-release-clean.sh scripts/test-install.sh scripts/check-name.sh)

cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/capyctl-ci"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target-ci}"
export CARGO_TERM_COLOR=always
export PYTHONDONTWRITEBYTECODE=1

use_limits=1
only=""
while [ $# -gt 0 ]; do
  case "$1" in
    --no-limits) use_limits=0 ;;
    --only) only="${2:?--only needs a step name}"; shift ;;
    --list) printf '%s\n' "${STEPS[@]}"; exit 0 ;;
    -h|--help) sed -n '2,5p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

if [ -n "$only" ]; then
  found=0
  for s in "${STEPS[@]}"; do [ "$s" = "$only" ] && found=1; done
  [ "$found" = 1 ] || { echo "unknown step: $only (see --list)" >&2; exit 2; }
fi

# Limits: taskset to 4 cores, systemd-run scope with MemoryMax=16G when usable.
limit_prefix=()
limit_desc="none"
if [ "$use_limits" = 1 ]; then
  parts=()
  if command -v taskset >/dev/null 2>&1 && taskset -c 0-3 true 2>/dev/null; then
    limit_prefix+=(taskset -c 0-3)
    parts+=("cpus 0-3")
  fi
  if command -v systemd-run >/dev/null 2>&1 \
    && systemd-run --user --scope --quiet -p MemoryMax=16G true 2>/dev/null; then
    limit_prefix=(systemd-run --user --scope --quiet -p MemoryMax=16G "${limit_prefix[@]}")
    parts+=("memory 16G")
  fi
  if [ ${#parts[@]} -gt 0 ]; then
    limit_desc="$(IFS=,; echo "${parts[*]}")"
  fi
fi
echo "ci-local: limits active: $limit_desc; target dir: $CARGO_TARGET_DIR"

limited() { "${limit_prefix[@]}" "$@"; }

fetch_verified() { # url sha256 dest
  local url="$1" sha="$2" dest="$3"
  mkdir -p "$(dirname "$dest")"
  if [ ! -f "$dest" ]; then
    curl --fail --location --retry 3 --silent --show-error --output "$dest.part" "$url"
    mv "$dest.part" "$dest"
  fi
  if ! printf '%s  %s\n' "$sha" "$dest" | sha256sum --check --quiet -; then
    echo "checksum mismatch for $dest; refusing to use it" >&2
    rm -f "$dest"
    return 1
  fi
}

pinned_shellcheck() {
  local arch sha archive bin
  case "$(uname -m)" in
    x86_64) arch=x86_64; sha=$SHELLCHECK_SHA_X86_64 ;;
    aarch64|arm64) arch=aarch64; sha=$SHELLCHECK_SHA_AARCH64 ;;
    *) echo "no pinned shellcheck for $(uname -m)" >&2; return 1 ;;
  esac
  archive="$cache_dir/shellcheck-$SHELLCHECK_VERSION.linux.$arch.tar.xz"
  bin="$cache_dir/shellcheck-$SHELLCHECK_VERSION-$arch/shellcheck"
  fetch_verified "https://github.com/koalaman/shellcheck/releases/download/$SHELLCHECK_VERSION/shellcheck-$SHELLCHECK_VERSION.linux.$arch.tar.xz" "$sha" "$archive" >&2
  if [ ! -x "$bin" ]; then
    mkdir -p "$(dirname "$bin")"
    tar -xJf "$archive" -C "$(dirname "$bin")" --strip-components=1 "shellcheck-$SHELLCHECK_VERSION/shellcheck"
  fi
  echo "$bin"
}

step_fmt() { limited cargo fmt --all --check; }
step_name() { scripts/check-name.sh; }
step_core() {
  limited cargo test "${CORE_PKGS[@]}" --all-targets --no-fail-fast --locked -- --test-threads=4
}
step_workspace() { limited cargo test --workspace --all-targets --no-fail-fast --locked; }
step_clippy() {
  limited cargo clippy "${CORE_PKGS[@]}" --all-targets --locked -- -D warnings
}
step_runtime() {
  local archive="$cache_dir/torch_memory_saver-0.0.9.post1.tar.gz"
  fetch_verified "$TMS_URL" "$TMS_SHA" "$archive"
  TMS_SOURCE_ARCHIVE="$archive" limited python3 -m unittest discover -s runtime/tests -p 'test_*.py' -v
}
step_shellcheck() {
  local sc
  sc="$(pinned_shellcheck)"
  "$sc" --version | sed -n '2p'
  "$sc" "${SHELL_FILES[@]}"
}
step_installer() { scripts/test-install.sh; }

names=() results=() durations=()
failed=""
for s in "${STEPS[@]}"; do
  if [ -n "$only" ] && [ "$s" != "$only" ]; then continue; fi
  echo "== ci-local: $s"
  start=$SECONDS
  if "step_$s"; then r=pass; else r=FAIL; fi
  names+=("$s"); results+=("$r"); durations+=("$((SECONDS - start))s")
  if [ "$r" = FAIL ]; then
    failed="$s"
    echo "ci-local: step failed: $s" >&2
    break
  fi
done

echo "== ci-local summary"
for i in "${!names[@]}"; do
  printf '%-12s %-4s %s\n' "${names[$i]}" "${results[$i]}" "${durations[$i]}"
done
[ -z "$failed" ] || exit 1
