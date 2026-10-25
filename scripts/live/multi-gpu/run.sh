#!/usr/bin/env bash
# The multi-GPU live rows (ADR 0035 MG1-MG14) and the download bandwidth test, on
# one rented x86_64 machine with two identical NVIDIA GPUs, in one session: push
# this checkout, install the approved engines and build CapyCTL there, run the
# rows detached, copy the results back, write the verdict, and remove everything
# the session created on the machine.
#
# usage: scripts/live/multi-gpu/run.sh --ssh USER@ADDRESS [options]
#
#   --ssh TARGET       the rented machine (ssh destination; key authentication)
#   --ssh-port N       its SSH port (22)
#   --only LIST        rows to run, e.g. MG1,MG7,BW (all); BW is the bandwidth test
#   --stall S          groups.stall_timeout of the roles, seconds (30; MG8 waits on it)
#   --cycles N         MG4/MG5 park and wake cycles (5); --hb-cycles N for MG6 (3)
#   --port N           the roles' inference port (18743); management N+1, engines 8740-8759
#   --skip-setup       reuse the tools, builds and engines of an earlier run on the machine
#   --keep             leave everything on the machine (no purge) for a follow-up run
#   --purge-only       only remove what earlier runs created there
#   --out DIR          local results (<main checkout>/target/live/multi-gpu/<run>)
#   --dry-run          print every step, local and remote, and contact no machine
#
# The machine's address never reaches the output: it is replaced by `rented-host`.
# One live session at a time: the host phases refuse to start while any engine runs.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
REPO_MAIN=$(dirname "$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir)")
usage() { sed -n '2,25p' "$0"; exit 2; }

TARGET="" SSH_PORT=22 ONLY=ALL STALL=30 CYCLES=5 HB_CYCLES=3 PORT=18743
SKIP_SETUP=0 KEEP=0 PURGE_ONLY=0 OUT="" DRY=0 OLD_TAG=v0.1.8
ARGS=("$@")
while [ $# -gt 0 ]; do
  case $1 in
    --ssh) TARGET=$2; shift ;;
    --ssh-port) SSH_PORT=$2; shift ;;
    --only) ONLY=${2^^}; shift ;;
    --stall) STALL=$2; shift ;;
    --cycles) CYCLES=$2; shift ;;
    --hb-cycles) HB_CYCLES=$2; shift ;;
    --port) PORT=$2; shift ;;
    --skip-setup) SKIP_SETUP=1 ;;
    --keep) KEEP=1 ;;
    --purge-only) PURGE_ONLY=1 ;;
    --out) OUT=$2; shift ;;
    --dry-run) DRY=1 ;;
    -h|--help) usage ;;
    *) echo "unknown option $1" >&2; usage ;;
  esac
  shift
done
[ -n "$TARGET" ] || [ "$DRY" = 1 ] || { echo "--ssh is required" >&2; usage; }
[ -n "$TARGET" ] || TARGET=user@rented-host
for n in "$STALL" "$CYCLES" "$HB_CYCLES" "$PORT" "$SSH_PORT"; do
  [[ $n =~ ^[1-9][0-9]*$ ]] || { echo "numbers only: $n" >&2; exit 2; }
done
if [ "$ONLY" != ALL ]; then
  for id in ${ONLY//,/ }; do
    [[ $id =~ ^(MG([1-9]|1[0-4])|BW)$ ]] || { echo "--only takes MG1-MG14 and BW separated by commas, not '$id'" >&2; exit 2; }
  done
else
  ONLY=all
fi
MPORT=$((PORT + 1))

RUN_ID=$(date +%F-%H%M)-multi-gpu
OUT=${OUT:-$REPO_MAIN/target/live/multi-gpu/$RUN_ID}
HOST_ADDR=${TARGET#*@}
SSH=(ssh -p "$SSH_PORT" -o BatchMode=yes -o ConnectTimeout=20 -o ServerAliveInterval=30 "$TARGET")
step() { printf '\n== %s\n' "$*"; }
redact() { sed -u -e "s#$HOST_ADDR#rented-host#g" -e "s#/home/[A-Za-z0-9._-]*/#~/#g" -e "s#/root/#~/#g"; }
exec > >(redact) 2>&1
x() { if [ "$DRY" = 1 ]; then printf '  $ %s\n' "$(printf '%q ' "$@")"; else "$@"; fi; }
rsh() { x "${SSH[@]}" "$@"; }
# Shown in a dry run only; the real commands expand ~ on the machine.
# shellcheck disable=SC2088
REMOTE_SRC='~/mg/src'

if [ "$DRY" = 1 ]; then
  step "dry run: the plan of every host phase, printed by remote.sh plan on this machine"
  PLANDIR=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/capyctl-mg-plan.XXXXXX")
  printf 'ONLY=%q\nSTALL=%q\nCYCLES=%q\nHB_CYCLES=%q\nPORT=%q\nMPORT=%q\n' "$ONLY" "$STALL" "$CYCLES" "$HB_CYCLES" "$PORT" "$MPORT" >"$PLANDIR/mg.env"
  MG=$PLANDIR bash "$HERE/remote.sh" plan | sed -e "s#$PLANDIR#~/mg#g" -e "s#${PLANDIR/#$HOME/\~}#~/mg#g" ||
    { rm -rf "$PLANDIR"; echo "dry run: the plan failed" >&2; exit 1; }
  rm -rf "$PLANDIR"
fi

if [ "$PURGE_ONLY" = 1 ]; then
  step "remove what earlier runs created on the machine"
  rsh "bash ~/mg/live/remote.sh purge"
  exit 0
fi

COMMIT=$(git -C "$REPO" rev-parse HEAD)
step "multi-GPU live rows ($ONLY) from capyctl ${COMMIT:0:9} -> $OUT"
git -C "$REPO" diff --quiet HEAD -- || echo "warning: uncommitted changes are not pushed; the machine builds ${COMMIT:0:9}"
x mkdir -p "$OUT"

step "preflight"
rsh "mkdir -m 700 -p ~/mg/live"
x rsync -e "ssh -p $SSH_PORT" -rlpz --chmod=Du=rwx,Dgo=,Fu=rwx,Fgo= "$HERE/remote.sh" "$HERE/mg.py" "$TARGET:mg/live/"
if [ "$DRY" = 0 ]; then
  "${SSH[@]}" "bash ~/mg/live/remote.sh preflight" | tee "$OUT/preflight.txt"
  grep -q PREFLIGHT_OK "$OUT/preflight.txt" || { echo "preflight failed; nothing was installed"; exit 1; }
else
  rsh "bash ~/mg/live/remote.sh preflight"
fi

if [ "$SKIP_SETUP" = 0 ]; then
  step "push the source trees: ${COMMIT:0:9} and $OLD_TAG"
  rsh "rm -rf ~/mg/src ~/mg/src-old && mkdir -p ~/mg/src ~/mg/src-old"
  if [ "$DRY" = 1 ]; then
    x git -C "$REPO" archive --format=tar HEAD '|' ssh rented-host tar -x -C "$REMOTE_SRC"
    x git -C "$REPO" archive --format=tar "$OLD_TAG" '|' ssh rented-host tar -x -C "$REMOTE_SRC-old"
  else
    git -C "$REPO" archive --format=tar HEAD | "${SSH[@]}" "tar -x -C ~/mg/src"
    git -C "$REPO" archive --format=tar "$OLD_TAG" | "${SSH[@]}" "tar -x -C ~/mg/src-old"
  fi
fi

step "settings"
ENVFILE=$(mktemp "${XDG_CACHE_HOME:-$HOME/.cache}/capyctl-mg-env.XXXXXX")
printf '%s=%q\n' ONLY "$ONLY" STALL "$STALL" CYCLES "$CYCLES" HB_CYCLES "$HB_CYCLES" PORT "$PORT" MPORT "$MPORT" \
  OLD_TAG "$OLD_TAG" SKIP_SETUP "$SKIP_SETUP" CAPYCTL_COMMIT "$COMMIT" >"$ENVFILE"
cat "$ENVFILE"
x rsync -e "ssh -p $SSH_PORT" -z "$ENVFILE" "$TARGET:mg/mg.env"
rm -f "$ENVFILE"

# The host phases run detached (setsid), so a dropped connection does not stop
# them; this side follows the progress log until the done marker appears.
step "setup and rows on the machine (progress below)"
STARTED=$(date -u +%FT%TZ)
rc=0
if [ "$DRY" = 1 ]; then
  rsh "cd ~/mg && MG=~/mg setsid nohup bash live/remote.sh all >all.log 2>&1 </dev/null &"
else
  "${SSH[@]}" "cd ~/mg && MG=\$HOME/mg setsid nohup bash live/remote.sh all >all.log 2>&1 </dev/null & echo started"
  seen=0
  while :; do
    sleep 30
    lines=$("${SSH[@]}" "tail -n +$((seen + 1)) ~/mg/res/progress.log 2>/dev/null; true" 2>/dev/null) || continue
    if [ -n "$lines" ]; then printf '%s\n' "$lines"; seen=$((seen + $(printf '%s\n' "$lines" | wc -l))); fi
    rc=$("${SSH[@]}" "cat ~/mg/done 2>/dev/null; true" 2>/dev/null) || continue
    [ -z "$rc" ] || break
  done
fi
ENDED=$(date -u +%FT%TZ)
step "host phases ended with status $rc"

step "copy the results back"
x mkdir -p "$OUT/raw"
x rsync -e "ssh -p $SSH_PORT" -rlz "$TARGET:mg/res/" "$OUT/raw/"
x rsync -e "ssh -p $SSH_PORT" -rlz "$TARGET:mg/dep" "$TARGET:mg/all.log" "$OUT/raw/"
if [ "$DRY" = 0 ]; then
  find "$OUT/raw" -type f \( -name '*.log' -o -name '*.txt' -o -name '*.tsv' -o -name '*.yaml' -o -name '*.json' -o -name '*.jsonl' -o -name '*.out' \) \
    -exec sed -i -e "s#$HOST_ADDR#rented-host#g" -e "s#/home/[A-Za-z0-9._-]*/#~/#g" -e "s#/root/#~/#g" {} +
fi

step "verdict"
if [ "$DRY" = 0 ]; then
  MACHINE=$(grep -E '^[0-9]+, ' "$OUT/preflight.txt" | cut -d, -f2,3 | sort | uniq -c | tr -s ' ' | paste -sd';')
  python3 -c 'import json, sys
keys = ["commit", "machine", "command", "started", "ended", "host_status"]
print(json.dumps(dict(zip(keys, sys.argv[1:])), indent=2))' \
    "$COMMIT" "$MACHINE" "scripts/live/multi-gpu/run.sh $(printf '%q ' "${ARGS[@]}")" "$STARTED" "$ENDED" "$rc" >"$OUT/settings.json"
  python3 "$HERE/mg.py" verdict "$OUT/raw/results.tsv" "$OUT/verdict.md" --settings "$OUT/settings.json"
else
  x python3 "$HERE/mg.py" verdict "$OUT/raw/results.tsv" "$OUT/verdict.md" --settings "$OUT/settings.json"
fi

if [ "$KEEP" = 1 ]; then
  step "kept on the machine (--keep): run again with --skip-setup, or --purge-only when done"
else
  step "remove everything the session created on the machine"
  rsh "bash ~/mg/live/remote.sh purge"
  echo "The machine can be shut down now."
fi
exit "${rc:-1}"
