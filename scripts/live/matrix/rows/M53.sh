# shellcheck shell=bash
# M53 (T20, T30): tight host; incumbent Ready; a request for a switch target
# whose recipe fails to initialize (an argument the engine refuses): the switch
# fails closed, the incumbent is not silently restarted, accounting is retained
# honestly. Then (2026-09-24 recheck) the failed target takes a plain `stop
# deployment` (accepted and recorded stopped), is started again, fails again,
# and `delete deployment --stop` removes it; the incumbent stops with verified
# cleanup.
#   run_row.sh M53 -- s17-4 v17-30   (the unmeasured q30 first start empties the host)
row_main() {
  local a=${1:-s17-4} b=${2:-v17-30} host rc=0 db
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
  step infer-a infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024
  step owned-a3 keep_owned "$a" end
  step start-bad start_dep "$db"
  step failed-bad2 wait_state "$db" failed 300
  step status-bad2 status_dep "$db"
  step delete-stop-bad timed delete-stop cli delete deployment "$db" --stop --output json || rc=1
  step gone-bad refused status_dep "$db" || rc=1
  step stop-a stop_dep "$a"
  step stopped-a wait_state "$a" stopped 300 || rc=1
  sleep 3
  step clean host_idle "$host" || rc=1
  step acct-bad-end accounting "$db"
  step delete-a delete_dep "$a" || rc=1
  return "$rc"
}
