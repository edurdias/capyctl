# shellcheck shell=bash
# M55 (T14; G13): a replica of route `qwen3-14b` backed by q4's checkpoint is
# rejected at admission (no silent model substitution, SPEC 10):
#   run_row.sh M55 -- va-14 vb-4
row_main() {
  local a=${1:-va-14} b=${2:-vb-4} rc=0
  step variant-a variant "$a" r55 --route qwen3-14b || return 1
  step variant-b variant "$b" r55 --route qwen3-14b || return 1
  FIXTURE_VARIANT=r55 step deploy-a deploy "$a" || return 1
  FIXTURE_VARIANT=r55 step deploy-b-refused refused deploy "$b" || rc=1
  step list cli list deployments --format json
  step delete-a delete_dep "$a-r55" || rc=1
  cli status deployment "$b-r55" --format json >/dev/null 2>&1 && { step delete-b delete_dep "$b-r55"; rc=1; }
  return "$rc"
}
