# shellcheck shell=bash
# M53D (T20, W6; 2026-09-24): `delete deployment --stop` on a deployment whose
# launch failed (bogus engine argument) is accepted and removes it; nothing is
# left on the host or in the ledger (the deleted id's tombstone excepted).
#   run_row.sh M53D --tag v17-4 -- v17-4
row_main() {
  local b=${1:-v17-4} host rc=0 db depid
  host=$(fixture_host "$b"); db=$b-bad
  step before host_idle "$host" || return 1
  step variant-bad variant "$b" bad --engine-config-json '{"accept_extra_args": true, "extra_args": ["--moe-backend", "bogus-m53"]}' || return 1
  FIXTURE_VARIANT=bad step deploy-bad deploy "$b" --activate --wait
  step failed-bad wait_state "$db" failed 300 || rc=1
  step status-bad status_dep "$db"
  depid=$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("deployment",d)["id"])' "$EVID/05-status-bad.out")
  step evidence-bad evidence "$db" failed
  step delete-stop-bad timed delete-stop cli delete deployment "$db" --stop --output json || rc=1
  step gone-bad refused status_dep "$db" || rc=1
  sleep 3
  step residue residue_check "$depid" || rc=1
  step clean host_idle "$host" || rc=1
  return "$rc"
}
