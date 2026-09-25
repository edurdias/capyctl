#!/usr/bin/env bash
# Local, CI-free verification of the service units and the release tarball.
#
#   scripts/verify-packaging.sh [TARBALL]
#
# 1. shellcheck over the packaging and release scripts.
# 2. Static checks of packaging/systemd: the SPEC §4.3 invariants a careless
#    edit could break (engines survive a restart, no draining ExecStop, a stop
#    timeout that covers the drain, foreground Type=simple, restart policy).
# 3. `systemd-analyze verify` of the system and user units, with ExecStart
#    pointed at the release binary under test.
# 4. The tarball: built twice with packaging/release.sh (same bytes both
#    times), or the TARBALL given. Its entries must match the expected set
#    exactly, with the expected owners and modes (no runtime files: the
#    runtime is compiled into the binary; no bytecode, no symlinks);
#    SHA256SUMS and the .sha256 must verify; the binary must be stripped,
#    report BUILDINFO's version and carry BUILDINFO's runtime manifest.
# 5. packaging/install.sh against a file:// release holding that tarball
#    (scripts/test-install.sh).
#
# A missing optional tool (shellcheck, systemd-analyze) is reported as
# SKIPPED; set MLLM_VERIFY_STRICT=1 to fail instead. SHELLCHECK names the
# ShellCheck binary when it is not on PATH. The tarball build accepts a dirty
# worktree (MLLM_RELEASE_ALLOW_DIRTY=1) because this checks packaging, not a
# publishable release. Run from anywhere inside the repository.
set -euo pipefail

root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
cd "$root"

failures=0
skipped=()
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
pass() { echo "ok:   $*"; }
skip() { echo "SKIP: $*" >&2; skipped+=("$*"); }

work=$(mktemp -d "${TMPDIR:-/tmp}/mllm-verify-packaging.XXXXXX")
trap 'rm -rf "$work"' EXIT

roles=(server host standalone)
units=()
for kind in system user; do
  for role in "${roles[@]}"; do
    units+=("packaging/systemd/$kind/mllm-$role.service")
  done
done

# --- 1. shellcheck -----------------------------------------------------------
shellcheck_bin=${SHELLCHECK:-$(command -v shellcheck || true)}
if [ -n "$shellcheck_bin" ]; then
  if "$shellcheck_bin" packaging/release.sh packaging/install.sh scripts/verify-packaging.sh \
    scripts/check-release-clean.sh scripts/test-install.sh; then
    pass "shellcheck"
  else
    fail "shellcheck"
  fi
else
  skip "shellcheck not found (set SHELLCHECK)"
fi

# --- 2. static unit checks ---------------------------------------------------
# The value of `key` in the [Service] section of unit file $1 (last one wins,
# as in systemd for single-valued settings).
service_value() {
  awk -v key="$2" '
    /^\[/ { section = $0; next }
    section == "[Service]" && index($0, key "=") == 1 { value = substr($0, length(key) + 2) }
    END { print value }' "$1"
}

# Seconds in a systemd time span of the simple forms the units use.
span_seconds() {
  local text=$1
  case "$text" in
    *min) echo $(( ${text%min} * 60 )) ;;
    *s) echo "${text%s}" ;;
    *[0-9]) echo "$text" ;;
    *) echo -1 ;;
  esac
}

# The largest shutdown.drain_timeout in the example role documents; the units
# must cover it (plus the default, 30s).
max_drain=30
for doc in docs/examples/server.yaml docs/examples/host.yaml docs/examples/standalone.yaml; do
  value=$(sed -n 's/^ *drain_timeout: *"\{0,1\}\([0-9]*\)s"\{0,1\}.*/\1/p' "$doc")
  if [ -n "$value" ] && [ "$value" -gt "$max_drain" ]; then
    max_drain=$value
  fi
done

for unit in "${units[@]}"; do
  if [ ! -f "$unit" ]; then
    fail "$unit missing"
    continue
  fi
  role=$(basename "$unit" .service)
  role=${role#mllm-}
  problems=()
  [ "$(service_value "$unit" Type)" = simple ] || problems+=("Type is not simple")
  [ "$(service_value "$unit" Restart)" = on-failure ] || problems+=("Restart is not on-failure")
  # Exit codes that never heal by restarting (crates/mllm-cli/src/output.rs):
  # 2 invalid config, 3 unauthorized, 5 unsupported, and for a host, 14 (the
  # controller revoked it; SPEC §4.1, ADR 0016).
  prevent=" $(service_value "$unit" RestartPreventExitStatus) "
  required=(2 3 5)
  if [ "$role" = host ]; then
    required+=(14)
  fi
  for code in "${required[@]}"; do
    case "$prevent" in
      *" $code "*) ;;
      *) problems+=("RestartPreventExitStatus lacks $code") ;;
    esac
  done
  [ -z "$(service_value "$unit" ExecStop)" ] || problems+=("has an ExecStop (a stop must never drain)")
  start=$(service_value "$unit" ExecStart)
  case "$start" in
    *"/bin/mllm start $role"*) ;;
    *) problems+=("ExecStart does not run 'mllm start $role'") ;;
  esac
  if [ "$role" != standalone ]; then
    case "$start" in
      *"--config "*) ;;
      *) problems+=("ExecStart has no --config") ;;
    esac
  fi
  kill_mode=$(service_value "$unit" KillMode)
  if [ "$role" = server ]; then
    [ "$kill_mode" = mixed ] || problems+=("server KillMode is '$kill_mode', expected mixed")
  else
    # SPEC §4.3: engines must survive a restart of the role.
    [ "$kill_mode" = process ] || problems+=("KillMode is '$kill_mode'; engines would not survive a restart")
    [ "$(service_value "$unit" PrivateTmp)" != yes ] || problems+=("PrivateTmp=yes would remove /tmp under retained engines")
    [ "$(service_value "$unit" OOMPolicy)" = continue ] || problems+=("OOMPolicy is not continue")
  fi
  stop=$(span_seconds "$(service_value "$unit" TimeoutStopSec)")
  if [ "$stop" -lt $((max_drain + 60)) ]; then
    problems+=("TimeoutStopSec ${stop}s is below drain_timeout ${max_drain}s + 60s")
  fi
  [ "$(service_value "$unit" NoNewPrivileges)" = yes ] || problems+=("NoNewPrivileges is not yes")
  [ "$(service_value "$unit" UMask)" = 0077 ] || problems+=("UMask is not 0077")
  if [ "${#problems[@]}" -eq 0 ]; then
    pass "$unit invariants"
  else
    for problem in "${problems[@]}"; do fail "$unit: $problem"; done
  fi
done

# --- 4 (first half). build or take the tarball --------------------------------
if [ $# -ge 1 ]; then
  tarball=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
  reproducible=unchecked
else
  export MLLM_RELEASE_ALLOW_DIRTY=1
  tarball=$(packaging/release.sh "$work/out1" | tail -n 1)
  second=$(packaging/release.sh "$work/out2" | tail -n 1)
  if cmp -s "$tarball" "$second"; then
    reproducible=yes
    pass "two builds produced identical tarballs"
  else
    reproducible=no
    fail "two builds of the same tree produced different tarballs"
  fi
fi
name=$(basename "$tarball" .tar.gz)

# --- 3. systemd-analyze verify -------------------------------------------------
tar -xzf "$tarball" -C "$work"
pkg="$work/$name"
if command -v systemd-analyze >/dev/null; then
  for kind in system user; do
    mkdir -p "$work/units-$kind"
    for role in "${roles[@]}"; do
      sed -e "s#^ExecStart=[^ ]*/bin/mllm #ExecStart=$pkg/bin/mllm #" \
        "packaging/systemd/$kind/mllm-$role.service" >"$work/units-$kind/mllm-$role.service"
    done
    flags=()
    [ "$kind" = user ] && flags+=(--user)
    # Only diagnostics about these units count; the host's own units may warn.
    if output=$(cd "$work/units-$kind" && systemd-analyze "${flags[@]}" verify --man=no ./mllm-*.service 2>&1); then
      :
    fi
    ours=$(grep -E '(^|/)mllm-(server|host|standalone)\.service' <<<"$output" || true)
    if [ -z "$ours" ]; then
      pass "systemd-analyze verify ($kind units)"
    else
      fail "systemd-analyze verify ($kind units):"
      echo "$ours" >&2
    fi
  done
else
  skip "systemd-analyze not found"
fi

# --- 4. tarball contents -----------------------------------------------------
expected="$work/expected"
actual="$work/actual"
{
  printf '%s\n' bin/mllm BUILDINFO SHA256SUMS
  git ls-files -- packaging/systemd docs/examples docs/operations/install.md
} >"$work/files"
# Every file plus each of its parent directories, under the package directory.
{
  echo "$name/"
  while IFS= read -r path; do
    echo "$name/$path"
    dir=$(dirname "$path")
    while [ "$dir" != . ]; do
      echo "$name/$dir/"
      dir=$(dirname "$dir")
    done
  done <"$work/files"
} | LC_ALL=C sort -u >"$expected"
tar -tzf "$tarball" | LC_ALL=C sort >"$actual"
if diff -u "$expected" "$actual" >"$work/contents.diff"; then
  pass "tarball entries match the expected set ($(wc -l <"$actual") entries)"
else
  fail "tarball entries differ from the expected set (- expected, + actual):"
  cat "$work/contents.diff" >&2
fi

for unit in system/mllm-server system/mllm-host system/mllm-standalone \
  user/mllm-server user/mllm-host user/mllm-standalone; do
  grep -qx "$name/packaging/systemd/$unit.service" "$actual" ||
    fail "tarball lacks packaging/systemd/$unit.service (is it tracked by git?)"
done

# Owners and modes, from the archive itself.
mode_problems=$(tar --numeric-owner -tvzf "$tarball" | awk -v pkg="$name/" '
  {
    perms = $1; owner = $2; path = $6
    rel = substr(path, length(pkg) + 1)
    if (owner != "0/0") print "owner " owner ": " path
    if (perms ~ /^l/ || perms ~ /^h/) print "link: " path
    if (path ~ /__pycache__|\.py[co]$/) print "bytecode: " path
    if (rel ~ /^runtime(\/|$)/) print "runtime file shipped (it is embedded in bin/mllm): " path
    if (rel == "bin/mllm") {
      want = "-rwxr-xr-x"
    } else {
      want = (perms ~ /^d/) ? "drwxr-xr-x" : "-rw-r--r--"
    }
    if (perms != want) print "mode " perms " (want " want "): " path
  }')
if [ -z "$mode_problems" ]; then
  pass "owners 0/0, no runtime files, no links or bytecode"
else
  fail "owner/mode problems:"
  echo "$mode_problems" >&2
fi

if (cd "$(dirname "$tarball")" && sha256sum -c --quiet "$name.tar.gz.sha256"); then
  pass "$name.tar.gz.sha256"
else
  fail "$name.tar.gz.sha256 does not verify"
fi
release_dir=$(dirname "$tarball")
if [ -f "$release_dir/SHA256SUMS" ]; then
  if (cd "$release_dir" && sha256sum -c --quiet SHA256SUMS) &&
    grep -q "  $name.tar.gz\$" "$release_dir/SHA256SUMS" && grep -q '  install.sh$' "$release_dir/SHA256SUMS"; then
    pass "release SHA256SUMS covers the tarball and install.sh"
  else
    fail "release SHA256SUMS does not verify or misses the tarball or install.sh"
  fi
fi
if (cd "$pkg" && sha256sum -c --quiet SHA256SUMS); then
  pass "SHA256SUMS"
else
  fail "SHA256SUMS does not verify"
fi
listed=$(cd "$pkg" && find . -type f ! -name SHA256SUMS -printf '%P\n' | LC_ALL=C sort)
if [ "$listed" = "$(awk '{print $2}' "$pkg/SHA256SUMS" | LC_ALL=C sort)" ]; then
  pass "SHA256SUMS covers every shipped file"
else
  fail "SHA256SUMS does not list exactly the shipped files"
fi

version=$(sed -n 's/^version: //p' "$pkg/BUILDINFO")
if [ "$("$pkg/bin/mllm" --version)" = "mllm $version" ]; then
  pass "bin/mllm --version reports $version"
else
  fail "bin/mllm --version does not report BUILDINFO version $version"
fi
# SPEC §3.3 / ADR 0001: the runtime the binary embeds is the tracked one.
manifest=$(sed -n 's/^runtime_manifest: //p' "$pkg/BUILDINFO")
if [ -n "$manifest" ] && grep -qaF "$manifest" "$pkg/bin/mllm"; then
  pass "bin/mllm embeds runtime manifest $manifest"
else
  fail "bin/mllm does not embed BUILDINFO's runtime manifest '$manifest'"
fi
if command -v readelf >/dev/null; then
  if readelf -S "$pkg/bin/mllm" | grep -q '\.symtab'; then
    fail "bin/mllm is not stripped"
  else
    pass "bin/mllm is stripped"
  fi
else
  skip "readelf not found; strip not checked"
fi

# --- 5. install.sh against this tarball ----------------------------------------
if scripts/test-install.sh "$tarball" >"$work/test-install.log" 2>&1; then
  pass "install.sh against a file:// release ($(grep -c '^ok:' "$work/test-install.log") checks)"
else
  fail "install.sh tests:"
  cat "$work/test-install.log" >&2
fi

echo
echo "reproducible: $reproducible"
if [ "${#skipped[@]}" -gt 0 ]; then
  echo "skipped: ${#skipped[@]}"
  if [ "${MLLM_VERIFY_STRICT:-0}" = 1 ]; then
    fail "skipped checks under MLLM_VERIFY_STRICT=1"
  fi
fi
if [ "$failures" -gt 0 ]; then
  echo "packaging verification FAILED ($failures)" >&2
  exit 1
fi
echo "packaging verification passed"
