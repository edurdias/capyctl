#!/bin/sh
# Install mllm from a GitHub release.
#
#   install.sh [--version VERSION] [--system] [--systemd ROLE] [--repo OWNER/NAME]
#   install.sh --uninstall [--system]
#
# Downloads mllm-<version>-linux-<arch>.tar.gz and the release's SHA256SUMS,
# refuses on any checksum mismatch, and installs the one self-contained
# binary (SPEC §3.3, ADR 0001; mllm's Python runtime helpers are compiled into
# it and written to <state_dir>/runtime when a role starts). Engines, engine
# Python environments, model weights and GPU drivers are never installed.
#
# Options:
#   --version V     release to install, e.g. 0.1.0-rc.3 (a leading "v" is
#                   accepted). Default: the latest published release.
#   --system        install for every user: /usr/local/bin/mllm, units under
#                   /etc/systemd/system (needs root). Default: this user only,
#                   ~/.local/bin/mllm and ~/.config/systemd/user.
#   --systemd ROLE  also install the unit for ROLE (server, host or
#                   standalone) and reload systemd. The unit is not enabled or
#                   started; docs/operations/install.md says how.
#   --repo R        GitHub repository (default edurdias/mllm).
#   --uninstall     remove the binary, the shared files and the units this
#                   script installed. State directories are never touched.
#   -h, --help      this text.
#
# Download, in order of preference:
#   MLLM_INSTALL_BASE_URL  a directory holding the release assets (https://
#                          or file://); used as is, for mirrors and tests.
#   gh                     `gh release download` when the GitHub CLI is
#                          installed and logged in (works for private repos).
#   curl + GITHUB_TOKEN    the GitHub API with the token (private repos).
#   curl                   the public release download URL.
#
# Environment: PREFIX overrides the install prefix (default ~/.local, or
# /usr/local with --system), UNIT_DIR the unit directory. SYSTEMCTL names the
# systemctl to run (tests).
# POSIX sh; shellcheck-clean.
set -eu

repo=edurdias/mllm
version=
scope=user
role=
action=install

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
usage() { sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case $1 in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version=$2; shift 2 ;;
    --version=*) version=${1#*=}; shift ;;
    --system) scope=system; shift ;;
    --systemd) [ $# -ge 2 ] || die "--systemd needs a role"; role=$2; shift 2 ;;
    --systemd=*) role=${1#*=}; shift ;;
    --repo) [ $# -ge 2 ] || die "--repo needs a value"; repo=$2; shift 2 ;;
    --repo=*) repo=${1#*=}; shift ;;
    --uninstall) action=uninstall; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

case $role in
  '' | server | host | standalone) ;;
  *) die "--systemd takes server, host or standalone, not '$role'" ;;
esac
version=${version#v}

if [ "$scope" = system ]; then
  prefix=${PREFIX:-/usr/local}
  unit_dir=${UNIT_DIR:-/etc/systemd/system}
  systemctl_scope=
else
  [ -n "${HOME:-}" ] || die "HOME is not set"
  prefix=${PREFIX:-$HOME/.local}
  unit_dir=${UNIT_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user}
  systemctl_scope=--user
fi
bin_dir=$prefix/bin
share_dir=$prefix/share/mllm
# What this script installed, so --uninstall removes exactly that.
record=$share_dir/installed-files

systemctl_bin=${SYSTEMCTL:-systemctl}
reload_systemd() {
  if command -v "$systemctl_bin" >/dev/null 2>&1; then
    # shellcheck disable=SC2086  # the scope flag is empty or one word
    "$systemctl_bin" $systemctl_scope daemon-reload ||
      say "warning: systemctl daemon-reload failed; run it yourself"
  fi
}

if [ "$action" = uninstall ]; then
  [ -f "$record" ] || die "nothing installed by install.sh under $prefix ($record missing)"
  while IFS= read -r path; do
    case $path in
      "$bin_dir"/mllm | "$unit_dir"/mllm-*.service) rm -f "$path" && say "removed $path" ;;
      *) say "skipped unexpected entry: $path" ;;
    esac
  done <"$record"
  rm -rf "$share_dir"
  say "removed $share_dir"
  reload_systemd
  say "state directories (for example ~/.local/state/mllm or /var/lib/mllm) were kept"
  exit 0
fi

case $(uname -s) in
  Linux) os=linux ;;
  *) die "unsupported OS $(uname -s); releases are built for Linux" ;;
esac
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -c1-64; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -c1-64; }
else
  die "neither sha256sum nor shasum is available; cannot verify the download"
fi

have_gh() { command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/mllm-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

# The GitHub API's JSON, one value per line, without a JSON tool: the asset
# "url" precedes its "name" within each asset object.
asset_api_url() { # $1 release JSON, $2 asset name
  tr ',' '\n' <"$1" | tr '{' '\n' | tr '}' '\n' | awk -v want="\"name\":\"$2\"" '
    { gsub(/^[ \t]+|[ \t]+$/, ""); gsub(/": "/, "\":\"") }
    index($0, "\"url\":\"") == 1 && index($0, "/releases/assets/") { url = substr($0, 8, length($0) - 8) }
    $0 == want && url != "" { print url; exit }'
}

curl_get() { # $1 url, $2 output, [$3 accept header]
  if [ -n "${GITHUB_TOKEN:-}" ] && [ "${1#https://api.github.com/}" != "$1" ]; then
    curl -fsSL --proto '=https' -H "Authorization: Bearer $GITHUB_TOKEN" \
      -H "Accept: ${3:-application/vnd.github+json}" -o "$2" "$1"
  else
    curl -fsSL -o "$2" "$1"
  fi
}

if [ -z "$version" ]; then
  if [ -n "${MLLM_INSTALL_BASE_URL:-}" ]; then
    die "--version is required with MLLM_INSTALL_BASE_URL"
  elif have_gh; then
    tag=$(gh release view -R "$repo" --json tagName --jq .tagName) ||
      die "no published release found in $repo; pass --version"
  else
    curl_get "https://api.github.com/repos/$repo/releases/latest" "$tmp/latest.json" ||
      die "cannot read the latest release of $repo (private repository? set GITHUB_TOKEN or log in with gh)"
    tag=$(tr ',' '\n' <"$tmp/latest.json" | sed -n 's/^[{ ]*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  fi
  [ -n "$tag" ] || die "could not determine the latest release; pass --version"
  version=${tag#v}
fi
tag=v$version
name=mllm-$version-$os-$arch
tarball=$name.tar.gz

say "installing mllm $version ($os-$arch) from $repo"
if [ -n "${MLLM_INSTALL_BASE_URL:-}" ]; then
  base=${MLLM_INSTALL_BASE_URL%/}
  curl -fsSL -o "$tmp/$tarball" "$base/$tarball" || die "cannot download $base/$tarball"
  curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "cannot download $base/SHA256SUMS"
elif have_gh; then
  gh release download "$tag" -R "$repo" -D "$tmp" -p "$tarball" -p SHA256SUMS ||
    die "cannot download $tarball from release $tag of $repo"
elif [ -n "${GITHUB_TOKEN:-}" ]; then
  curl_get "https://api.github.com/repos/$repo/releases/tags/$tag" "$tmp/release.json" ||
    die "release $tag not found in $repo (or the token cannot read it)"
  for asset in "$tarball" SHA256SUMS; do
    url=$(asset_api_url "$tmp/release.json" "$asset")
    [ -n "$url" ] || die "release $tag has no asset $asset"
    curl_get "$url" "$tmp/$asset" application/octet-stream || die "cannot download $asset"
  done
else
  base=https://github.com/$repo/releases/download/$tag
  for asset in "$tarball" SHA256SUMS; do
    curl -fsSL -o "$tmp/$asset" "$base/$asset" ||
      die "cannot download $base/$asset (private repository? set GITHUB_TOKEN or log in with gh)"
  done
fi

# Refuse anything SHA256SUMS does not vouch for, byte for byte.
want=$(awk -v f="$tarball" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")
[ -n "$want" ] || die "SHA256SUMS lists no $tarball; refusing to install"
got=$(sha256 "$tmp/$tarball")
[ "$got" = "$want" ] || die "checksum mismatch for $tarball (expected $want, got $got); refusing to install"
say "verified $tarball sha256:$got"

tar -xzf "$tmp/$tarball" -C "$tmp" --no-same-owner
pkg=$tmp/$name
[ -f "$pkg/bin/mllm" ] && [ -f "$pkg/SHA256SUMS" ] || die "$tarball does not hold $name/bin/mllm"
# The archive's own manifest: every file it ships.
while read -r sum path; do
  [ "$(sha256 "$pkg/$path")" = "$sum" ] || die "$path in $tarball does not match its SHA256SUMS; refusing to install"
done <"$pkg/SHA256SUMS"
reported=$("$pkg/bin/mllm" --version) || die "the downloaded binary does not run on this machine"
[ "$reported" = "mllm $version" ] || die "the downloaded binary reports '$reported', not mllm $version"

umask 022
mkdir -p "$bin_dir" "$share_dir"
# Replace the binary atomically: a running role keeps its open executable.
cp "$pkg/bin/mllm" "$bin_dir/.mllm.new.$$"
chmod 0755 "$bin_dir/.mllm.new.$$"
mv -f "$bin_dir/.mllm.new.$$" "$bin_dir/mllm"
rm -rf "$share_dir/packaging" "$share_dir/docs"
cp -R "$pkg/packaging" "$pkg/docs" "$share_dir/"
cp "$pkg/BUILDINFO" "$share_dir/BUILDINFO"
printf '%s\n' "$bin_dir/mllm" >"$record.tmp"
if [ -f "$record" ]; then
  grep -x "$unit_dir/mllm-.*\\.service" "$record" >>"$record.tmp" || true
fi
say "installed $bin_dir/mllm ($reported)"

if [ -n "$role" ]; then
  unit=mllm-$role.service
  mkdir -p "$unit_dir"
  source_unit=$share_dir/packaging/systemd/$scope/$unit
  # The packaged units name the default locations (/usr/local, ~/.local);
  # point them at this install, so a PREFIX elsewhere is honoured.
  sed -e "s#^ExecStart=[^ ]*/bin/mllm #ExecStart=$bin_dir/mllm #" \
    -e "s#^Documentation=file://[^ ]*/share/mllm/#Documentation=file://$share_dir/#" \
    "$source_unit" >"$unit_dir/.$unit.new"
  chmod 0644 "$unit_dir/.$unit.new"
  mv -f "$unit_dir/.$unit.new" "$unit_dir/$unit"
  printf '%s\n' "$unit_dir/$unit" >>"$record.tmp"
  say "installed $unit_dir/$unit (not enabled)"
  if [ "$scope" = user ]; then
    # The user units keep state in StateDirectory=mllm (~/.local/state/mllm).
    # systemd 254 and later, finding that missing while ~/.config/mllm (where
    # the units read <role>.yaml and <role>.env) exists, assumes the pre-254
    # layout and makes ~/.local/state/mllm a symlink to ~/.config/mllm; state
    # then lands in the configuration directory, behind a symlink the roles'
    # identity rules refuse (found live 2026-09-24). An empty owner-only
    # directory made now prevents that; nothing in it is ever touched.
    state_root=${XDG_STATE_HOME:-$HOME/.local/state}/mllm
    if [ -L "$state_root" ]; then
      say "warning: $state_root is a symlink (systemd's pre-254 compatibility link?); the roles refuse state behind it. Stop the unit, remove the link, and run this installer again."
    elif [ ! -e "$state_root" ]; then
      (umask 077 && mkdir -p "$state_root") && chmod 0700 "$state_root"
      say "created $state_root (0700) for the unit's state"
    fi
  fi
  reload_systemd
fi
sort -u "$record.tmp" >"$record"
rm -f "$record.tmp"

case ":${PATH:-}:" in
  *":$bin_dir:"*) ;;
  *) say "note: $bin_dir is not on PATH" ;;
esac
say "next: docs/operations/install.md ($share_dir/docs/operations/install.md)"
