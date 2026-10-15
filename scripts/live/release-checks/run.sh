#!/usr/bin/env bash
# The 0.1.3 release live checks (B1-B17 of the 0.1.3 live backlog) on one GB10
# host, through CapyCTL standalone roles of their own, ordered so that one
# deployment serves several checks. Modelled on scripts/live/engine-quick-check.sh,
# whose matrix helpers it uses.
#
# usage: scripts/live/release-checks/run.sh [options]
#
#   --only L               checks to run, e.g. B1,B4,B7 (all; B2 rides on B1's cycles)
#   --host a|b             lab host (a)
#   --port N               inference port of the run's roles (18643); management N+1,
#                          the B13 HTTP server N+7, the B17 container's role N+100 and N+101;
#                          engines 8340-8359 (container 8360-8379)
#   --max-parks N          B4: parks before giving up on the growth stop (6)
#   --draft-source S       B12 drafter: `hf` (z-lab/Qwen3.8-27B-DFlash2 at its pinned commit,
#                          downloaded by CapyCTL as model.draft; default) or `local`
#                          (~/drafters/Qwen3.8-27B-DFlash2)
#   --download-flash-next  B10: let CapyCTL download Qwen3.8-Flash-Next (about 135 GB, plus
#                          the TensorFold MLX export) into ~/models/sources when no complete
#                          copy is on the host; they stay there afterwards. Without it B10
#                          is SKIP unless a complete copy exists.
#   --container            B17: build an image on the host (ubuntu:24.04 + the binary) and run
#                          a standalone role as its PID 1. Without it B17 is SKIP.
#   --out DIR              local results (<main checkout>/target/live/0.1.3-backlog/<run>)
#   --skip-build           use the CapyCTL binary already built on the host
#   --dry-run              print every step, local and on the host, and contact no host
#
# Hosts come from the matrix harness's hosts.local.env ($CAPYCTL_MATRIX_HOSTS_ENV,
# else the main checkout's scripts/live/matrix/hosts.local.env); output never
# names a host. One live session at a time: the host phases refuse to start
# while any engine runs there.
set -euo pipefail

RC=$(cd "$(dirname "$0")" && pwd)
LIVE_DIR=$(cd "$RC/.." && pwd)
REPO=$(cd "$LIVE_DIR/../.." && pwd)
usage() { sed -n '2,30p' "$0"; exit 2; }

ONLY=all HOST_CODE=a PORT=18643 MAX_PARKS=6 DRAFT_SOURCE=hf DOWNLOAD_FLASH_NEXT=0 CONTAINER=0
OUT="" SKIP_BUILD=0 DRY=0
ARGS=("$@")
while [ $# -gt 0 ]; do
  case $1 in
    --only) ONLY=$2; shift ;;
    --host) HOST_CODE=$2; shift ;;
    --port) PORT=$2; shift ;;
    --max-parks) MAX_PARKS=$2; shift ;;
    --draft-source) DRAFT_SOURCE=$2; shift ;;
    --download-flash-next) DOWNLOAD_FLASH_NEXT=1 ;;
    --container) CONTAINER=1 ;;
    --out) OUT=$2; shift ;;
    --skip-build) SKIP_BUILD=1 ;;
    --dry-run) DRY=1 ;;
    -h|--help) usage ;;
    *) echo "unknown option $1" >&2; usage ;;
  esac
  shift
done
case $DRAFT_SOURCE in hf|local) ;; *) echo "--draft-source is hf or local" >&2; exit 2 ;; esac
[[ $MAX_PARKS =~ ^[1-9][0-9]*$ ]] || { echo "--max-parks takes a positive number" >&2; exit 2; }
[[ $PORT =~ ^[0-9]+$ ]] && [ "$PORT" -ge 1024 ] && [ "$PORT" -le 65000 ] || { echo "--port takes 1024-65000" >&2; exit 2; }
[ "$PORT" != 18443 ] && [ "$PORT" != 18543 ] || { echo "ports 18443 and 18543 are the quick check's and the B18 comparison's; pick another" >&2; exit 2; }
if [ "$ONLY" != all ]; then
  ONLY=${ONLY^^}
  for id in ${ONLY//,/ }; do
    [[ $id =~ ^B([1-9]|1[0-7])$ ]] || { echo "--only takes check IDs B1-B17 separated by commas, not '$id'" >&2; exit 2; }
  done
  # B2 is B1's three cycles on the GB10.
  case ",$ONLY," in *,B2,*) case ",$ONLY," in *,B1,*) ;; *) ONLY="$ONLY,B1" ;; esac ;; esac
fi
MPORT=$((PORT + 1)) HTTP_PORT=$((PORT + 7)) CPORT=$((PORT + 100))
ENGINE_PORTS=8340-8359 C_ENGINE_PORTS=8360-8379

# The main checkout (hosts file, output root), also when this is a worktree.
REPO_MAIN=$(dirname "$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir)")
export CAPYCTL_MATRIX_HOSTS_ENV=${CAPYCTL_MATRIX_HOSTS_ENV:-$REPO_MAIN/scripts/live/matrix/hosts.local.env}
if [ "$DRY" = 1 ] && [ ! -f "$CAPYCTL_MATRIX_HOSTS_ENV" ]; then
  echo "dry run: $CAPYCTL_MATRIX_HOSTS_ENV missing, using hosts.example.env" >&2
  export CAPYCTL_MATRIX_HOSTS_ENV=$LIVE_DIR/matrix/hosts.example.env
fi
export DRY_RUN=$DRY

# Hosts, sync and remote helpers of the matrix harness.
# shellcheck source-path=SCRIPTDIR/../matrix source=lib.sh
. "$LIVE_DIR/matrix/lib.sh"
HOST=$(resolve_host "$HOST_CODE")
HOST_ADDR=$(host_ip "$HOST")
# Every line this script and its children print goes through redact: no host
# name, address or home directory reaches the terminal or the output files.
redact() { sed -u -e "s#$REMOTE_HOME#~#g" -e "s#$HOME#~#g" -e "s#$HOST_ADDR#host-$HOST_CODE-addr#g" -e "s#$HOST#host-$HOST_CODE#g"; }
exec > >(redact) 2>&1

# The backlog's tree for 0.1.3 live work (shared with the B18 comparison; one
# session at a time). The run's work directory has a short path: deep parking's
# observation socket lives under the state directory, and a Unix socket path
# has a 107-byte limit.
export CAPYCTL_REMOTE_TREE=${CAPYCTL_REMOTE_TREE:-$REMOTE_HOME/capyctl-0.1.3-live}
RID=$(date +%m%d%H%M)
RUN_ID=$(date +%F-%H%M)-release-checks
OUT=${OUT:-$REPO_MAIN/target/live/0.1.3-backlog/$RUN_ID}
WORK=$REMOTE_HOME/c13/$RID
step() { printf '\n== %s\n' "$*"; }

step "0.1.3 release checks ($ONLY), host $HOST_CODE -> $OUT"
echo "capyctl $(git -C "$REPO" rev-parse --short HEAD) from $REPO; origin/main is $(git -C "$REPO" rev-parse --short origin/main 2>/dev/null || echo unknown)"
echo "host work directory ~/${WORK#"$REMOTE_HOME"/} (state directories s1-s5 inside), tree ~/${CAPYCTL_REMOTE_TREE#"$REMOTE_HOME"/}"
EST=$(mktemp)
bash "$RC/remote.sh" estimate >"$EST"
total=$([ "$SKIP_BUILD" = 1 ] && echo 0 || echo 20)
echo "estimated minutes per check (GB10; a shared deployment is counted under its first check):"
[ "$SKIP_BUILD" = 1 ] || printf '  %-5s %4s  %s\n' build 20 "snapshot, push, release build on the host"
while IFS=$'\t' read -r id mins note; do
  sel=0
  if [ "$ONLY" = all ]; then sel=1; else case ",$ONLY," in *",$id,"*) sel=1 ;; esac; fi
  [ "$sel" = 1 ] || continue
  case $id in
    B10) [ "$DOWNLOAD_FLASH_NEXT" = 1 ] || { note="SKIP unless a complete Flash-Next copy is on the host; with --download-flash-next about $mins: $note"; mins=1; } ;;
    B17) [ "$CONTAINER" = 1 ] || { note="SKIP; with --container about $mins: $note"; mins=0; } ;;
  esac
  printf '  %-5s %4s  %s\n' "$id" "$mins" "$note"
  total=$((total + mins))
done <"$EST"
echo "  total about $((total + 5)) minutes, cleanup included"
[ "$DRY" = 1 ] || mkdir -p "$OUT"

# 1. CapyCTL from this checkout, built on the host.
if [ "$SKIP_BUILD" = 1 ]; then
  rsh "$HOST" "test -x $CAPYCTL_REMOTE_TREE/target/release/capyctl" || die "no binary on host $HOST_CODE; drop --skip-build"
  COMMIT=$(git -C "$REPO" rev-parse HEAD)
else
  step "build CapyCTL $(git -C "$REPO" rev-parse --short HEAD) on host $HOST_CODE"
  "$LIVE_DIR/matrix/sync.sh" snapshot
  "$LIVE_DIR/matrix/sync.sh" push "$HOST"
  "$LIVE_DIR/matrix/sync.sh" build "$HOST"
  if [ "$DRY" = 1 ]; then COMMIT=$(git -C "$REPO" rev-parse HEAD); else COMMIT=$(cat "$SNAPSHOT/commit"); fi
fi

# 2. Helpers and settings to the host's work directory.
step "prepare the host work directory"
ENVFILE=$(mktemp)
{
  printf '%s=%q\n' PORT "$PORT" MPORT "$MPORT" ENGINE_PORTS "$ENGINE_PORTS" HTTP_PORT "$HTTP_PORT" \
    CPORT "$CPORT" C_ENGINE_PORTS "$C_ENGINE_PORTS" ONLY "$ONLY" DOWNLOAD_FLASH_NEXT "$DOWNLOAD_FLASH_NEXT" \
    CONTAINER "$CONTAINER" DRAFT_SOURCE "$DRAFT_SOURCE" MAX_PARKS "$MAX_PARKS" RID "$RID" \
    CAPYCTL_COMMIT "$COMMIT" CAPYCTL_BIN "$CAPYCTL_REMOTE_TREE/target/release/capyctl"
} >"$ENVFILE"
rsh "$HOST" "rm -rf $WORK && mkdir -m 700 -p $WORK"
x rsync -rlpz --chmod=Dgo-w,Fgo-w "$RC/remote.sh" "$RC/rc.py" "$HOST:$WORK/"
x rsync -z "$ENVFILE" "$HOST:$WORK/rc.env"

# 3. The checks run detached on the host (setsid), so a dropped connection does
# not stop them; this side follows the progress log until the done marker appears.
step "checks on host $HOST_CODE (progress below)"
if [ "$DRY" = 1 ]; then
  log_cmd "$HOST" "cd $WORK && WORK=$WORK setsid nohup bash remote.sh run >run.log 2>&1 </dev/null &"
  PLANDIR=$(mktemp -d)
  cp "$RC/remote.sh" "$RC/rc.py" "$PLANDIR/"
  sed -e "s#^CAPYCTL_BIN=.*#CAPYCTL_BIN=capyctl#" "$ENVFILE" >"$PLANDIR/rc.env"
  WORK=$PLANDIR bash "$PLANDIR/remote.sh" plan | sed -e "s#$PLANDIR#~/c13/$RID#g"
  rm -rf "$PLANDIR"
  rc=0
else
  rsh "$HOST" "cd $WORK && WORK=$WORK setsid nohup bash remote.sh run >run.log 2>&1 </dev/null & echo started" &
  LAUNCH=$!
  seen=0
  while :; do
    sleep 20
    lines=$(rsh_out "$HOST" "" "tail -n +$((seen + 1)) $WORK/res/progress.log 2>/dev/null; true" 2>/dev/null) || continue
    if [ -n "$lines" ]; then printf '%s\n' "$lines"; seen=$((seen + $(printf '%s\n' "$lines" | wc -l))); fi
    rc=$(rsh_out "$HOST" "" "cat $WORK/done 2>/dev/null; true" 2>/dev/null) || continue
    [ -z "$rc" ] || break
  done
  wait "$LAUNCH" 2>/dev/null || true
fi
rm -f "$ENVFILE"
step "host phases ended with status $rc"

# 4. Results back, names and home directories redacted; the work directory
# (state directories, kernel caches, this run's downloads) removed.
step "copy the results back and remove the host's work directory"
x mkdir -p "$OUT/raw"
x rsync -rlz "$HOST:$WORK/res/" "$OUT/raw/"
x rsync -rlz "$HOST:$WORK/dep" "$HOST:$WORK/run.log" "$OUT/raw/"
if [ "$DRY" = 1 ]; then
  log_cmd control-host "redact host names, addresses and the remote home in $OUT/raw"
else
  find "$OUT/raw" -type f \( -name '*.log' -o -name '*.txt' -o -name '*.tsv' -o -name '*.yaml' -o -name '*.json' -o -name '*.jsonl' -o -name '*.out' -o -name '*.err' \) \
    -exec sed -i -e "s#$REMOTE_HOME#~#g" -e "s#$HOST_ADDR#host-$HOST_CODE-addr#g" -e "s#$HOST#host-$HOST_CODE#g" {} +
fi
rsh "$HOST" "bash $WORK/remote.sh purge"

# 5. Verdict.
step "verdict"
CMDLINE="scripts/live/release-checks/run.sh $(printf '%q ' "${ARGS[@]}")"
if [ "$DRY" = 1 ]; then
  log_cmd control-host "write $OUT/settings.json, $OUT/estimates.tsv and $OUT/verdict.md (rc.py verdict raw/results.tsv)"
else
  cp "$EST" "$OUT/estimates.tsv"
  python3 -c 'import json, sys
v = sys.argv[2:]
json.dump(dict(zip(v[0::2], v[1::2])), open(sys.argv[1], "w"), indent=1)' "$OUT/settings.json" \
    date "$(date +%F)" capyctl_commit "$COMMIT" checks "$ONLY" host "host-$HOST_CODE" port "$PORT" \
    max_parks "$MAX_PARKS" draft_source "$DRAFT_SOURCE" download_flash_next "$DOWNLOAD_FLASH_NEXT" \
    container "$CONTAINER" command "${CMDLINE% }"
  python3 "$RC/rc.py" verdict "$OUT/raw/results.tsv" "$OUT/estimates.tsv" "$OUT/verdict.md" --settings "$OUT/settings.json"
fi
rm -f "$EST"
step "done: $OUT (verdict.md, raw/)"
[ "$rc" = 0 ]
