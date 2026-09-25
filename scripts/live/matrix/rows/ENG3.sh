# shellcheck shell=bash
# ENG3 (ADR 0018 §4): removing a published profile is refused while a
# deployment uses it, then --drain stops it through the ordinary path and
# removes the profile only on stop evidence:
#   run_row.sh ENG3 --tag 92 -- host-a v92-4
#
# Expected:
#   a  engine remove vllm while the deployment is Ready exits 20 naming it;
#      the deployment stays Ready and answers.
#   b  engine remove vllm --drain exits 0; the deployment is stopped with
#      verified cleanup (accounting released only on gone evidence).
#   c  list engines no longer shows vllm on the host; a start of the
#      deployment is refused (no eligible host carries the profile).
#   d  engine add of the vLLM environment again publishes it.
. "$MATRIX_DIR/rows/ENG1.sh"

row_main() {
  local host vfix=$2 dep rc=0
  host=$1; dep=$vfix
  step before host_idle "$host" || return 1
  step bare-systemd eng_bare_systemd "$host" || return 1
  step add-vllm eng_add "$host" "$(vllm_venv "$host")/bin/vllm" || return 1
  step deploy deploy "$vfix" --activate --wait || return 1
  step owned keep_owned "$dep" ready
  step remove-in-use refused_with profile_in_use eng_remote "$host" remove vllm || rc=1
  step still-ready wait_state "$dep" ready 30 || rc=1
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step remove-drain timed remove-drain eng_remote "$host" remove vllm --drain || rc=1
  step stopped wait_state "$dep" stopped 900 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step gone refused eng_listed "$host" vllm || rc=1
  step start-refused refused start_dep "$dep" --wait || rc=1
  step readd eng_add "$host" "$(vllm_venv "$host")/bin/vllm" || rc=1
  step delete delete_dep "$dep" || rc=1
  step restore eng_restore "$host" || rc=1
  return "$rc"
}
