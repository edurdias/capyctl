# shellcheck shell=bash
# M27 / M31 (T15, T16, T19, T22, T23; W10): request-driven switching on one host
# under the tight budget (only one of the pair fits), A -> B -> A ... by requests
# alone, with distinct models (I1 at every step):
#   roles.sh host-doc host-a tight; roles.sh host-down host-a; roles.sh host-up host-a
#   run_row.sh M27 --tag s14-s30 -- s92-14 s92-30 1 restart_only   (same engine, A restart_only)
#   run_row.sh M31 --tag v14-s30 -- v92-14 s92-30 3 deep           (cross engine, deep, three cycles)
#
# Expected per switch: the request for the absent deployment waits in the queue
# (no refusal), the incumbent's admission closes, it drains, then parks (deep) or
# stops with verified release (restart_only), and the target is activated and
# answers correctly within the request deadline. The first q30 start is a solo
# first start (unmeasured, startup estimate above the managed limit): the host is
# emptied, the whole limit reserved, and its peak measured; later cycles wake
# parked instances warm (same processes, no cold init). Journal and switch events
# record each step. Afterwards `start deployment <A> --evict` reports its victims.
# Stop everything with verified cleanup; delete.

switch_to() { # switch_to <dep> <fixture> <label>: one request that must be served
  local dep=$1 fix=$2 label=$3
  echo "request_$label $(now_ms)" >>"$EVID/marks.txt"
  step "req-$label" timed "switch-$label" infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 --timeout 1800 || return 1
  echo "served_$label $(now_ms)" >>"$EVID/marks.txt"
  step "i1-$label" i1 "$dep" "$fix" "$dep" || return 1
  step "owned-$label" keep_owned "$dep" "$label"
  step "states-$label" states "$@"
  snap "$label"
}

states() {
  local d
  for d in $SWITCH_DEPS; do
    printf '%s ' "$d"; status_dep "$d" 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
print(json.dumps({k:d.get(k) for k in ("observed_state","generation","admission_enabled","dispatch_enabled","conditions")}, separators=(",",":")))'
  done
  python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" | python3 -c 'import json,sys
s=json.load(sys.stdin); print("reservations", json.dumps(s["reservations"]))'
}

switch_row() {
  local fa=$1 fb=$2 cycles=${3:-1} residency=${4:-deep} host da db c rc=0
  host=$(fixture_host "$fa")
  [ "${POLICY_92:-}" = tight ] || dry || { echo "host-a is not on the tight policy"; return 1; }
  local va="" mem=()
  # SWITCH_MEMORY_JSON sizes both sides' engine memory so that two small
  # models still cannot share the tight host (the 2026-09-24 q4 smoke used
  # '{"memory": {"request": "47244640256B", "kv_cache": "4294967296B"}}').
  [ -n "${SWITCH_MEMORY_JSON:-}" ] && mem=(--engine-config-json "$SWITCH_MEMORY_JSON")
  da=$fa; db=$fb
  if [ "$residency" = restart_only ]; then
    step variant-a variant "$fa" rs --residency restart_only "${mem[@]}" || return 1
    va=rs
  elif [ ${#mem[@]} -gt 0 ]; then
    step variant-a variant "$fa" big "${mem[@]}" || return 1
    va=big
  fi
  da=$fa${va:+-$va}
  # The derived wake placeholder (60 s + 5 s/GB) is below a measured SGLang
  # disk reload on GB10 (q30: 61 GB in about 350 s, M27 2026-09-23), so B
  # declares its wake timeout (a deployment setting, ADR 0014 A1).
  step variant-b variant "$fb" wk "${mem[@]}" --document-json '{"timeouts": {"wake": "900s"}}' || return 1
  db=$fb-wk
  SWITCH_DEPS="$da $db"
  step before host_idle "$host" || return 1
  FIXTURE_VARIANT=$va step deploy-a deploy "$fa" --activate --wait || return 1
  FIXTURE_VARIANT=wk step deploy-b deploy "$fb" || return 1
  step i1-a0 i1 "$da" "$fa" "$da" || rc=1
  step owned-a0 keep_owned "$da" a0
  for c in $(seq 1 "$cycles"); do
    switch_to "$db" "$fb" "b$c" || rc=1
    step "evidence-b$c" evidence "$da" "after-b$c"
    switch_to "$da" "$fa" "a$c" || rc=1
    step "evidence-a$c" evidence "$db" "after-a$c"
  done
  step distinct python3 "$MATRIX_DIR/identity_probe.py" distinct --goldens "$GOLDENS"
  # Warm switching (deep, found live 2026-09-23: every switch was a cold
  # restart): after its first start each side wakes in place, so its owned
  # identities stay the same from cycle to cycle.
  if [ "$residency" = deep ] && [ "$cycles" -gt 1 ]; then
    for c in $(seq 2 "$cycles"); do
      step "same-b$c" same_identities "$EVID/owned-$db-b1.json" "$EVID/owned-$db-b$c.json" || rc=1
    done
    # A's first yield may be a stop (B's solo first start empties the host,
    # M31 2026-09-24), so A is compared from its first return onward.
    for c in $(seq 2 "$cycles"); do
      step "same-a$c" same_identities "$EVID/owned-$da-a1.json" "$EVID/owned-$da-a$c.json" || rc=1
    done
  fi
  step switch-log bash -c "grep -aE 'switch' '$LRD/server.log' | grep -viE 'key|secret|bearer|token' | tail -n 60"
  # Operator eviction: B again, explicitly.
  step evict timed evict cli start deployment "$db" --evict --output json || rc=1
  step evict-ready wait_state "$db" ready 1800 || rc=1
  step states-evict states
  for d in $da $db; do step "stop-$d" stop_dep "$d"; done
  for d in $da $db; do step "stopped-$d" wait_state "$d" stopped 600; done
  sleep 5
  step host-clean host_idle "$host" || rc=1
  for d in $da $db; do
    step "acct-$d" cleanup_check_partial "$d" "$host" "$EVID/owned-$d-$([ "$d" = "$da" ] && echo a0 || echo b1).json" || rc=1
    step "delete-$d" delete_dep "$d" || rc=1
  done
  return "$rc"
}

row_main() { switch_row "${1:-s92-14}" "${2:-s92-30}" "${3:-1}" "${4:-restart_only}"; }
