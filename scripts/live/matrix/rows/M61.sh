# shellcheck shell=bash
# M61 (T22; G13, G14): mixed-engine replicas of route `qwen3-4b`: va-4 (vLLM)
# and sb-4 (SGLang), same checkpoint, both Ready; concurrent load is served by
# both; I1 on each engine against its own golden; balance as M56.
#   run_row.sh M61 -- va-4 sb-4
. "$MATRIX_DIR/rows/M54.sh"
row_main() {
  local a=${1:-va-4} b=${2:-sb-4} rc=0 d
  REP_ROUTE=qwen3-4b
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant-a variant "$a" mx --route "$REP_ROUTE" || return 1
  step variant-b variant "$b" mx --route "$REP_ROUTE" || return 1
  FIXTURE_VARIANT=mx step deploy-a deploy "$a" --activate --wait || return 1
  FIXTURE_VARIANT=mx step deploy-b deploy "$b" --activate --wait || rc=1
  for d in "$a-mx" "$b-mx"; do step "owned-$d" keep_owned "$d" ready; done
  step models cli list deployments --output json
  sel_mark
  step load load --route "$REP_ROUTE" --nonstream 48 --stream 16 --concurrency 16 --max-tokens 64 || rc=1
  step selections sel_count m61 --max-share 0.7 || rc=1
  for i in 1 2 3 4; do step "i1-$i" i1 "$REP_ROUTE" "$a" "$a-mx"; done
  for d in "$a-mx" "$b-mx"; do step "stop-$d" stop_dep "$d" || rc=1; done
  for d in "$a-mx" "$b-mx"; do step "stopped-$d" wait_state "$d" stopped 600 || rc=1; done
  sleep 3
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  for d in "$a-mx" "$b-mx"; do step "delete-$d" delete_dep "$d" || rc=1; done
  return "$rc"
}
