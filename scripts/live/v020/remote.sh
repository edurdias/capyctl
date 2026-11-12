#!/usr/bin/env bash
# One supervised session; no system-package, driver or system CUDA modifications.
# shellcheck source-path=SCRIPTDIR
set -euo pipefail
umask 077
HERE=$(cd "$(dirname "$0")" && pwd)
export PYTHONDONTWRITEBYTECODE=1
export PYTHONPATH=$HERE
V020=${V020:-$HOME/capyctl-v020}
BIN=$V020/bin EVIDENCE=$V020/evidence
export V020 BIN EVIDENCE
ONLY=${ONLY:-all} FUNCTIONAL_ONLY=${FUNCTIONAL_ONLY:-0} SKIP_SETUP=${SKIP_SETUP:-0}
MAX_SECONDS=${MAX_SECONDS:-25200} ENGINES_FILE=${ENGINES_FILE:-}
SESSION_NAME=${SESSION_NAME:-}
if [ "${1:-}" != plan ] && [ -f "$V020/session.env" ]; then
  # This file is generated locally from validated flags, never ambient credentials.
  # shellcheck disable=SC1091
  . "$V020/session.env"
fi
# shellcheck source=setup.sh
. "$HERE/setup.sh"
marker=$HOME/LIVE-SESSION-IN-USE.txt
preflight() {
  local tool
  for tool in python3 nvidia-smi; do
    command -v "$tool" >/dev/null || { printf 'BLOCKED: missing prerequisite %s\n' "$tool"; return 1; }
  done
  nvidia-smi --query-gpu=name,compute_cap,driver_version --format=csv,noheader
  python3 - "$FUNCTIONAL_ONLY" <<'PY'
import os, platform, shutil, sys
if platform.machine() not in ('x86_64', 'aarch64'): raise SystemExit('BLOCKED: unsupported architecture')
if sys.argv[1] == '0' and shutil.disk_usage(os.path.expanduser('~')).free < 250 * 2**30:
    raise SystemExit('BLOCKED: fewer than 250 GiB free')
print('PREFLIGHT_OK')
PY
}
plan() {
  printf '%s\n' 'Remote plan: private ~/capyctl-v020, umask 077; exclusive LIVE-SESSION-IN-USE marker.'
  printf '%s\n' 'Preflight compiler/tools/GPU; never apt, driver updates, or system CUDA changes.'
  setup_plan
  python3 "$HERE/v020.py" plan --only "$ONLY"
  python3 - "$ONLY" "$SKIP_SETUP" <<'PY'
import sys
from v020 import select_rows
from setup_selection import dependencies
rows = sum(row['budget'] for row in select_rows(sys.argv[1]))
setup = 3600 if sys.argv[2] != '1' and dependencies(sys.argv[1]) else 0
print(f'Catalog worst-case row budgets: {rows}s; setup planning allowance: {setup}s; combined: {rows + setup}s ({(rows + setup)/3600:.2f}h).')
print('Setup allowance is an estimate; the outer session deadline remains authoritative.')
PY
  printf '%s\n' 'Roles: standalone 19743/19744; server 19843-19847; skew/upgrade 19943-19947; private state mode 700.'
  printf '%s\n' 'Copy only sanitized JSON evidence, freeze/fingerprints and summaries; SHA-256 manifest locally.'
  printf '%s\n' 'Finally delete managed deployments, stop owned roles, require GPU PID subset of preflight, remove marker; purge created paths unless --keep.'
}
collect() {
  [ -d "$EVIDENCE" ] || return 0
  # No state, credentials, raw logs, process argv, or environment crosses SSH.
  mkdir -p "$V020/export/evidence"
  cp -a "$EVIDENCE/." "$V020/export/evidence/"
  local path
  for path in summary.json summary.txt completion.json preflight.json; do
    [ ! -f "$V020/$path" ] || cp "$V020/$path" "$V020/export/$path"
  done
  python3 - "$V020/export" "$V020/state" <<'PY'
import json,pathlib,socket,sys
from v020 import Context, credentials
cleaner=Context.__new__(Context)
cleaner.secrets=[]
for path in pathlib.Path(sys.argv[2]).rglob('*'):
    if path.is_file() and (path.name=='credentials' or path.name=='server-credentials.json'):
        cleaner.secrets.extend(credentials(path).values())
for path in pathlib.Path(sys.argv[1]).rglob('*'):
    if path.is_file() and not path.is_symlink():
        text=path.read_text()
        try:
            text=json.dumps(cleaner.clean(json.loads(text)), indent=2) + '\n'
        except ValueError:
            text=cleaner.clean(text)
        path.write_text(text.replace(socket.gethostname(), '<host>'))
PY
  python3 "$HERE/v020.py" manifest --root "$V020/export"
}
all() {
  mkdir -p "$V020" "$EVIDENCE" "$BIN"
  chmod 700 "$V020" "$EVIDENCE" "$BIN"
  local owner=${SESSION_NAME:-$V020}
  if [ -e "$marker" ]; then
    [ -n "$SESSION_NAME" ] && [ "$(cat "$marker")" = "$owner" ] || { echo 'BLOCKED: another live session owns the marker'; return 1; }
  else
    ( set -o noclobber; printf '%s\n' "$owner" >"$marker" ) 2>/dev/null || { echo 'BLOCKED: another live session owns the marker'; return 1; }
  fi
  python3 - "$V020" <<'PY'
import json, pathlib, subprocess, sys
root=pathlib.Path(sys.argv[1])
pids=subprocess.run(['nvidia-smi','--query-compute-apps=pid','--format=csv,noheader,nounits'],capture_output=True,text=True,timeout=20,check=True).stdout
(root/'gpu-before.json').write_text(json.dumps(sorted(int(x) for x in pids.splitlines() if x.strip().isdigit())))
PY
  finish() {
    local rc=$? down_rc=0
    trap - EXIT
    python3 "$HERE/session.py" down --root "$V020" || down_rc=$?
    if [ "$down_rc" != 0 ]; then rc=1; fi
    python3 "$HERE/v020.py" summary --root "$V020" --only "$ONLY" || rc=1
    python3 - "$V020/completion.json" "$rc" <<'PY'
import json,sys
open(sys.argv[1],'w').write(json.dumps(dict(exit_code=int(sys.argv[2]),state='complete'))+'\n')
PY
    collect || rc=1
    if [ "$down_rc" = 0 ] && [ "$(cat "$marker" 2>/dev/null)" = "$owner" ]; then rm -f "$marker"; fi
    exit "$rc"
  }
  trap finish EXIT
  preflight
  if [ "$FUNCTIONAL_ONLY" = 1 ] && [ "$SKIP_SETUP" != 1 ]; then
    echo 'BLOCKED: functional-only requires --skip-setup and --engines; existing environments are read-only'
    return 1
  fi
  if [ "$SKIP_SETUP" = 0 ]; then setup; fi
  local engines=$V020/engines.env
  if [ "$SKIP_SETUP" = 1 ] && [ -n "$ENGINES_FILE" ]; then engines=$ENGINES_FILE; fi
  [ -f "$engines" ] || { echo 'BLOCKED: engine locations are missing'; return 1; }
  local flags=()
  [ "$FUNCTIONAL_ONLY" = 0 ] || flags+=(--functional-only)
  [ "$(uname -m)" != aarch64 ] || flags+=(--unified)
  python3 "$HERE/v020.py" run --root "$V020" --engines "$engines" --only "$ONLY" --max-seconds "$MAX_SECONDS" "${flags[@]}"
}
purge() {
  python3 "$HERE/session.py" down --root "$V020"
  # Only paths recorded before creation by this setup are removable.
  python3 "$HERE/session.py" purge --root "$V020"
}
case ${1:-} in
  plan) plan ;; preflight) preflight ;; setup) setup ;; all) all ;;
  collect) collect ;; down) python3 "$HERE/session.py" down --root "$V020" ;;
  purge) purge ;; *) echo 'usage: remote.sh plan|preflight|setup|all|collect|down|purge' >&2; exit 2 ;;
esac
