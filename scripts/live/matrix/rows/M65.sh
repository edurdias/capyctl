# shellcheck shell=bash
# M65 (T17; G10, G13): remove one replica of `qwen3-14b` under load by a
# count-only revision (2 -> 1): the route keeps serving, no failed request.
#   run_row.sh M65 -- va-14
. "$MATRIX_DIR/rows/M54.sh"
row_main() {
  local fix=${1:-va-14} dep rc=0
  dep=$fix-r65
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant variant "$fix" r65 --route "$REP_ROUTE" \
    --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  FIXTURE_VARIANT=r65 step deploy deploy "$fix" --activate || return 1
  step ready both_ready "$dep" 1200 || return 1
  # The count-only revision is the same document with instances 1 (the
  # variant generator names its output after the tag, so the name is copied).
  step variant-1 python3 - "$(FIXTURE_VARIANT=r65 fixture_file "$fix")" "$(FIXTURE_VARIANT=r65one fixture_file "$fix")" <<'PY' || return 1
import json, sys
d = json.load(open(sys.argv[1])); d["instances"] = 1
json.dump(d, open(sys.argv[2], "w"), indent=1); print(d["name"], d["routes"], d["instances"])
PY
  sel_mark
  bg_load r65 --stream 16 --nonstream 600 --concurrency 12 --max-tokens 256
  sleep 10
  echo "count_down $(now_ms)" >>"$EVID/marks.txt"
  FIXTURE_VARIANT=r65one step revise deploy "$fix" --revision 1 || rc=1
  step one-ready n_ready "$dep" 1 600 || rc=1
  wait "$BG_LOAD" || rc=1
  step load-summary cat "$EVID/load-r65.summary.json"
  step selections sel_count r65
  step status status_dep "$dep"
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
