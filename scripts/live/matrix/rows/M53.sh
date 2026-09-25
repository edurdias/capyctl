# shellcheck shell=bash
# M53 (T20, T30): tight host; incumbent Ready; a request for a switch target
# whose recipe fails to initialize (an argument the engine refuses): the switch
# fails closed, the incumbent is not silently restarted, accounting is retained
# honestly. Then (2026-09-24 recheck) the failed target takes a plain `stop
# deployment` (accepted and recorded stopped). An explicit start of it while the
# incumbent runs is refused `startup_requires_empty_host` (its unmeasured first
# start needs an empty host; `--evict` would release the incumbent), and
# `delete deployment --stop` removes it with nothing left in the ledger for its
# id; the incumbent stops with verified cleanup.
#   run_row.sh M53 -- sb-4 vb-30   (the unmeasured q30 first start empties the host)
row_main() {
  local a=${1:-sb-4} b=${2:-vb-30} host rc=0 db dbid=
  host=$(fixture_host "$a"); db=$b-bad
  step before host_idle "$host" || return 1
  step variant-bad variant "$b" bad --engine-config-json '{"accept_extra_args": true, "extra_args": ["--moe-backend", "bogus-m53"]}' || return 1
  step deploy-a deploy "$a" --activate --wait || return 1
  step owned-a keep_owned "$a" ready
  FIXTURE_VARIANT=bad step deploy-bad deploy "$b" || { echo "bad variant refused at deploy"; }
  step request-bad infer "$db" "What is 17+25? Answer with only the number." --max-tokens 16 --timeout 1200
  step failed-bad wait_state "$db" failed 300 || true
  step status-a status_dep "$a"
  step status-bad status_dep "$db"
  step owned-a2 keep_owned "$a" after
  step switch-log bash -c "grep -aE 'switch' '$LRD/server.log' | grep -viE 'key|secret|bearer|token' | tail -n 8"
  step evidence-bad-failed evidence "$db" failed
  step stop-bad timed stop-bad stop_dep "$db" || rc=1
  step stopped-bad wait_state "$db" stopped 120 || rc=1
  step status-bad-stopped status_dep "$db"
  step evidence-bad-stopped evidence "$db" stopped
  step acct-bad-stopped accounting "$db"
  dry || dbid=$(accounting "$db" | python3 -c 'import json,sys; print(json.load(sys.stdin)["deployment_id"])')
  step infer-a infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024
  step owned-a3 keep_owned "$a" end
  step start-bad refused_with startup_requires_empty_host start_dep "$db" || rc=1
  step status-bad2 status_dep "$db"
  step delete-stop-bad timed delete-stop cli delete deployment "$db" --stop --format json || rc=1
  step gone-bad refused status_dep "$db" || rc=1
  step stop-a stop_dep "$a"
  step stopped-a wait_state "$a" stopped 300 || rc=1
  sleep 3
  step clean host_idle "$host" || rc=1
  step residue-bad residue_check "$dbid" || rc=1
  step delete-a delete_dep "$a" || rc=1
  return "$rc"
}
