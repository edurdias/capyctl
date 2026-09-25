# shellcheck shell=bash
# M29 (T16, T21): vLLM deep park (sleep level 2) then wake, reload weights, KV wake,
# prefix reset and a fresh probe, on the same processes:
#   run_row.sh M29 --tag va-14 -- va-14
# The steps and checks are M28's (rows/M28.sh); the vLLM guard policy is recorded
# by the status development_controls mark in the E0 snapshots.
# shellcheck source=scripts/live/matrix/rows/M28.sh
. "$MATRIX_DIR/rows/M28.sh"

row_main() { park_wake_row "${1:?fixture, e.g. va-14}"; }
