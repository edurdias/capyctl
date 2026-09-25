# shellcheck shell=bash
# M05 (T08, T37): deploy va-4 --activate --wait; one completion. Expected: vLLM
# Ready remotely through runtime/vllm_entry.py; the guard refuses unkeyed control
# routes. The guard check probes the engine's loopback port on host-a with no
# key and expects 401 on a control route and on /v1, and only /health unkeyed.

row_main() {
  local dep=va-4 host=$HOST_A
  step deploy deploy "$dep" --activate --wait || return 1
  step status status_dep "$dep"
  step owned owned "$dep"
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 || return 1
  step i1 i1 "$dep" "$dep" || return 1
  # Unkeyed calls from the host itself to the loopback engine port (from the ledger's endpoint lease).
  step guard guard_probe "$host" "$dep"
  snap ready
  step stop stop_dep "$dep" || return 1
  step stopped wait_state "$dep" stopped 300 || return 1
}

guard_probe() {
  local host=$1 dep=$2 port
  if dry; then rsh "$host" "for p in /health /v1/models /sleep /wake_up /collective_rpc; do curl -s -o /dev/null -w \"\$p %{http_code}\\n\" -X POST http://127.0.0.1:<port>\$p; done"; return 0; fi
  port=$(python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" | python3 -c '
import json, sys
snap = json.load(sys.stdin)
dep = [d["id"] for d in snap["deployments"] if d["name"] == sys.argv[1]][0]
binding = [b["id"] for b in snap["runtime_bindings"] if b["deployment_id"] == dep][0]
print([l["port"] for l in snap["endpoint_leases"] if l["binding_id"] == binding][0])' "$dep")
  rsh "$host" "for p in /health /v1/models /sleep /wake_up /collective_rpc; do curl -s -o /dev/null -w \"\$p %{http_code}\\n\" -X POST http://127.0.0.1:$port\$p; done"
}
