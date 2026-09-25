#!/usr/bin/env bash
# Build the mllm release tarball for the current architecture.
#
#   packaging/release.sh [OUT_DIR]        (default OUT_DIR: dist/)
#   packaging/release.sh --sums OUT_DIR   (re)write OUT_DIR/SHA256SUMS only
#
# Produces OUT_DIR/mllm-<version>-linux-<arch>.tar.gz and its .sha256, copies
# packaging/install.sh beside it, and rewrites OUT_DIR/SHA256SUMS over every
# mllm-*.tar.gz and install.sh in OUT_DIR. Build each architecture into its
# own directory, gather the tarballs into one, and run `--sums` there: that
# SHA256SUMS is the release asset install.sh verifies against.
#
# The archive unpacks into one directory, mllm-<version>-linux-<arch>/:
#
#   bin/mllm                 stripped release binary (ADR 0001: one executable
#                            per OS/architecture supplies every role). mllm's
#                            Python runtime helpers are compiled into it and
#                            written to <state_dir>/runtime at role start
#                            (owner decision 2026-09-24; SPEC §3.3).
#   packaging/systemd/       system and user service units (SPEC §4.3)
#   docs/examples/           example role and deployment documents
#   docs/operations/install.md
#   BUILDINFO                version, commit, target, toolchain, build time,
#                            embedded runtime manifest digest
#   SHA256SUMS               digest of every file above
#
# Only files tracked by git are shipped, so no __pycache__, bytecode or local
# scratch can reach the archive. Entries are sorted, owned by 0:0 and stamped
# with the commit time (SOURCE_DATE_EPOCH), and gzip omits its own timestamp,
# so rebuilding the same commit with the same toolchain gives the same archive.
#
# The compiler embeds absolute source paths (registry cache, toolchain, the
# checkout) into the binary's debug and panic-location strings. RUSTFLAGS
# carries --remap-path-prefix for $CARGO_HOME, the rustup toolchain dir, the
# checkout and $HOME so two builders (or the same builder in two directories)
# produce byte-identical output. Any RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS already
# in the environment is kept; the remap flags are appended.
#
# The build must pass scripts/check-release-clean.sh (no test engine in the
# shipped binary). A dirty worktree is refused unless MLLM_RELEASE_ALLOW_DIRTY=1,
# in which case BUILDINFO records it. An untracked file under runtime/ counts
# as dirty: the build embeds every runtime/*.py it finds. Honours
# CARGO_TARGET_DIR.
set -euo pipefail

root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
cd "$root"

# The release-level checksum file: every tarball and the installer.
write_sums() {
  (cd "$1" && find . -maxdepth 1 -type f \( -name 'mllm-*.tar.gz' -o -name install.sh \) -printf '%P\n' |
    LC_ALL=C sort | xargs -r -d '\n' sha256sum) >"$1/SHA256SUMS.tmp"
  mv "$1/SHA256SUMS.tmp" "$1/SHA256SUMS"
}

if [ "${1:-}" = --sums ]; then
  [ -d "${2:-}" ] || { echo "usage: $0 --sums OUT_DIR" >&2; exit 2; }
  write_sums "$2"
  cat "$2/SHA256SUMS"
  exit 0
fi

out_dir=${1:-dist}
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)

dirty=false
if [ -n "$(git status --porcelain --untracked-files=no)" ] ||
  [ -n "$(git status --porcelain --untracked-files=all --ignored=no -- runtime)" ]; then
  dirty=true
  if [ "${MLLM_RELEASE_ALLOW_DIRTY:-0}" != 1 ]; then
    echo "worktree has uncommitted changes; commit them or set MLLM_RELEASE_ALLOW_DIRTY=1" >&2
    exit 1
  fi
fi

commit=$(git rev-parse HEAD)
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct HEAD)}
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)
if [ -z "$version" ]; then
  echo "cannot read the workspace version from Cargo.toml" >&2
  exit 1
fi
arch=$(uname -m)
case "$(uname -s)" in
  Linux) os=linux ;;
  *) echo "unsupported build OS: $(uname -s)" >&2; exit 1 ;;
esac
name="mllm-${version}-${os}-${arch}"
target_dir=${CARGO_TARGET_DIR:-target}

# Remap the builder's absolute paths to fixed, portable stand-ins. Order
# matters: rustc applies the last matching --remap-path-prefix rule, so the
# broad $HOME rule goes first and the paths nested under it (cargo home,
# toolchain, checkout) go after so they take precedence over it.
cargo_home=${CARGO_HOME:-$HOME/.cargo}
toolchain_dir=$(rustc --print sysroot)
remap_flags="--remap-path-prefix=$HOME=/home --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$toolchain_dir=/rustc --remap-path-prefix=$root=/mllm"
if [ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
  sep=$(printf '\x1f')
  encoded=$(printf '%s' "$remap_flags" | tr ' ' "$sep")
  export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${sep}${encoded}"
elif [ -n "${RUSTFLAGS:-}" ]; then
  export RUSTFLAGS="${RUSTFLAGS} ${remap_flags}"
else
  export RUSTFLAGS="$remap_flags"
fi

# Release build from the lockfile, then the shipped-binary check (it rebuilds
# with the same profile, which is a no-op here).
cargo build --release --locked --bin mllm
scripts/check-release-clean.sh

stage=$(mktemp -d "${TMPDIR:-/tmp}/mllm-release.XXXXXX")
trap 'rm -rf "$stage"' EXIT
pkg="$stage/$name"
umask 022
mkdir -p "$pkg/bin"

install -m 0755 "$target_dir/release/mllm" "$pkg/bin/mllm"
strip --strip-all "$pkg/bin/mllm"
if ! "$pkg/bin/mllm" --version >/dev/null; then
  echo "stripped binary does not run" >&2
  exit 1
fi

# Copy tracked files only, preserving their relative paths.
copy_tracked() {
  local mode=$1 path
  shift
  while IFS= read -r -d '' path; do
    install -D -m "$mode" "$path" "$pkg/$path"
  done < <(git ls-files -z -- "$@")
}

# The runtime helpers are not shipped as files: they are compiled into
# bin/mllm (crates/mllm-agent/build.rs) and materialized owner-only at role
# start (crates/mllm-agent/src/embedded_runtime.rs).
copy_tracked 0644 packaging/systemd docs/examples docs/operations/install.md

if find "$pkg" \( -name __pycache__ -o -name '*.pyc' -o -name '*.pyo' -o -type l \) -print -quit | grep -q .; then
  echo "staged tree contains bytecode or symlinks" >&2
  exit 1
fi

toolchain=$(rustc --version)
# The same digest crates/mllm-agent/build.rs embeds: sha256 over
# "<sha256>  <name>" lines of the shipped runtime/*.py, sorted by name.
runtime_manifest=$(git ls-files -- 'runtime/*.py' ':(exclude)runtime/tests' | grep -v '/.*/' |
  LC_ALL=C sort | while IFS= read -r path; do
    printf '%s  %s\n' "$(sha256sum "$path" | cut -c1-64)" "${path#runtime/}"
  done | sha256sum | cut -c1-64)
build_time=$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ)
cat >"$pkg/BUILDINFO" <<EOF
name: mllm
version: $version
commit: $commit
dirty: $dirty
os: $os
arch: $arch
toolchain: $toolchain
source_date: $build_time
runtime_manifest: $runtime_manifest
EOF
chmod 0644 "$pkg/BUILDINFO"

(cd "$pkg" && find . -type f -printf '%P\n' | LC_ALL=C sort |
  xargs -d '\n' sha256sum) >"$stage/SHA256SUMS"
install -m 0644 "$stage/SHA256SUMS" "$pkg/SHA256SUMS"
find "$pkg" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

tarball="$out_dir/$name.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner --format=gnu \
  --mtime="@$SOURCE_DATE_EPOCH" -C "$stage" -cf - "$name" | gzip -9 -n >"$tarball.tmp"
mv "$tarball.tmp" "$tarball"
(cd "$out_dir" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
install -m 0755 packaging/install.sh "$out_dir/install.sh"
write_sums "$out_dir"

echo "$tarball"
