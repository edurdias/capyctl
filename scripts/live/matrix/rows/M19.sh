# shellcheck shell=bash
# M19 + M22 (T22, T26, T16) with co-residence fixtures (owner decision 2026-09-23):
# vLLM and SGLang co-resident on one unified-memory host.
#   run_row.sh M19 --tag a -- va-14 sa-4
#
# Expected: both `-co` variants Ready on the same host at once (two reservations
# within the managed limit, distinct ports, distinct owned process groups);
# overlapping load answered by each route's own model (I1 per route); stopping the
# first leaves the second serving with the same processes; only the stopped
# reservation is released after absence proof (M22); then the second stops clean.

row_main() {
  local a=${1:?first fixture, e.g. va-14} b=${2:?second fixture, e.g. sa-4} host rc=0 da db
  host=$(fixture_host "$a")
  da=$a-co; db=$b-co
  step before host_idle "$host" || return 1
  step mem-before host_mem "$host" before
  step variant-a variant "$a" co --co || return 1
  step variant-b variant "$b" co --co || return 1
  FIXTURE_VARIANT=co step deploy-a deploy "$a" --activate || return 1
  FIXTURE_VARIANT=co step deploy-b deploy "$b" --activate || return 1
  step ready-a wait_state "$da" ready 1200 || { rc=1; step errors-a engine_errors "$host"; }
  step ready-b wait_state "$db" ready 1200 || { rc=1; step errors-b engine_errors "$host"; }
  [ "$rc" = 0 ] || return 1
  step owned-a keep_owned "$da" ready
  step owned-b keep_owned "$db" ready
  step mem-ready host_mem "$host" ready
  snap both-ready
  step distinct python3 - "$EVID/accounting-$da-ready.json" "$EVID/accounting-$db-ready.json" "$EVID/owned-$da-ready.json" "$EVID/owned-$db-ready.json" <<'PY' || rc=1
import json, sys
aa, ab, oa, ob = (json.load(open(p)) for p in sys.argv[1:])
pa = {l["port"] for l in aa["endpoint_leases"]}; pb = {l["port"] for l in ab["endpoint_leases"]}
ra = sum(r["bytes"] for r in aa["reservations"]); rb = sum(r["bytes"] for r in ab["reservations"])
ida = {i["pid"] for i in oa}; idb = {i["pid"] for i in ob}
v = {"ports": [sorted(pa), sorted(pb)], "reserved_GiB": [round(ra / 2**30, 2), round(rb / 2**30, 2), round((ra + rb) / 2**30, 2)],
     "distinct_ports": bool(pa and pb and not pa & pb), "distinct_pids": bool(ida and idb and not ida & idb)}
print(json.dumps(v)); sys.exit(0 if v["distinct_ports"] and v["distinct_pids"] else 1)
PY
  step infer-a infer "$da" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step infer-b infer "$db" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step i1-a i1 "$da" "$a" "$da" || rc=1
  step i1-b i1 "$db" "$b" "$db" || rc=1
  if dry; then :; else
    python3 "$MATRIX_DIR/loadgen.py" --route "$da" --stream 4 --nonstream 8 --concurrency 6 --out "$EVID/load-a.jsonl" --summary "$EVID/load-a.summary.json" >"$EVID/load-a.out" 2>&1 &
    local la=$!
    python3 "$MATRIX_DIR/loadgen.py" --route "$db" --stream 4 --nonstream 8 --concurrency 6 --out "$EVID/load-b.jsonl" --summary "$EVID/load-b.summary.json" >"$EVID/load-b.out" 2>&1 &
    local lb=$!
    wait "$la" || rc=1; wait "$lb" || rc=1
  fi
  step load-summary cat "$EVID/load-a.summary.json" "$EVID/load-b.summary.json"
  # M22: stop the first; the second keeps serving.
  step stop-a stop_dep "$da" || rc=1
  step stopped-a wait_state "$da" stopped 600 || rc=1
  sleep 3
  step cleanup-a-only cleanup_check_partial "$da" "$host" "$EVID/owned-$da-ready.json" || rc=1
  step b-alive alive_check "$host" "$EVID/owned-$db-ready.json" || rc=1
  step infer-b-after infer "$db" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step owned-b-after keep_owned "$db" after
  step same-b same_identities "$EVID/owned-$db-ready.json" "$EVID/owned-$db-after.json" || rc=1
  step stop-b stop_dep "$db" || rc=1
  step stopped-b wait_state "$db" stopped 600 || rc=1
  sleep 3
  step cleanup-b cleanup_check "$db" "$host" "$EVID/owned-$db-ready.json" "$EVID/accounting-$db-ready.json" b || rc=1
  step mem-after host_mem "$host" after
  step delete-a delete_dep "$da" || rc=1
  step delete-b delete_dep "$db" || rc=1
  return "$rc"
}
