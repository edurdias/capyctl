# shellcheck shell=bash
# Solo first start (owner decision 2026-09-23, store schema v28) and `start
# --evict`, on one host under the normal budget:
#   run_row.sh SOLO --tag sb-30 -- sb-30 vb-4
#
# Expected:
#   a  the small fixture (vb-4) is Ready on the host.
#   b  a new, unmeasured q30 deployment (its placeholder startup estimate is above
#      the managed limit) is refused a plain start with `startup_requires_empty_host`
#      while vb-4 holds a charge; nothing is spawned for it.
#   c  `start deployment <q30> --evict` reports vb-4 as its victim, vb-4 is
#      released with verified cleanup, q30 starts alone reserving the whole managed
#      limit (status startup provenance `whole_host`), reaches Ready and answers.
#   d  after Ready its startup peak is recorded (status startup.measured); stop,
#      then a second start reserves the measured peak (provenance `measured`),
#      Ready again, then stop with verified cleanup; both deployments deleted.

startup_of() { status_dep "$1" | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
print(json.dumps({"deployment": d.get("startup"), "instances": [i.get("startup") for i in d.get("instances", [])], "state": d.get("observed_state")}))'; }

row_main() {
  local big=${1:-sb-30} small=${2:-vb-4} host dep rc=0
  host=$(fixture_host "$big")
  dep=$big-solo
  step before host_idle "$host" || return 1
  step deploy-small deploy "$small" --activate --wait || return 1
  step owned-small keep_owned "$small" ready
  step variant variant "$big" solo || return 1
  FIXTURE_VARIANT=solo step deploy-big deploy "$big" || return 1
  step startup-planned startup_of "$dep"
  step plain-start-refused refused start_dep "$dep" || rc=1
  step small-still-ready wait_state "$small" ready 30 || rc=1
  step evict timed evict cli start deployment "$dep" --evict --format json || rc=1
  step small-released wait_state "$small" stopped 600 || rc=1
  step startup-solo startup_of "$dep"
  step ledger-solo python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"
  step big-ready wait_state "$dep" ready 1800 || { rc=1; step errors engine_errors "$host"; }
  step small-cleanup cleanup_check_partial "$small" "$host" "$EVID/owned-$small-ready.json" || rc=1
  step owned-big keep_owned "$dep" gen1
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  sleep 5
  step startup-measured startup_of "$dep"
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup-gen1 cleanup_check "$dep" "$host" "$EVID/owned-$dep-gen1.json" "$EVID/accounting-$dep-gen1.json" gen1 || rc=1
  step restart start_dep "$dep" || rc=1
  step startup-second startup_of "$dep"
  step ledger-second python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"
  step ready-2 wait_state "$dep" ready 1800 || rc=1
  step owned-big2 keep_owned "$dep" gen2
  step infer-2 infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stop-2 stop_dep "$dep" || rc=1
  step stopped-2 wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup-gen2 cleanup_check "$dep" "$host" "$EVID/owned-$dep-gen2.json" "$EVID/accounting-$dep-gen2.json" gen2 || rc=1
  step delete-big delete_dep "$dep" || rc=1
  step delete-small delete_dep "$small" || rc=1
  return "$rc"
}
