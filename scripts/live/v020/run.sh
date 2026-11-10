#!/usr/bin/env bash
# Foreground runner; shade_session.py independently owns rental deletion.
set -euo pipefail
umask 077
HERE=$(cd "$(dirname "$0")" && pwd)
TARGET='' SSH_PORT=22 OUT='' ONLY=all SKIP_SETUP=0 KEEP=0 PURGE_ONLY=0 DRY=0
MAX_SECONDS=28200 BINARY='' ENGINES='' FUNCTIONAL_ONLY=0
usage() { echo 'usage: run.sh --ssh USER@ADDR --binary PATH --out DIR [--ssh-port N] [--only rows] [--skip-setup] [--keep] [--purge-only] [--dry-run] [--max-seconds N] [--engines FILE] [--functional-only]' >&2; exit 2; }
while [ $# -gt 0 ]; do
  case $1 in
    --ssh|--binary|--out|--ssh-port|--only|--max-seconds|--engines)
      [ $# -ge 2 ] || usage
      case $1 in
        --ssh) TARGET=$2 ;; --binary) BINARY=$2 ;; --out) OUT=$2 ;;
        --ssh-port) SSH_PORT=$2 ;; --only) ONLY=$2 ;; --max-seconds) MAX_SECONDS=$2 ;;
        --engines) ENGINES=$2 ;;
      esac; shift ;;
    --skip-setup) SKIP_SETUP=1 ;; --keep) KEEP=1 ;; --purge-only) PURGE_ONLY=1 ;;
    --dry-run) DRY=1 ;; --functional-only) FUNCTIONAL_ONLY=1 ;; *) usage ;;
  esac
  shift
done
[[ $SSH_PORT =~ ^[1-9][0-9]*$ && $MAX_SECONDS =~ ^[1-9][0-9]*$ ]] || usage
[[ $ONLY =~ ^[A-Za-z0-9_,-]+$ ]] || usage
python3 "$HERE/v020.py" plan --only "$ONLY" >/dev/null
if [ "$DRY" = 1 ]; then
  ONLY=$ONLY FUNCTIONAL_ONLY=$FUNCTIONAL_ONLY SKIP_SETUP=$SKIP_SETUP bash "$HERE/remote.sh" plan
  exit 0
fi
[ -n "$TARGET" ] && [ -n "$OUT" ] || usage
[[ $TARGET =~ ^[A-Za-z0-9_.-]+@[A-Za-z0-9_.:-]+$ ]] || usage
if [ "$FUNCTIONAL_ONLY" = 1 ]; then
  [ "$SKIP_SETUP" = 1 ] && [ -f "$ENGINES" ] || { echo 'functional-only requires --skip-setup --engines FILE' >&2; exit 2; }
fi
if [ "$PURGE_ONLY" = 0 ]; then [ -f "$BINARY" ] && [ -x "$BINARY" ] || usage; fi
SSH=(ssh -p "$SSH_PORT" -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=2 "$TARGET")
RSYNC_SSH="ssh -p $SSH_PORT -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=2"
remote() { timeout --kill-after=5 210 "${SSH[@]}" "$@"; }
copy() { timeout --kill-after=5 120 rsync --checksum --fsync --timeout=30 -e "$RSYNC_SSH" "$@"; }
mkdir -p "$OUT"
chmod 700 "$OUT"
python3 - "$OUT" <<'PY'
import pathlib,time
root=pathlib.Path(__import__('sys').argv[1])
names=('evidence','settings.json','summary.json','summary.txt','completion.json','transport-completion.json','SHA256SUMS')
existing=[root/name for name in names if (root/name).exists()]
if existing:
    archive=root/'attempts'/str(time.time_ns())
    archive.mkdir(parents=True,mode=0o700)
    for path in existing: path.rename(archive/path.name)
PY
SCRATCH=$(mktemp -d)
READY=0 COLLECTED=0 PURGED=0
collect() {
  remote 'bash ~/capyctl-v020/live/remote.sh collect' >/dev/null 2>&1 || return 1
  copy -rltz "$TARGET:capyctl-v020/export/" "$OUT/" >/dev/null 2>&1
}
# Invoked by EXIT, including bounded interruption cleanup.
# shellcheck disable=SC2329
finish() {
  local rc=$? cleanup=0
  trap - EXIT INT TERM HUP
  set +e
  if [ "$READY" = 1 ]; then
    remote 'bash ~/capyctl-v020/live/remote.sh down' >/dev/null 2>&1 || cleanup=1
    if collect; then COLLECTED=1; else rc=1; fi
    if [ "$COLLECTED" = 1 ] && [ "$KEEP" = 0 ] && [ "$cleanup" = 0 ]; then
      if remote 'bash ~/capyctl-v020/live/remote.sh purge' >/dev/null 2>&1; then PURGED=1; else rc=1; fi
    fi
    [ "$cleanup" = 0 ] || rc=1
  fi
  python3 "$HERE/v020.py" summary --root "$OUT" --only "$ONLY" || rc=1
  python3 - "$OUT/transport-completion.json" "$rc" "$COLLECTED" "$PURGED" <<'PY'
import json,sys
open(sys.argv[1],'w').write(json.dumps(dict(exit_code=int(sys.argv[2]),collected=sys.argv[3]=='1',purged=sys.argv[4]=='1'))+'\n')
PY
  python3 "$HERE/v020.py" manifest --root "$OUT"
  rm -rf "$SCRATCH"
  [ ! -f "$OUT/summary.txt" ] || cat "$OUT/summary.txt"
  printf 'RUN_COMPLETE exit=%s evidence=%s\n' "$rc" "$OUT"
  exit "$rc"
}
trap finish EXIT
trap 'exit 143' TERM HUP
trap 'exit 130' INT
if [ "$PURGE_ONLY" = 1 ]; then
  READY=1
  exit 0
fi
# Refuse an unrelated/pre-existing session before copying files into it.
remote 'umask 077; if [ -e ~/LIVE-SESSION-IN-USE.txt ]; then exit 1; fi; mkdir -m 700 -p ~/capyctl-v020/live ~/capyctl-v020/bin ~/capyctl-v020/evidence'
READY=1
copy -rlpt "$HERE/" "$TARGET:capyctl-v020/live/" >/dev/null
remote 'python3 ~/capyctl-v020/live/session.py begin --root ~/capyctl-v020'
copy "$BINARY" "$TARGET:capyctl-v020/bin/capyctl" >/dev/null
printf 'ONLY=%q\nFUNCTIONAL_ONLY=%q\nSKIP_SETUP=%q\nMAX_SECONDS=%q\n' "$ONLY" "$FUNCTIONAL_ONLY" "$SKIP_SETUP" "$MAX_SECONDS" >"$SCRATCH/session.env"
if [ -n "$ENGINES" ]; then
  [ -f "$ENGINES" ] || usage
  copy "$ENGINES" "$TARGET:capyctl-v020/input-engines.env" >/dev/null
  printf '%s\n' "ENGINES_FILE=\$HOME/capyctl-v020/input-engines.env" >>"$SCRATCH/session.env"
fi
copy "$SCRATCH/session.env" "$TARGET:capyctl-v020/session.env" >/dev/null
python3 - "$OUT/settings.json" "$BINARY" "$ONLY" "$FUNCTIONAL_ONLY" "$MAX_SECONDS" <<'PY'
import hashlib,json,pathlib,sys
path,binary,only,functional,seconds=sys.argv[1:]
# Deliberately no command, path to binary, SSH target, environment, or credentials.
digest=hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest()
pathlib.Path(path).write_text(json.dumps(dict(binary_sha256=digest,only=only,functional_only=functional=='1',max_seconds=int(seconds)),indent=2)+'\n')
PY
remote "umask 077; setsid nohup timeout --kill-after=180 $MAX_SECONDS bash ~/capyctl-v020/live/remote.sh all >~/capyctl-v020/session.log 2>&1 </dev/null &"
start=$SECONDS last_contact=$SECONDS
while [ $((SECONDS - start)) -lt "$MAX_SECONDS" ]; do
  if collect; then
    last_contact=$SECONDS
    if [ -f "$OUT/completion.json" ]; then
      code=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["exit_code"])' "$OUT/completion.json")
      exit "$code"
    fi
  elif [ $((SECONDS - last_contact)) -ge 300 ]; then
    echo 'Connection lost; attempting bounded final collection and cleanup' >&2
    exit 1
  fi
  sleep 20
done
echo 'Session deadline reached' >&2
exit 124
