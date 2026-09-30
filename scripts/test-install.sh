#!/usr/bin/env bash
# Exercise packaging/install.sh against a local, fake GitHub release.
#
#   scripts/test-install.sh [TARBALL]
#
# With TARBALL (a packaging/release.sh output), that archive is served;
# otherwise a fake one is built whose bin/capyctl is a stub that prints its
# version. Everything happens under a scratch HOME: no real systemd, no
# network, no GitHub. Each case runs under every POSIX shell found (sh, dash,
# bash --posix). Cases:
#
#   - file:// install (CAPYCTL_INSTALL_BASE_URL) of the binary and a user unit,
#     with the unit's ExecStart pointed at the installed binary;
#   - --system with PREFIX/UNIT_DIR (system unit, no --user reload);
#   - the private-repository API path (GITHUB_TOKEN, a fake curl standing in
#     for api.github.com, gh unavailable);
#   - refusals: a tarball whose checksum differs from SHA256SUMS, a tarball
#     SHA256SUMS does not list, a file inside the archive that does not match
#     the archive's own SHA256SUMS, an unknown --systemd role; none of them
#     leaves a binary behind;
#   - no --version while only pre-releases exist: the refusal names GitHub's
#     latest-release rule and --version, with and without a token;
#   - --uninstall removes what was installed and keeps state.
set -euo pipefail

root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
installer=$root/packaging/install.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/capyctl-test-install.XXXXXX")
trap 'rm -rf "$work"' EXIT

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
pass() { echo "ok:   $*"; }

arch=$(uname -m)
case $arch in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; esac

# --- the release fixture -----------------------------------------------------
release=$work/release
mkdir -p "$release"
if [ $# -ge 1 ]; then
  tarball=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
  name=$(basename "$tarball" .tar.gz)
  version=${name#capyctl-}
  version=${version%-linux-*}
  cp "$tarball" "$release/"
else
  version=0.0.0-test
  name=capyctl-$version-linux-$arch
  pkg=$work/build/$name
  mkdir -p "$pkg/bin" "$pkg/docs/operations"
  printf '#!/bin/sh\necho "capyctl %s"\n' "$version" >"$pkg/bin/capyctl"
  chmod 0755 "$pkg/bin/capyctl"
  mkdir -p "$pkg/packaging"
  cp -R "$root/packaging/systemd" "$pkg/packaging/"
  cp "$root/docs/operations/install.md" "$pkg/docs/operations/"
  cp "$root/LICENSE" "$pkg/LICENSE"
  printf 'name: capyctl\nversion: %s\n' "$version" >"$pkg/BUILDINFO"
  (cd "$pkg" && find . -type f -printf '%P\n' | LC_ALL=C sort | xargs -d '\n' sha256sum) >"$work/sums"
  mv "$work/sums" "$pkg/SHA256SUMS"
  tar -C "$work/build" -czf "$release/$name.tar.gz" "$name"
fi
cp "$installer" "$release/install.sh"
(cd "$release" && sha256sum "$name.tar.gz" install.sh >SHA256SUMS)

# A fake systemctl that records its arguments.
fakebin=$work/fakebin
mkdir -p "$fakebin"
cat >"$fakebin/systemctl" <<'EOF'
#!/bin/sh
echo "$*" >>"$SYSTEMCTL_LOG"
EOF
# gh is unavailable in every case: the fixture is served another way.
printf '#!/bin/sh\nexit 1\n' >"$fakebin/gh"
chmod 0755 "$fakebin/systemctl" "$fakebin/gh"

shells=()
for candidate in sh dash; do
  command -v "$candidate" >/dev/null && shells+=("$candidate")
done
shells+=("bash --posix")

case_no=0
fresh_home() {
  case_no=$((case_no + 1))
  home=$work/home-$case_no
  mkdir -p "$home"
  log=$work/systemctl-$case_no.log
  : >"$log"
}

# Run the installer as a user would: `run SHELL [VAR=value ...] -- [ARG ...]`.
run() {
  local shell=$1
  local -a vars=()
  shift
  while [ $# -gt 0 ] && [ "$1" != -- ]; do vars+=("$1"); shift; done
  [ $# -gt 0 ] && shift
  # shellcheck disable=SC2086  # "bash --posix" is a command and a flag
  env -i HOME="$home" PATH="$fakebin:/usr/bin:/bin" TMPDIR="$work" \
    SYSTEMCTL="$fakebin/systemctl" SYSTEMCTL_LOG="$log" "${vars[@]}" \
    $shell "$installer" "$@"
}

for shell in "${shells[@]}"; do
  label="[$shell]"

  # --- user install over file:// --------------------------------------------
  fresh_home
  if out=$(run "$shell" CAPYCTL_INSTALL_BASE_URL="file://$release" -- --version "v$version" --systemd host 2>&1); then
    bin=$home/.local/bin/capyctl
    unit=$home/.config/systemd/user/capyctl-host.service
    problems=()
    [ -x "$bin" ] || problems+=("no executable $bin")
    [ "$(stat -c %a "$bin" 2>/dev/null)" = 755 ] || problems+=("binary mode is not 0755")
    [ "$("$bin" --version 2>/dev/null)" = "capyctl $version" ] || problems+=("binary does not report $version")
    grep -qx "ExecStart=$bin start host --config \${CAPYCTL_CONFIG}" "$unit" 2>/dev/null ||
      problems+=("unit ExecStart does not run the installed binary")
    grep -q "^Documentation=file://$home/.local/share/capyctl/docs/operations/install.md" "$unit" 2>/dev/null ||
      problems+=("unit Documentation does not point at the installed guide")
    [ -f "$home/.local/share/capyctl/docs/operations/install.md" ] || problems+=("guide not installed")
    [ -f "$home/.local/share/capyctl/LICENSE" ] || problems+=("license not installed")
    [ -f "$home/.local/share/capyctl/packaging/systemd/system/capyctl-server.service" ] || problems+=("units not kept")
    grep -qx -- '--user daemon-reload' "$log" || problems+=("no systemctl --user daemon-reload")
    grep -q 'enable\|start' "$log" && problems+=("the unit was enabled or started")
    # systemd >= 254 would otherwise link the state root to ~/.config/capyctl.
    { [ -d "$home/.local/state/capyctl" ] && [ ! -L "$home/.local/state/capyctl" ] &&
      [ "$(stat -c %a "$home/.local/state/capyctl")" = 700 ]; } ||
      problems+=("the state root ~/.local/state/capyctl is not a 0700 directory")
    if [ "${#problems[@]}" -eq 0 ]; then pass "$label user install over file://"; else
      for p in "${problems[@]}"; do fail "$label user install: $p"; done; echo "$out" >&2; fi

    # Reinstall is idempotent; a second role's unit is added to the record.
    if run "$shell" CAPYCTL_INSTALL_BASE_URL="file://$release" -- --version "$version" --systemd standalone >/dev/null 2>&1 &&
      [ -f "$home/.config/systemd/user/capyctl-standalone.service" ] && [ -f "$unit" ]; then
      pass "$label reinstall adds a unit and keeps the first"
    else
      fail "$label reinstall"
    fi

    # --- uninstall keeps state ------------------------------------------------
    mkdir -p "$home/.local/state/capyctl/host"
    echo keep >"$home/.local/state/capyctl/host/sentinel"
    : >"$log"
    if run "$shell" -- --uninstall >/dev/null 2>&1 && [ ! -e "$bin" ] && [ ! -e "$unit" ] &&
      [ ! -e "$home/.config/systemd/user/capyctl-standalone.service" ] &&
      [ ! -e "$home/.local/share/capyctl" ] && [ -f "$home/.local/state/capyctl/host/sentinel" ] &&
      grep -qx -- '--user daemon-reload' "$log"; then
      pass "$label uninstall removes the install and keeps state"
    else
      fail "$label uninstall"
    fi

    # A state root systemd already replaced by its compatibility link is
    # reported, never touched.
    fresh_home
    mkdir -p "$home/.config/capyctl" "$home/.local/state"
    ln -s ../../.config/capyctl "$home/.local/state/capyctl"
    if out=$(run "$shell" CAPYCTL_INSTALL_BASE_URL="file://$release" -- --version "$version" --systemd host 2>&1) &&
      printf '%s\n' "$out" | grep -q 'is a symlink' && [ -L "$home/.local/state/capyctl" ]; then
      pass "$label warns about a linked state root and leaves it"
    else
      fail "$label linked state root"; echo "$out" >&2
    fi

  else
    fail "$label user install over file:// exited non-zero"
    echo "$out" >&2
  fi

  # --- system install under a prefix -----------------------------------------
  fresh_home
  if run "$shell" CAPYCTL_INSTALL_BASE_URL="file://$release" PREFIX="$home/usr" UNIT_DIR="$home/etc" -- \
    --system --version "$version" --systemd server >/dev/null 2>&1 &&
    grep -qx "ExecStart=$home/usr/bin/capyctl start server --config \${CAPYCTL_CONFIG}" "$home/etc/capyctl-server.service" &&
    grep -qx 'User=capyctl' "$home/etc/capyctl-server.service" &&
    grep -qx 'daemon-reload' "$log"; then
    pass "$label system install (system unit, system reload)"
  else
    fail "$label system install"
  fi

  # --- private repository through the API with a token ------------------------
  fresh_home
  api=$work/api-$case_no
  mkdir -p "$api"
  cat >"$api/release.json" <<EOF
{"url":"https://api.github.com/repos/o/r/releases/1","assets_url":"https://api.github.com/repos/o/r/releases/1/assets","tag_name":"v$version","assets":[
  {"url": "https://api.github.com/repos/o/r/releases/assets/11", "id": 11, "name": "SHA256SUMS", "uploader": {"url": "https://api.github.com/users/u"}},
  {"url": "https://api.github.com/repos/o/r/releases/assets/12", "id": 12, "name": "$name.tar.gz", "uploader": {"url": "https://api.github.com/users/u"}}]}
EOF
  cat >"$fakebin/curl" <<EOF
#!/bin/sh
# Fake curl: serves the fixture for api.github.com, requires the token.
out=; url=; auth=no
while [ \$# -gt 0 ]; do
  case \$1 in
    -o) out=\$2; shift 2 ;;
    -H) case \$2 in "Authorization: Bearer secret-token") auth=yes ;; esac; shift 2 ;;
    --proto) shift 2 ;;
    -*) shift ;;
    *) url=\$1; shift ;;
  esac
done
[ \$auth = yes ] || exit 22
case \$url in
  https://api.github.com/repos/o/r/releases/tags/v$version) cp "$api/release.json" "\$out" ;;
  https://api.github.com/repos/o/r/releases/assets/11) cp "$release/SHA256SUMS" "\$out" ;;
  https://api.github.com/repos/o/r/releases/assets/12) cp "$release/$name.tar.gz" "\$out" ;;
  *) exit 22 ;;
esac
EOF
  chmod 0755 "$fakebin/curl"
  if run "$shell" GITHUB_TOKEN=secret-token -- --repo o/r --version "$version" >/dev/null 2>&1 &&
    [ "$("$home/.local/bin/capyctl" --version)" = "capyctl $version" ]; then
    pass "$label private release through the API with GITHUB_TOKEN"
  else
    fail "$label private release through the API with GITHUB_TOKEN"
  fi
  fresh_home
  if run "$shell" GITHUB_TOKEN=wrong -- --repo o/r --version "$version" >/dev/null 2>&1 ||
    [ -e "$home/.local/bin/capyctl" ]; then
    fail "$label a rejected token must not install"
  else
    pass "$label a rejected token installs nothing"
  fi
  # Without --version, when the repository has only pre-releases: GitHub's
  # releases/latest answers 404, and the refusal names that cause and how to
  # pass a version, not only a private-repository guess.
  printf '#!/bin/sh\nexit 22\n' >"$fakebin/curl"
  chmod 0755 "$fakebin/curl"
  for token in "" secret-token; do
    fresh_home
    if out=$(run "$shell" ${token:+GITHUB_TOKEN=$token} -- --repo o/r 2>&1); then
      fail "$label no --version with only pre-releases: installed anyway"
    elif grep -qF "skips pre-releases" <<<"$out" && grep -qF -- "--version v0.1.0" <<<"$out" &&
      { [ -n "$token" ] || grep -qF "gh auth login" <<<"$out"; } && [ ! -e "$home/.local/bin/capyctl" ]; then
      pass "$label no --version with only pre-releases names the cause${token:+ (token)}"
    else
      fail "$label no --version with only pre-releases: message '$out'"
    fi
  done
  rm -f "$fakebin/curl"

  # --- refusals -----------------------------------------------------------------
  refuse() { # $1 description, $2 release dir, $3 expected message, rest: args
    local what=$1 dir=$2 message=$3 out
    shift 3
    fresh_home
    if out=$(run "$shell" CAPYCTL_INSTALL_BASE_URL="file://$dir" -- "$@" 2>&1); then
      fail "$label $what: installed anyway"
    elif [ -e "$home/.local/bin/capyctl" ]; then
      fail "$label $what: left a binary behind"
    elif ! grep -qF -- "$message" <<<"$out"; then
      fail "$label $what: message '$out' lacks '$message'"
    else
      pass "$label refuses $what"
    fi
  }

  bad=$work/bad-sum
  mkdir -p "$bad"
  cp "$release/$name.tar.gz" "$bad/"
  printf '%064d  %s\n' 0 "$name.tar.gz" >"$bad/SHA256SUMS"
  refuse "a checksum mismatch" "$bad" "checksum mismatch" --version "$version"

  unlisted=$work/unlisted
  mkdir -p "$unlisted"
  cp "$release/$name.tar.gz" "$unlisted/"
  printf '%064d  %s\n' 0 other.tar.gz >"$unlisted/SHA256SUMS"
  refuse "an unlisted tarball" "$unlisted" "lists no" --version "$version"

  inner=$work/inner
  if [ ! -d "$inner" ]; then
    mkdir -p "$inner/x"
    tar -xzf "$release/$name.tar.gz" -C "$inner/x"
    chmod u+w "$inner/x/$name/docs/operations/install.md"
    echo tampered >>"$inner/x/$name/docs/operations/install.md"
    tar -C "$inner/x" -czf "$inner/$name.tar.gz" "$name"
    (cd "$inner" && sha256sum "$name.tar.gz" >SHA256SUMS)
  fi
  refuse "a file that fails the archive's SHA256SUMS" "$inner" "does not match its SHA256SUMS" --version "$version"

  refuse "an unknown role" "$release" "--systemd takes" --version "$version" --systemd router
  refuse "a missing release" "$release" "cannot download" --version 9.9.9
done

echo
if [ "$failures" -gt 0 ]; then
  echo "install.sh tests FAILED ($failures)" >&2
  exit 1
fi
echo "install.sh tests passed"
