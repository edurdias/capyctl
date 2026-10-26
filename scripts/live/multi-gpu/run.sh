#!/usr/bin/env bash
# Foreground ADR0035 qualification runner. A separate supervisor owns the rental.
# Usage: run.sh --ssh USER@ADDRESS [--ssh-port N] [--only MG1,MG2,...,BW]
#   --stall S --cycles N --hb-cycles N --port N
#   --skip-setup   reuse builds only if their recorded commit matches this checkout
#   --keep         do not purge after a successful final evidence copy
#   --purge-only   collect existing evidence before removing the remote session
#   --out DIR      durable session directory; reruns append unique attempts
#   --max-seconds N --disconnect-seconds N --poll-seconds N
#   --dry-run      print the remote plan without contacting a machine
set -euo pipefail
umask 077
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
REPO_MAIN=$(dirname "$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir)")
usage() { printf '%s\n' 'Usage: run.sh --ssh USER@ADDRESS [--only MG1,...,BW] [--out DIR] [--skip-setup] [--keep] [--dry-run]' >&2; exit 2; }
TARGET='' SSH_PORT=22 ONLY=all STALL=30 CYCLES=5 HB_CYCLES=3 PORT=18743
SKIP_SETUP=0 KEEP=0 PURGE_ONLY=0 OUT='' DRY=0 OLD_TAG=v0.1.8
MAX_SECONDS=28800 DISCONNECT_SECONDS=300 POLL_SECONDS=15
while [ $# -gt 0 ]; do
  case $1 in
    --ssh|--ssh-port|--only|--stall|--cycles|--hb-cycles|--port|--out|--max-seconds|--disconnect-seconds|--poll-seconds)
      [ $# -ge 2 ] || usage
      case $1 in
        --ssh) TARGET=$2 ;; --ssh-port) SSH_PORT=$2 ;; --only) ONLY=${2^^} ;;
        --stall) STALL=$2 ;; --cycles) CYCLES=$2 ;; --hb-cycles) HB_CYCLES=$2 ;;
        --port) PORT=$2 ;; --out) OUT=$2 ;; --max-seconds) MAX_SECONDS=$2 ;;
        --disconnect-seconds) DISCONNECT_SECONDS=$2 ;; --poll-seconds) POLL_SECONDS=$2 ;;
      esac
      shift ;;
    --skip-setup) SKIP_SETUP=1 ;; --keep) KEEP=1 ;; --purge-only) PURGE_ONLY=1 ;;
    --dry-run) DRY=1 ;; -h|--help) usage ;; *) usage ;;
  esac
  shift
done
[ -n "$TARGET" ] || [ "$DRY" = 1 ] || usage
TARGET=${TARGET:-user@rented-host}
for n in "$SSH_PORT" "$STALL" "$CYCLES" "$HB_CYCLES" "$PORT" "$MAX_SECONDS" "$DISCONNECT_SECONDS" "$POLL_SECONDS"; do
  [[ $n =~ ^[1-9][0-9]*$ ]] || usage
done
if [ "${ONLY^^}" = ALL ]; then ONLY=all; else
  [[ $ONLY =~ ^(MG([1-9]|1[0-4])|BW)(,(MG([1-9]|1[0-4])|BW))*$ ]] || usage
fi
MPORT=$((PORT + 1))
ATTEMPT_ID=$(date -u +%Y%m%dT%H%M%S%N)-$$
OUT=${OUT:-$REPO_MAIN/target/live/multi-gpu/$ATTEMPT_ID}
HOST_ADDR=${TARGET#*@}
SSH=(ssh -p "$SSH_PORT" -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=2 "$TARGET")
RSYNC_SSH="ssh -p $SSH_PORT -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=2"
remote() { timeout --kill-after=2 25 "${SSH[@]}" "$@"; }
copy() { timeout --kill-after=2 30 rsync --checksum --fsync --timeout=20 -e "$RSYNC_SSH" "$@"; }
SCRATCH=$HOME/.cache/capyctl-agent-tmp/mg-run
mkdir -p "$SCRATCH"
WORK=$(mktemp -d "$SCRATCH/attempt.XXXXXX")
if [ "$DRY" = 1 ]; then
  trap 'rm -rf "$WORK"' EXIT
  printf '%s=%q\n' ONLY "$ONLY" STALL "$STALL" CYCLES "$CYCLES" HB_CYCLES "$HB_CYCLES" PORT "$PORT" MPORT "$MPORT" >"$WORK/mg.env"
  MG=$WORK bash "$HERE/remote.sh" plan
  exit
fi
exec > >(trap '' INT TERM HUP; python3 "$HERE/mg.py" redact --host "$HOST_ADDR") 2>&1
COMMIT=$(git -C "$REPO" rev-parse HEAD)
git -C "$REPO" diff --quiet HEAD -- crates runtime ||
  printf 'WARNING: uncommitted product changes are not deployed; source commit is %s\n' "$COMMIT" >&2
HARNESS=$(sha256sum "$HERE/run.sh" "$HERE/remote.sh" "$HERE/mg.py" | sha256sum | cut -d' ' -f1)
ATTEMPT=$OUT/attempts/$ATTEMPT_ID
mkdir -p "$ATTEMPT/raw"
# No command line, SSH destination, environment dump, or credentials in metadata.
python3 - "$ATTEMPT/settings.json" "$COMMIT" "$HARNESS" "$ONLY" "$CYCLES" "$HB_CYCLES" "$STALL" "$ATTEMPT_ID" "$MAX_SECONDS" "$PORT" "$SKIP_SETUP" "$DISCONNECT_SECONDS" "$POLL_SECONDS" <<'PY'
import datetime, json, os, sys
path, commit, harness, only, cycles, hb_cycles, stall, attempt, deadline, port, skip, disconnect, poll = sys.argv[1:]
with open(path, 'w') as f:
    json.dump(dict(commit=commit, harness=harness, only=only, cycles=int(cycles), hb_cycles=int(hb_cycles),
                   stall=int(stall), attempt=attempt, max_seconds=int(deadline), port=int(port),
                   skip_setup=skip == "1", disconnect_seconds=int(disconnect), poll_seconds=int(poll),
                   engines={"vllm": "0.32.0", "sglang": "0.5.22", "llama.cpp": "0.7.0"},
                   started=datetime.datetime.now(datetime.timezone.utc).isoformat()), f, indent=2)
    f.flush()
    os.fsync(f.fileno())
PY
printf '%s=%q\n' ONLY "$ONLY" STALL "$STALL" CYCLES "$CYCLES" HB_CYCLES "$HB_CYCLES" PORT "$PORT" MPORT "$MPORT" \
  OLD_TAG "$OLD_TAG" SKIP_SETUP "$SKIP_SETUP" CAPYCTL_COMMIT "$COMMIT" ATTEMPT_ID "$ATTEMPT_ID" >"$WORK/mg.env"
REMOTE_READY=0 HOST_STATUS='' FINAL_COPY=0 PURGED=0 REASON=setup SIGNALLED=0
collect() {
  local snapshot_rc=0
  remote "ATTEMPT_ID=$ATTEMPT_ID bash ~/mg/live/remote.sh collect" >>"$ATTEMPT/transport.log" 2>&1 || snapshot_rc=$?
  # Only the remote sanitized allowlist crosses the connection, never raw state.
  # All attempts are retained, and rsync never deletes the local last good copy.
  copy -rltz "$TARGET:mg/evidence/" "$OUT/attempts/" >>"$ATTEMPT/transport.log" 2>&1 || return 1
  python3 "$HERE/mg.py" sanitize-tree "$OUT/attempts" --host "$HOST_ADDR" || return 1
  return "$snapshot_rc"
}
# Invoked by the EXIT trap, including failure and signal paths.
# shellcheck disable=SC2329
finish() {
  local original=$? rc=1 tries=3 i verdict_rc=1
  trap - EXIT
  trap 'SIGNALLED=1; REASON=interrupted; tries=1' INT TERM HUP
  set +e
  [ "$SIGNALLED" = 0 ] || tries=1
  if [ "$REMOTE_READY" = 1 ]; then
    for ((i=0; i<tries; i++)); do
      if collect; then FINAL_COPY=1; break; fi
    done
  fi
  if [ -f "$ATTEMPT/host-completion.json" ]; then
    HOST_STATUS=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["exit_code"])' "$ATTEMPT/host-completion.json")
  fi
  python3 "$HERE/mg.py" verdict "$ATTEMPT/raw/results.tsv" "$OUT/verdict.md" --settings "$ATTEMPT/settings.json" --attempts "$OUT/attempts" --only "$ONLY" --host-status "$HOST_STATUS" --collection-status "$FINAL_COPY" >"$ATTEMPT/verdict.log" 2>&1
  verdict_rc=$?
  [ "$verdict_rc" = 0 ] || REASON=qualification_failed
  cp "$OUT/verdict.md" "$OUT/verdict.json" "$ATTEMPT/" 2>/dev/null
  if [ "$FINAL_COPY" = 1 ] && [ -n "$HOST_STATUS" ] && [ "$KEEP" = 0 ]; then
    if remote "ATTEMPT_ID=$ATTEMPT_ID bash ~/mg/live/remote.sh purge" >>"$ATTEMPT/purge.log" 2>&1; then PURGED=1; else REASON=purge_failed; fi
  fi
  if [ "$SIGNALLED" = 0 ] && [ "$original" = 0 ] && [ "$HOST_STATUS" = 0 ] && [ "$FINAL_COPY" = 1 ] && [ "$verdict_rc" = 0 ] && { [ "$KEEP" = 1 ] || [ "$PURGED" = 1 ]; }; then
    rc=0 REASON=complete
  fi
  [ "$FINAL_COPY" = 1 ] || REASON=final_collection_failed
  if ! python3 "$HERE/mg.py" sanitize-tree "$OUT/attempts" --host "$HOST_ADDR"; then rc=1 REASON=redaction_failed; fi
  python3 - "$OUT/completion.json" "$ATTEMPT_ID" "$rc" "$HOST_STATUS" "$FINAL_COPY" "$PURGED" "$REASON" <<'PY'
import datetime, json, os, sys
path, attempt, rc, host, copied, purged, reason = sys.argv[1:]
doc = dict(state='complete', attempt=attempt, exit_code=int(rc), host_exit=int(host) if host.isdigit() else None,
           final_collection=copied == '1', purged=purged == '1', reason=reason,
           ended=datetime.datetime.now(datetime.timezone.utc).isoformat())
with open(path + '.partial', 'w') as f:
    json.dump(doc, f, indent=2); f.flush(); os.fsync(f.fileno())
os.replace(path + '.partial', path)
PY
  cp "$OUT/completion.json" "$ATTEMPT/completion.json"
  rm -rf "$WORK"
  printf 'RUN_COMPLETE attempt=%s exit=%s evidence=%s\n' "$ATTEMPT_ID" "$rc" "$OUT"
  exit "$rc"
}
trap 'finish' EXIT
trap 'SIGNALLED=1; REASON=interrupted; exit 143' TERM HUP
trap 'SIGNALLED=1; REASON=interrupted; exit 130' INT
# A stale completion marker must never describe a newly running attempt.
python3 - "$OUT/completion.json" "$ATTEMPT_ID" <<'PY'
import json, os, sys
path, attempt = sys.argv[1:]
with open(path + '.partial', 'w') as f:
    json.dump(dict(state='running', attempt=attempt), f); f.flush(); os.fsync(f.fileno())
os.replace(path + '.partial', path)
PY
if [ "$PURGE_ONLY" = 1 ]; then
  # Finalize stopped attempts and refresh their exports before destructive cleanup.
  timeout --kill-after=2 660 "${SSH[@]}" 'bash ~/mg/live/remote.sh finalize'
  copy -rltz "$TARGET:mg/evidence/" "$OUT/attempts/"
  python3 "$HERE/mg.py" sanitize-tree "$OUT/attempts" --host "$HOST_ADDR"
  # This mode never deletes a remote session when its evidence copy failed.
  remote 'bash ~/mg/live/remote.sh purge'
  python3 - "$OUT/completion.json" "$ATTEMPT_ID" <<'PY'
import json, os, sys
path, attempt = sys.argv[1:]
with open(path + ".partial", "w") as stream:
    json.dump(dict(state="complete", attempt=attempt, exit_code=0, final_collection=True, purged=True,
                   reason="purge_only"), stream)
    stream.flush()
    os.fsync(stream.fileno())
os.replace(path + ".partial", path)
PY
  trap - EXIT
  rm -rf "$WORK"
  exit 0
fi
printf 'RUN_STARTED attempt=%s commit=%s evidence=%s\n' "$ATTEMPT_ID" "$COMMIT" "$OUT"
remote "umask 077; mkdir -m 700 -p ~/mg/live ~/mg/attempts/$ATTEMPT_ID/raw"
copy -rlpz --chmod=Du=rwx,Dgo=,Fu=rwx,Fgo= "$HERE/remote.sh" "$HERE/mg.py" "$TARGET:mg/live/"
copy -z "$WORK/mg.env" "$ATTEMPT/settings.json" "$TARGET:mg/attempts/$ATTEMPT_ID/"
REMOTE_READY=1
remote "ATTEMPT_ID=$ATTEMPT_ID bash ~/mg/live/remote.sh preflight" | python3 "$HERE/mg.py" redact --host "$HOST_ADDR" | tee "$ATTEMPT/preflight.txt"
grep -q PREFLIGHT_OK "$ATTEMPT/preflight.txt"
collect
if [ "$SKIP_SETUP" = 0 ]; then
  remote 'rm -rf ~/mg/src ~/mg/src-old && mkdir -p ~/mg/src ~/mg/src-old'
  git -C "$REPO" archive --format=tar HEAD | remote 'tar -x -C ~/mg/src'
  git -C "$REPO" archive --format=tar "$OLD_TAG" | remote 'tar -x -C ~/mg/src-old'
fi
# The remote process is detached from SSH, not from this runner's responsibility.
# Setup output is copied on every poll, before any row has completed.
remote "cd ~/mg && ATTEMPT_ID=$ATTEMPT_ID setsid nohup timeout --kill-after=10 $MAX_SECONDS bash live/remote.sh all >attempts/$ATTEMPT_ID/raw/all.log 2>&1 </dev/null &"
start=$SECONDS
last_contact=$SECONDS
REASON=running
while [ $((SECONDS - start)) -lt "$MAX_SECONDS" ]; do
  if collect; then
    last_contact=$SECONDS
    if [ -f "$ATTEMPT/host-completion.json" ]; then
      HOST_STATUS=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["exit_code"])' "$ATTEMPT/host-completion.json")
      REASON=host_finished
      exit "$HOST_STATUS"
    fi
    alive=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("runner_alive"))' "$ATTEMPT/snapshot.json")
    if [ "$alive" = False ] || { [ "$alive" = None ] && [ $((SECONDS - start)) -ge "$DISCONNECT_SECONDS" ]; }; then
      REASON=remote_runner_missing
      exit 1
    fi
  elif [ $((SECONDS - last_contact)) -ge "$DISCONNECT_SECONDS" ]; then
    REASON=connection_lost
    exit 1
  fi
  sleep "$POLL_SECONDS"
done
REASON=deadline
exit 1
