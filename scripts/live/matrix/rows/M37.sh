# shellcheck shell=bash
# M37 (T33; W13): M36 for vLLM on host-b:
#   run_row.sh M37 --tag v17-4 -- v17-4
# shellcheck source=scripts/live/matrix/rows/M36.sh
. "$MATRIX_DIR/rows/M36.sh"

row_main() { kill_row "${1:-v17-4}"; }
