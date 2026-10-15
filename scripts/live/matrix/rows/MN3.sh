# shellcheck shell=bash
# MN3 (design section 17.2, ADR 0028, ADR 0023; T34): TensorFold 0.6.5 brings up
# Nemotron 3.5 Lightning 30B-A3B MLX 4-bit as a two-rank group, head on host A,
# worker on host B:
#   run_row.sh MN3
# The steps and expectations are MN1's (rows/MN1.sh). TensorFold has no deep park,
# so the fixture (tg-n35) is restart_only and states its memory in `resources`.
#
# Setup beyond MN1's: TENSORFOLD_VENV_DIR in hosts.local.env (roles.sh then adds the
# `tensorfold` profile to both host documents), and the checkpoint
# GROUP_MODEL_TENSORFOLD under the model store on both hosts with the same files.
# The preflight fails the row before it deploys anything when either host lacks
# the checkpoint; copying it there is an operator step, not the row's.
# shellcheck source=scripts/live/matrix/rows/MN1.sh
. "$MATRIX_DIR/rows/MN1.sh"

row_main() {
  [ -n "$TENSORFOLD_VENV" ] || { echo "PRECONDITION FAILED TENSORFOLD_VENV_DIR is not set in $HOSTS_ENV"; return 1; }
  bringup_row tensorfold
}
