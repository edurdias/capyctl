# shellcheck shell=bash
# Instances (ADR 0013, Q7; P1), with co-residence fixtures:
#   run_row.sh M20I --tag same -- same va-4     two instances of one deployment on one host
#   run_row.sh M20I --tag spread -- spread va-4 two instances spread across both hosts,
#                                                 then a count change 2 -> 1 -> 2
#
# Expected (same): both instances Ready on the fixture's host with distinct ports and
# process groups; a burst is balanced over both (router_selection); `stop instance
# <dep>/1` stops only that instance with verified release while /0 keeps serving;
# `start instance <dep>/1` brings it back (new generation); stop; cleanup; delete.
# Expected (spread): one instance per host; both serve; a count-only revision to 1
# retires one instance with verified cleanup and leaves the other untouched (same
# identities); a revision back to 2 adds a stopped instance (ADR 0013 §7) that
# `start deployment` brings up without touching the first.

SERVER_LOG_LINE=0
sel_mark() { SERVER_LOG_LINE=$(wc -l <"$LRD/server.log"); }
sel_count() { local label=$1; shift
  cli list hosts --output json >"$EVID/hosts-$label.json" 2>/dev/null || true
  python3 "$MATRIX_DIR/selections.py" --log "$LRD/server.log" --from-line "$SERVER_LOG_LINE" \
    --hosts-json "$EVID/hosts-$label.json" "$@" | tee "$EVID/selections-$label.json"; }

ready_count() { # ready_count <dep> <n> <timeout>
  local dep=$1 want=$2 timeout=$3 i n=0
  dry && return 0
  for ((i = 0; i < timeout; i += 3)); do
    n=$(status_dep "$dep" 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
print(sum(1 for x in d.get("instances",[]) if x.get("observed_state")=="ready"))' 2>/dev/null || echo 0)
    [ "$n" = "$want" ] && { echo "$want ready after ${i}s"; return 0; }
    sleep 3
  done
  echo "ready $n, not $want, after ${timeout}s"; return 1
}

instances() { status_dep "$1" | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
for i in d.get("instances",[]): print(json.dumps({k:i.get(k) for k in ("index","host_id","observed_state","generation","operator_stopped","lifecycle")}))'; }

owned_by_instance() { # owned identities with binding, for a label
  owned "$1" >/dev/null
  dry && return 0
  cp "$EVID/owned-$1.json" "$EVID/owned-$1-$2.json"
  python3 -c 'import json,sys
for i in json.load(open(sys.argv[1])): print(i["binding_id"], i.get("host_id"), i["role"], i["pid"])' "$EVID/owned-$1-$2.json"
}

revise_count() { # revise_count <dep> <fixture> <tag> <count>: a count-only revision (SPEC §14, ADR 0013 §7)
  local dep=$1 fix=$2 tag=$3 n=$4 rev
  rev=$(status_dep "$dep" | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d); print(d["revision"])')
  x python3 - "$(FIXTURE_VARIANT=$tag fixture_file "$fix")" "$EVID/revision-$n.json" "$n" <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d["instances"] = int(sys.argv[3])
json.dump(d, open(sys.argv[2], "w"), indent=1)
PY
  cli deploy model --file "$EVID/revision-$n.json" --revision "$rev" --output json
}

burst() { load --route "$1" --nonstream 24 --concurrency 12 --max-tokens 64; }

row_same() {
  local fix=$1 host dep rc=0
  host=$(fixture_host "$fix"); dep=$fix-co2
  step before host_idle "$host" || return 1
  step variant variant "$fix" co2 --co --document-json '{"instances": 2}' || return 1
  FIXTURE_VARIANT=co2 step deploy deploy "$fix" --activate || return 1
  step ready ready_count "$dep" 2 1200 || rc=1
  step instances-ready instances "$dep"
  step owned-ready owned_by_instance "$dep" ready
  step accounting-ready keep_owned "$dep" ready
  sel_mark; step burst burst "$dep" || rc=1
  step selections sel_count burst
  step stop-1 cli stop instance "$dep/1" --output json || rc=1
  step one-ready ready_count "$dep" 1 300 || rc=1
  sleep 5
  step instances-after-stop instances "$dep"
  step owned-after-stop owned_by_instance "$dep" stop1
  step burst-after-stop burst "$dep" || rc=1
  step start-1 cli start instance "$dep/1" --output json || rc=1
  step two-ready ready_count "$dep" 2 900 || rc=1
  step instances-after-start instances "$dep"
  step owned-after-start owned_by_instance "$dep" start1
  sel_mark; step burst-after-start burst "$dep" || rc=1
  step selections-after-start sel_count after-start
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check_partial "$dep" "$host" "$EVID/owned-$dep-start1.json" || rc=1
  step host-clean host_idle "$host" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}

row_spread() {
  local fix=$1 dep rc=0
  dep=$fix-sp2
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant variant "$fix" sp2 --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  FIXTURE_VARIANT=sp2 step deploy deploy "$fix" --activate || return 1
  step ready ready_count "$dep" 2 1200 || rc=1
  step instances-ready instances "$dep"
  step owned-ready owned_by_instance "$dep" ready
  sel_mark; step burst burst "$dep" || rc=1
  step selections sel_count burst --min-hosts 2 || rc=1
  step revise-1 revise_count "$dep" "$fix" sp2 1 || rc=1
  step one-ready ready_count "$dep" 1 600 || rc=1
  sleep 10
  step instances-1 instances "$dep"
  step owned-1 owned_by_instance "$dep" count1
  step burst-1 burst "$dep" || rc=1
  step revise-2 revise_count "$dep" "$fix" sp2 2 || rc=1
  # ADR 0013 §7: an increase creates the new index stopped; it activates at the
  # next `start deployment` (or on demand). The running instance is untouched.
  step instances-2-stopped instances "$dep"
  step start-all start_dep "$dep" || rc=1
  step two-ready ready_count "$dep" 2 1200 || rc=1
  step instances-2 instances "$dep"
  step owned-2 owned_by_instance "$dep" count2
  sel_mark; step burst-2 burst "$dep" || rc=1
  step selections-2 sel_count count2 --min-hosts 2 || rc=1
  step kept python3 - "$EVID/owned-$dep-count1.json" "$EVID/owned-$dep-count2.json" <<'PY' || rc=1
import json, sys
one, two = (json.load(open(p)) for p in sys.argv[1:])
k = lambda s: {(i["pid"], i["start_ticks"], i["boot_id"]) for i in s}
kept = k(one) <= k(two)
print(json.dumps({"survivor_untouched": kept, "count1": len(one), "count2": len(two)}))
sys.exit(0 if kept else 1)
PY
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 900 || rc=1
  sleep 3
  step cleanup cleanup_check_partial "$dep" "$HOST_A" "$EVID/owned-$dep-count2.json" || rc=1
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() {
  case ${1:-same} in
    same) row_same "${2:-va-4}" ;;
    spread) row_spread "${2:-va-4}" ;;
    *) echo "usage: same|spread <fixture>"; return 2 ;;
  esac
}
