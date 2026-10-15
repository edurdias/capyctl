# shellcheck shell=bash
# MN6 (design section 17.2, ADR 0028 §11; T20, T33, T34): SIGKILL of the head's
# engine (rank 0, host A, the one serving the API) while the group is Ready, per engine:
#   run_row.sh MN6 --tag vllm -- vllm
#   run_row.sh MN6 --tag sglang -- sglang
#   run_row.sh MN6 --tag tensorfold -- tensorfold
# The steps and expectations are MN5's (rows/MN5.sh) with rank 0 failed: the whole
# group stops, each host releases its own member on its own evidence, the
# rendezvous port is released, and a start brings the group back.
# shellcheck source=scripts/live/matrix/rows/MN5.sh
. "$MATRIX_DIR/rows/MN5.sh"

row_main() { member_kill_row "${1:?engine: vllm, sglang or tensorfold}" a; }
