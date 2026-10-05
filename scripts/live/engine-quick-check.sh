#!/usr/bin/env bash
# Per-release quick check of an engine on one GB10 host: new version against old,
# through CapyCTL, with a short interleaved benchmark (capyctl-bench) and a verdict.
#
# usage: scripts/live/engine-quick-check.sh --engine tensorfold|vllm|sglang --new V --old V
#          [--host a|b] [--model qwen38-27b|nemotron|qwen3-4b] [options]
#
#   --rounds N       measured rounds per concurrency level and pass (5)
#   --passes N       interleaved passes per version (2)
#   --runs N         requests per context point (3)
#   --concurrency L  stream counts (preset, usually 1,4,8)
#   --sweep L        context sizes, "" for none (preset, usually 2k,32k,128k)
#   --threshold P    mark points more than P% worse on the new version (5)
#   --port N         port of the check's own standalone role on the host (18443)
#   --out DIR        output directory
#                    ($CAPYCTL_QC_OUT_ROOT or target/quick-check, then <date>-<engine>-<old>-vs-<new>[-<model>])
#   --skip-venv      do not create missing venvs (fail instead)
#   --skip-build     use the CapyCTL binary already built on the host
#   --skip-cold      no first-start (cold) measurement
#   --prune          remove the venvs the retention rule lists (otherwise only printed)
#   --bench DIR      capyctl-bench directory (../capyctl-recipes/tools/capyctl-bench)
#   --recipes DIR    capyctl-recipes checkout, scanned for the versions to keep (../capyctl-recipes)
#
# Hosts come from scripts/live/matrix/hosts.local.env (lib.sh); output never
# names a host. See scripts/live/README.md.
set -euo pipefail

LIVE_DIR=$(cd "$(dirname "$0")" && pwd)
QC=$LIVE_DIR/quick-check
REPO=$(cd "$LIVE_DIR/../.." && pwd)
usage() { sed -n '2,27p' "$0"; exit 2; }

ENGINE="" NEW="" OLD="" HOST_CODE=a MODEL="" ROUNDS=5 PASSES=2 RUNS=3 CONC="" SWEEP="-"
THRESHOLD=5 PORT=18443 OUT="" SKIP_VENV=0 SKIP_BUILD=0 SKIP_COLD=0 PRUNE=0
BENCH=${CAPYCTL_BENCH_DIR:-$REPO/../capyctl-recipes/tools/capyctl-bench}
RECIPES=${CAPYCTL_RECIPES_DIR:-$REPO/../capyctl-recipes}
ARGS=("$@")
while [ $# -gt 0 ]; do
  case $1 in
    --engine) ENGINE=$2; shift ;;
    --new) NEW=$2; shift ;;
    --old) OLD=$2; shift ;;
    --host) HOST_CODE=$2; shift ;;
    --model) MODEL=$2; shift ;;
    --rounds) ROUNDS=$2; shift ;;
    --passes) PASSES=$2; shift ;;
    --runs) RUNS=$2; shift ;;
    --concurrency) CONC=$2; shift ;;
    --sweep) SWEEP=$2; shift ;;
    --threshold) THRESHOLD=$2; shift ;;
    --port) PORT=$2; shift ;;
    --out) OUT=$2; shift ;;
    --bench) BENCH=$2; shift ;;
    --recipes) RECIPES=$2; shift ;;
    --skip-venv) SKIP_VENV=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --skip-cold) SKIP_COLD=1 ;;
    --prune) PRUNE=1 ;;
    -h|--help) usage ;;
    *) echo "unknown option $1" >&2; usage ;;
  esac
  shift
done
if [ -z "$ENGINE" ] || [ -z "$NEW" ] || [ -z "$OLD" ]; then usage; fi
[ "$NEW" != "$OLD" ] || { echo "--new and --old are the same version" >&2; exit 2; }
for v in "$NEW" "$OLD"; do [[ $v =~ ^[0-9]+(\.[0-9]+)+([a-z0-9.]*)$ ]] || { echo "bad version $v" >&2; exit 2; }; done
[ -n "$MODEL" ] || MODEL=qwen38-27b
[ -f "$BENCH/capyctl_bench.py" ] || { echo "capyctl-bench not found in $BENCH (--bench)" >&2; exit 2; }

# Preset defaults (one source: remote.sh).
PRESET=$(ENGINE=$ENGINE MODEL=$MODEL bash "$QC/remote.sh" preset) || exit 2
eval "$PRESET"
[ -n "$CONC" ] || CONC=$CONC_DEFAULT
[ "$SWEEP" != - ] || SWEEP=$SWEEP_DEFAULT
case $ENGINE in tensorfold) LABEL=TensorFold ;; vllm) LABEL=vLLM ;; sglang) LABEL=SGLang ;; esac

# Hosts, sync and remote helpers of the matrix harness.
# shellcheck source-path=SCRIPTDIR source=matrix/lib.sh
. "$LIVE_DIR/matrix/lib.sh"
HOST=$(resolve_host "$HOST_CODE")
HOST_ADDR=$(host_ip "$HOST")
# Every line this script and its children print goes through redact: no host
# name, address or home directory reaches the terminal or the output files.
redact() { sed -u -e "s#$REMOTE_HOME#~#g" -e "s#$HOME#~#g" -e "s#$HOST_ADDR#host-$HOST_CODE-addr#g" -e "s#$HOST#host-$HOST_CODE#g"; }
exec > >(redact) 2>&1

export CAPYCTL_REMOTE_TREE=${CAPYCTL_REMOTE_TREE:-$REMOTE_HOME/capyctl-quick-check}
DATE=$(date +%F)
RUN_ID=$DATE-$ENGINE-$OLD-vs-$NEW
[ "$MODEL" = qwen38-27b ] || RUN_ID=$RUN_ID-$MODEL
OUT=${OUT:-${CAPYCTL_QC_OUT_ROOT:-$REPO/target/quick-check}/$RUN_ID}
WORK=$REMOTE_HOME/capyctl-quick-check-runs/$RUN_ID
mkdir -p "$OUT"
step() { printf '\n== %s\n' "$*"; }
step "quick check $LABEL $OLD vs $NEW, $TITLE, host $HOST_CODE -> $OUT"

# 1. CapyCTL from this checkout, built on the host.
if [ "$SKIP_BUILD" = 1 ]; then
  rsh "$HOST" "test -x $CAPYCTL_REMOTE_TREE/target/release/capyctl" || die "no binary on host $HOST_CODE; drop --skip-build"
  COMMIT=$(git -C "$REPO" rev-parse HEAD) SNAP=none
else
  step "build CapyCTL $(git -C "$REPO" rev-parse --short HEAD) on host $HOST_CODE"
  "$LIVE_DIR/matrix/sync.sh" snapshot
  "$LIVE_DIR/matrix/sync.sh" push "$HOST"
  "$LIVE_DIR/matrix/sync.sh" build "$HOST"
  COMMIT=$(cat "$SNAPSHOT/commit") SNAP=$(cat "$SNAPSHOT/tree.sha256")
fi

# 2. Helpers, capyctl-bench and settings to the host's work directory.
step "prepare the host work directory"
rsh "$HOST" "rm -rf $WORK && mkdir -m 700 -p $WORK"
x rsync -rlpz --chmod=Dgo-w,Fgo-w "$QC/remote.sh" "$QC/stream_check.py" "$HOST:$WORK/"
x rsync -rlpz --chmod=Dgo-w,Fgo-w --exclude __pycache__ --exclude tests "$BENCH/" "$HOST:$WORK/capyctl-bench/"
ENVFILE=$(mktemp)
{
  printf '%s=%q\n' ENGINE "$ENGINE" MODEL "$MODEL" OLD "$OLD" NEW "$NEW" PORT "$PORT" CONC "$CONC" \
    SWEEP "$SWEEP" ROUNDS "$ROUNDS" RUNS "$RUNS" PASSES "$PASSES" SKIP_VENV "$SKIP_VENV" \
    SKIP_COLD "$SKIP_COLD" CAPYCTL_COMMIT "$COMMIT" WORK "$WORK" \
    CAPYCTL_BIN "$CAPYCTL_REMOTE_TREE/target/release/capyctl"
} >"$ENVFILE"
x rsync -z "$ENVFILE" "$HOST:$WORK/qc.env"
rm -f "$ENVFILE"

# 3. The phases run detached on the host (setsid), so a dropped connection does
# not stop them. Some SSH servers keep the launching session open until every
# process it started has exited, so the launch runs in the background here and
# this side follows the progress log until the done marker appears.
step "lifecycle and benchmark on host $HOST_CODE (progress below)"
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
step "host phases ended with status $rc"
wait "$LAUNCH" 2>/dev/null || true

# 4. Results back; the host's work directory removed.
mkdir -p "$OUT/raw"
x rsync -rlz "$HOST:$WORK/res/" "$OUT/raw/"
x rsync -rlz "$HOST:$WORK/deployments" "$HOST:$WORK/run.log" "$OUT/raw/"
find "$OUT/raw" -type f \( -name '*.log' -o -name '*.txt' -o -name '*.tsv' -o -name '*.yaml' -o -name '*.json' \) \
  -exec sed -i -e "s#$REMOTE_HOME#~#g" -e "s#$HOST_ADDR#host-$HOST_CODE-addr#g" -e "s#$HOST#host-$HOST_CODE#g" {} +
rsh "$HOST" "bash $WORK/remote.sh purge"

# 5. Merge, report, verdict.
step "merge passes and compare outputs"
mkdir -p "$OUT/results"
python3 "$QC/merge_passes.py" "$BENCH" "$OUT/raw" "$OUT/results" "$ENGINE" "$OLD" "$NEW" | tee "$OUT/results/merge.txt" || true
TITLE_FULL="$TITLE: $LABEL $OLD vs $NEW"
if [ -f "$OUT/results/$ENGINE-$OLD.json" ] && [ -f "$OUT/results/$ENGINE-$NEW.json" ]; then
  step "report"
  uv run --quiet --with matplotlib python3 "$BENCH/capyctl_bench.py" report \
    "$OUT/results/$ENGINE-$OLD.json" "$OUT/results/$ENGINE-$NEW.json" \
    --out "$OUT/report" --summary --title "$TITLE_FULL" || echo "report failed"
fi
BENCH_COMMIT=$(git -C "$BENCH" rev-parse HEAD 2>/dev/null || echo unknown)
CMDLINE="scripts/live/engine-quick-check.sh $(printf '%q ' "${ARGS[@]}")"
QC_SETTINGS=$(printf '%s\n' engine "$ENGINE" engine_label "$LABEL" old "$OLD" new "$NEW" model_title "$TITLE" \
  model_ref "$MODEL_REF" drafter_ref "$DRAFTER_REF" threshold "$THRESHOLD" rounds "$ROUNDS" passes "$PASSES" \
  runs "$RUNS" concurrency "$CONC" sweep "$SWEEP" command "${CMDLINE% }" capyctl_commit "$COMMIT" \
  snapshot "$SNAP" bench_commit "$BENCH_COMMIT") \
  python3 -c 'import json, os, sys
v = os.environ["QC_SETTINGS"].split("\n")
json.dump(dict(zip(v[0::2], v[1::2])), open(sys.argv[1], "w"), indent=1)' "$OUT/settings.json"
step "verdict"
python3 "$QC/verdict.py" "$OUT" "$OUT/settings.json"

# 6. Venv retention: the last two versions per engine, the versions the recipes
# name and this run's two are kept; the rest are listed, and removed with --prune.
step "venv retention on host $HOST_CODE"
KEEP=$(grep -rhoiE "${ENGINE}[ -]v?[0-9]+\.[0-9]+\.[0-9]+" "$RECIPES" --include='*.md' --include='*.yaml' 2>/dev/null |
  grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sort -uV | tr '\n' ' ')
echo "named in recipes: ${KEEP:-none}"
HAVE=$(rsh_out "$HOST" "" "cd $REMOTE_HOME && ls -d $ENGINE-*-venv 2>/dev/null | sed -n 's/^$ENGINE-\(.*\)-venv\$/\1/p' | sort -V; true")
LAST2=$(printf '%s\n' "$HAVE" | grep . | tail -2 | tr '\n' ' ')
CANDIDATES=()
for v in $HAVE; do
  case " $LAST2 $KEEP $OLD $NEW " in *" $v "*) ;; *) CANDIDATES+=("$ENGINE-$v-venv") ;; esac
done
echo "on host: $(echo "$HAVE" | tr '\n' ' ')"
if [ ${#CANDIDATES[@]} -eq 0 ]; then echo "nothing to remove"
elif [ "$PRUNE" = 1 ]; then
  for d in "${CANDIDATES[@]}"; do rsh "$HOST" "rm -rf -- $REMOTE_HOME/$d" && echo "removed ~/$d"; done
else
  printf 'removal candidates (rerun with --prune to remove): %s\n' "${CANDIDATES[*]/#/~/}"
fi

step "done: $OUT (verdict.md, versions.md, report/)"
[ "$rc" = 0 ]
