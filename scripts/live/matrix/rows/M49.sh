# shellcheck shell=bash
# M49 (T33; G10, G15): final cleanup of the soak state (M72 on what M48 left):
#   run_row.sh M49 --no-e0
#
# Expected: `delete deployment --stop` removes every deployment with verified
# cleanup (residue by id: only the tombstone remains); the ledger then holds no
# charge, reservation, lifecycle claim, request lease, retained binding or endpoint
# lease; neither host has an engine process, GPU compute process, SGLang
# rendezvous directory or bytecode in the runtime tree; MemAvailable is back near
# the pre-soak baseline (M48 records it); then SIGTERM both host roles and the
# server: each exits 0 (role_exec.sh records the status in the role's log) and no
# mllm role process remains on any machine.

M49_MEM_SLACK_KB=${M49_MEM_SLACK_KB:-$((4 * 1024 * 1024))}

live_deployments() {
  cli list deployments --format json | python3 -c 'import json,sys
d=json.load(sys.stdin)
for x in (d if isinstance(d, list) else d["deployments"]):
    n=x.get("name","")
    if n and not n.startswith("deleted/"): print(n, x.get("id",""))'
}

ledger_empty() {
  python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" | python3 -c 'import json,sys
s=json.load(sys.stdin)
held={k:s[k] for k in ("reservations","resource_owners","endpoint_leases","lifecycle_claims","request_leases","runtime_bindings") if s[k]}
live=[d for d in s["deployments"] if not d["name"].startswith("deleted/")]
print(json.dumps({"held":held,"live_deployments":live}))
sys.exit(1 if held or live else 0)'
}

host_final() { # host_final <host>: host_clean plus bytecode, and the pre-soak MemAvailable
  local host=$1 out rc=0
  out=$(host_clean "$host"; rsh "$host" "echo '## bytecode'; find $REMOTE_TREE/runtime \\( -name __pycache__ -o -name '*.pyc' \\) -print | head -5 | grep . && echo LEFTOVER_BYTECODE; true")
  echo "$out"
  grep -q LEFTOVER <<<"$out" && rc=1
  host_mem "$host" "final-$host" >/dev/null
  python3 - "$RUNSTATE/soak-baseline-mem.txt" "$EVID/mem.txt" "$host" "$M49_MEM_SLACK_KB" <<'PY' || rc=1
import json, sys
base = dict(l.split() for l in open(sys.argv[1]) if l.strip())
now = dict(l.split() for l in open(sys.argv[2]) if l.strip())
host, slack = sys.argv[3], int(sys.argv[4])
b, n = int(base[f"before-{host}"]), int(now[f"final-{host}"])
v = {"host": host, "baseline_kB": b, "final_kB": n, "delta_kB": b - n, "within_slack": b - n < slack}
print(json.dumps(v)); sys.exit(0 if v["within_slack"] else 1)
PY
  return "$rc"
}

roles_exit() { # after roles.sh down: each role logged exit 0 and none is running
  local rc=0 host
  tail -n 3 "$LRD/server.log" | grep -a '^# .* exit ' || true
  tail -n 1 "$LRD/server.log" | grep -aq '^# .* exit 0$' || { echo "FINDING: server did not log exit 0"; rc=1; }
  for host in "${MATRIX_HOSTS[@]}"; do
    rsh "$host" "tail -n 3 $RRD/host.log | grep -a '^# .* exit ' ; tail -n 1 $RRD/host.log | grep -aq '^# .* exit 0\$' || { echo 'FINDING: host role did not log exit 0'; exit 1; }; \
pgrep -af 'mllm[ ]start' && { echo 'FINDING: an mllm role still runs'; exit 1; }; true" || rc=1
  done
  pgrep -af 'mllm[ ]start' && { echo "FINDING: an mllm role still runs on control-host"; rc=1; }
  return "$rc"
}

row_main() {
  local rc=0 name id
  # A listing that cannot be read stops the row before anything is deleted or
  # any role is signalled (the first M49 run read none and took the roles down
  # with engines still retained).
  step before-list live_deployments || return 1
  while read -r name id; do
    [ -n "$name" ] || continue
    echo "$name $id" >>"$EVID/deleted.txt"
    step "delete-$name" timed "delete-$name" cli delete deployment "$name" --stop --format json || rc=1
    step "residue-$name" residue_check "$id" || rc=1
  done < <(live_deployments)
  step ledger-empty ledger_empty || rc=1
  sleep 5
  step final-a host_final "$HOST_A" || rc=1
  step final-b host_final "$HOST_B" || rc=1
  snap final
  step roles-down "$MATRIX_DIR/roles.sh" down || rc=1
  step roles-exit roles_exit || rc=1
  step after-a host_idle "$HOST_A" || rc=1
  step after-b host_idle "$HOST_B" || rc=1
  step ledger-after ledger_empty || rc=1
  return "$rc"
}
