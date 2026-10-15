# shellcheck shell=bash
# MN2 (design section 17.2, ADR 0028; T34): SGLang 0.5.21 brings up Qwen3-30B-A3B as a
# two-rank tensor-parallel group, head on host A, worker on host B:
#   run_row.sh MN2
# The steps, setup and expectations are MN1's (rows/MN1.sh). A SGLang deep group
# needs the same model path on both hosts (group_model_path_mismatch otherwise);
# the fixture names one path under the shared model store root.
# shellcheck source=scripts/live/matrix/rows/MN1.sh
. "$MATRIX_DIR/rows/MN1.sh"

row_main() { bringup_row sglang; }
