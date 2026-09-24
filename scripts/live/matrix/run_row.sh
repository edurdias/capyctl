#!/usr/bin/env bash
# Run one matrix row script and keep its evidence (plan unit W2).
#
#   run_row.sh <ROW> [--tag TAG] [--dry-run] [--no-e0] [-- row args...]
#
# Sources rows/<ROW>.sh (which defines row_main), takes E0 snapshots before and
# after, and writes everything under target/live/matrix/<ROW>[-TAG]/:
# commands.log, timeline.txt, numbered step output, requests.jsonl, i1.jsonl,
# load-*.jsonl, e0 snapshots and result.json. An existing evidence directory is
# archived, never deleted. The result records the exit status only; whether a
# row passed is judged from the evidence against the matrix, and CPU or fake
# runs are never evidence.
# shellcheck source=scripts/live/matrix/lib.sh
. "$(dirname "$0")/lib.sh"

ROW=${1:-}; shift || true
[ -n "$ROW" ] || { sed -n '2,12p' "$0"; exit 2; }
TAG="" E0=1
while [ $# -gt 0 ]; do
  case $1 in
    --tag) TAG=$2; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --no-e0) E0=0; shift ;;
    --) shift; break ;;
    *) break ;;
  esac
done
# SCRATCH_ROWS=<dir> tries <dir>/<ROW>.sh first (a row being revised outside the
# tree), falling back to rows/<ROW>.sh.
ROWFILE=${SCRATCH_ROWS:-$MATRIX_DIR/rows}/$ROW.sh
[ -f "$ROWFILE" ] || ROWFILE=$MATRIX_DIR/rows/$ROW.sh
[ -f "$ROWFILE" ] || die "no row script $ROWFILE"

NAME=$ROW${TAG:+-$TAG}
EVID=$LIVE/$NAME
if dry; then
  EVID=$LIVE/dry-run/$NAME
fi
if [ -d "$EVID" ] && ! dry; then
  mv "$EVID" "$EVID.prev-$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$EVID"
CMDLOG=$EVID/commands.log
export EVID CMDLOG DRY_RUN

load_run
# shellcheck source=scripts/live/matrix/rowlib.sh
. "$MATRIX_DIR/rowlib.sh"
# shellcheck disable=SC1090
. "$ROWFILE"
declare -F row_main >/dev/null || die "$ROWFILE defines no row_main"
load_api_key

STARTED=$(now)
echo "row $NAME run $RUN started $STARTED dry_run=$DRY_RUN" | tee "$EVID/timeline.txt"
[ "$E0" = 1 ] && "$MATRIX_DIR/e0.sh" snap before "$EVID"
RC=0
row_main "$@" || RC=$?
[ "$E0" = 1 ] && "$MATRIX_DIR/e0.sh" snap after "$EVID"
FINISHED=$(now)
python3 - "$EVID/result.json" "$NAME" "$RUN" "${SNAPSHOT_DIGEST:-unknown}" "$STARTED" "$FINISHED" "$RC" "$DRY_RUN" <<'PY'
import json, sys
path, name, run, snap, started, finished, rc, dry = sys.argv[1:]
json.dump({"row": name, "run": run, "snapshot_tree": snap, "started": started, "finished": finished,
           "exit_status": int(rc), "dry_run": dry == "1",
           "note": "exit status only; judge the row from the evidence against the matrix"},
          open(path, "w"), indent=1)
PY
echo "row $NAME finished rc=$RC evidence $EVID"
exit "$RC"
