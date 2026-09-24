# shellcheck shell=bash
# M31 (T16, T22, T23): cross-engine warm switching, three cycles (rows/M27.sh):
#   run_row.sh M31 --tag v14-s30 -- v92-14 s92-30 3 deep
# q4 pair on the tight host, two cycles (2026-09-24 smoke; 44 GiB requests make
# the two small models mutually exclusive):
#   SWITCH_MEMORY_JSON='{"memory": {"request": "47244640256B", "kv_cache": "4294967296B"}}' \
#     run_row.sh M31 --tag v4-s4 -- v92-4 s92-4 2 deep
# shellcheck source=scripts/live/matrix/rows/M27.sh
. "$MATRIX_DIR/rows/M27.sh"

row_main() { switch_row "${1:-v92-14}" "${2:-s92-30}" "${3:-3}" "${4:-deep}"; }
