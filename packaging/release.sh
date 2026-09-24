#!/usr/bin/env bash
# Build the mllm release tarball for the current architecture.
#
#   packaging/release.sh [OUT_DIR]        (default OUT_DIR: dist/)
#
# Produces OUT_DIR/mllm-<version>-linux-<arch>.tar.gz and its .sha256. The
# archive unpacks into one directory, mllm-<version>-linux-<arch>/:
#
#   bin/mllm                 stripped release binary (ADR 0001: one executable
#                            per OS/architecture supplies every role)
#   runtime/                 mllm's Python runtime helpers, owner-only modes
#                            (directories 0700, files 0600; SPEC §13.3). The
#                            tests under runtime/tests are not shipped.
#   packaging/systemd/       system and user service units (SPEC §4.3)
#   docs/examples/           example role and deployment documents
#   docs/operations/install.md
#   BUILDINFO                version, commit, target, toolchain, build time
#   SHA256SUMS               digest of every file above
#
# Only files tracked by git are shipped, so no __pycache__, bytecode or local
# scratch can reach the archive. Entries are sorted, owned by 0:0 and stamped
# with the commit time (SOURCE_DATE_EPOCH), and gzip omits its own timestamp,
# so rebuilding the same commit with the same toolchain gives the same archive.
#
# The build must pass scripts/check-release-clean.sh (no test engine in the
# shipped binary). A dirty worktree is refused unless MLLM_RELEASE_ALLOW_DIRTY=1,
# in which case BUILDINFO records it. Honours CARGO_TARGET_DIR.
set -euo pipefail

root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
cd "$root"

out_dir=${1:-dist}
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)

dirty=false
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
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

# SPEC §13.3: mllm's runtime helpers are owner-only. The host refuses a
# runtime tree holding bytecode, symlinks or other-writable modules
# (crates/mllm-agent/src/runtime_integrity.rs).
copy_tracked 0600 runtime ':(exclude)runtime/tests'
find "$pkg/runtime" -type d -exec chmod 0700 {} +
copy_tracked 0644 packaging/systemd docs/examples docs/operations/install.md

if find "$pkg" \( -name __pycache__ -o -name '*.pyc' -o -name '*.pyo' -o -type l \) -print -quit | grep -q .; then
  echo "staged tree contains bytecode or symlinks" >&2
  exit 1
fi

toolchain=$(rustc --version)
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

echo "$tarball"
