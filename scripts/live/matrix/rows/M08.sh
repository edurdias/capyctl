# shellcheck shell=bash
# M08 (T37, T21): development controls and engine surfaces stay off every
# non-loopback path, with s92-4 and v92-4 Ready together on host-a.
#   run_row.sh M08
#
# Expected:
#   router   with the inference key, every engine control or native path
#            (/sleep, /wake_up, /collective_rpc, /is_sleeping,
#            /release_memory_occupation, /resume_memory_occupation, /flush_cache,
#            /update_weights_from_disk, /metrics, /health, /get_server_info) is not
#            served (404); without the key every path is refused (401/403).
#   ingress  the host's private ingress on its Tailscale address refuses the same
#            paths from control-host without the gate key (401/403/404, never 2xx).
#   engines  each engine port listens on loopback only and is refused from control-host;
#            unkeyed loopback calls are refused 401, except /health and SGLang's
#            /metrics (read-only, loopback, marked in status).
#   status   v92-4 reports development_controls.state `exposed` with its surface,
#            mitigations and production_safe false; s92-4 is not exposed and
#            carries unauthenticated_local_surfaces [/metrics] on loopback.
# Leaves both stopped with verified cleanup and deletes them.

M08_PATHS="/sleep /wake_up /collective_rpc /is_sleeping /release_memory_occupation /resume_memory_occupation /flush_cache /update_weights_from_disk /metrics /health /get_server_info /v1/load_lora_adapter"

router_paths() {
  dry && { log_cmd control-host "router probes"; return 0; }
  python3 - "$M08_PATHS" <<'PY'
import http.client, json, os, sys
paths = sys.argv[1].split()
def call(method, path, key):
    c = http.client.HTTPConnection("127.0.0.1", 8443, timeout=15)
    headers = {"Content-Type": "application/json"}
    if key:
        headers["Authorization"] = "Bearer " + key
    c.request(method, path, body=b"{}" if method == "POST" else None, headers=headers)
    s = c.getresponse().status
    c.close()
    return s
bad = []
out = {}
for p in paths:
    for m in ("GET", "POST"):
        keyed = call(m, p, os.environ["MLLM_API_KEY"])
        unkeyed = call(m, p, None)
        out[f"{m} {p}"] = {"keyed": keyed, "unkeyed": unkeyed}
        # The router's own /health may answer; no engine path may.
        if p == "/health":
            continue
        if 200 <= keyed < 300 or 200 <= unkeyed < 300:
            bad.append(f"{m} {p} keyed={keyed} unkeyed={unkeyed}")
print(json.dumps({"calls": out, "findings": bad}, indent=1))
sys.exit(1 if bad else 0)
PY
}

ingress_paths() { # ingress_paths <host>
  local ip
  ip=$(host_ip "$1")
  dry && { log_cmd control-host "curl http://$ip:$INGRESS_PORT<path> without gate key"; return 0; }
  local p m code bad=0
  for p in $M08_PATHS /v1/models /v1/chat/completions; do
    for m in GET POST; do
      code=$(curl -sk -m 10 -o /dev/null -w '%{http_code}' -X "$m" -H 'Content-Type: application/json' -d '{}' "http://$ip:$INGRESS_PORT$p" || true)
      echo "$m $p $code"
      # The ingress is plain HTTP on the trusted private link (host document `address`).
      # 000 is no answer at all, which proves nothing either way.
      case $code in 2*|000) bad=1; echo "FINDING: ingress answered $m $p with $code" ;; esac
    done
  done
  return "$bad"
}

engine_surface() { # engine_surface <host> <dep> <engine>
  local host=$1 dep=$2 engine=$3 port ip rc=0
  ip=$(host_ip "$host")
  if dry; then port="<port>"; else
    port=$(accounting "$dep" | python3 -c 'import json,sys; l=json.load(sys.stdin)["endpoint_leases"]; print(l[0]["port"] if l else "")'); fi
  [ -n "$port" ] || { echo "no endpoint lease for $dep"; return 1; }
  echo "## $dep port $port listeners"
  rsh "$host" "ss -ltnH 2>/dev/null | awk '{print \$4}' | grep -E ':$port\$'" | tee "$EVID/listen-$dep.txt"
  dry || { grep -vqE '^(127\.0\.0\.1|\[::1\]):' "$EVID/listen-$dep.txt" && { echo "FINDING: beyond loopback"; rc=1; }; }
  dry || [ -s "$EVID/listen-$dep.txt" ] || { echo "FINDING: not listening"; rc=1; }
  if x timeout 10 bash -c "exec 3<>/dev/tcp/$ip/$port" 2>/dev/null; then dry || { echo "FINDING: reachable from control-host"; rc=1; }; else echo "off-host connect refused"; fi
  rsh "$host" "for p in /v1/models $M08_PATHS; do for m in GET POST; do printf '%s %s %s\n' \$m \$p \$(curl -s -m 10 -o /dev/null -w '%{http_code}' -X \$m -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:$port\$p); done; done" \
    | tee "$EVID/unkeyed-$dep.txt"
  dry && return 0
  # Allowed unkeyed answers: /health; SGLang /metrics (GET). Anything else must be 401 (404/405 = not served).
  awk -v e="$engine" '
    $2 == "/health" { next }
    e == "sglang" && $2 == "/metrics" { next }
    $3 ~ /^2/ { print "FINDING: unkeyed " $1 " " $2 " answered " $3; bad = 1 }
    END { exit bad }' "$EVID/unkeyed-$dep.txt" || rc=1
  return "$rc"
}

# Every TCP listener held by an engine process (the owned groups' python, vLLM
# and SGLang workers) must be on loopback; any other is probed from control-host.
engine_sockets() { # engine_sockets <host>
  local host=$1 ip addr port rc=0
  ip=$(host_ip "$host")
  rsh "$host" "ss -ltnpH 2>/dev/null | grep -E 'python|vllm|sglang|VLLM|EngineCore' | awk '{print \$4, \$6}'" | tee "$EVID/engine-sockets.txt"
  dry && return 0
  while read -r addr _; do
    case $addr in 127.0.0.1:*|'[::1]':*) continue ;; esac
    port=${addr##*:}
    echo "FINDING: engine listener beyond loopback: $addr"
    rc=1
    if timeout 5 bash -c "exec 3<>/dev/tcp/$ip/$port" 2>/dev/null; then
      echo "FINDING: $ip:$port accepts connections from control-host"
    else
      echo "$ip:$port refused from control-host"
    fi
  done <"$EVID/engine-sockets.txt"
  return "$rc"
}

status_marks() {
  dry && return 0
  status_dep v92-4 >"$EVID/status-v92-4.json"; status_dep s92-4 >"$EVID/status-s92-4.json"
  cli list hosts --output json >"$EVID/hosts-marks.json"
  python3 - "$EVID/status-v92-4.json" "$EVID/status-s92-4.json" <<'PY'
import json, sys
v = json.load(open(sys.argv[1])); s = json.load(open(sys.argv[2]))
v = v.get("deployment", v); s = s.get("deployment", s)
vd, sd = v.get("development_controls", {}), s.get("development_controls", {})
uls = sd.get("unauthenticated_local_surfaces") or {}
checks = {
    "vllm_exposed": vd.get("state") == "exposed",
    "vllm_not_production_safe": vd.get("production_safe") is False,
    "vllm_surface": sorted(vd.get("surface") or []),
    "vllm_instance_marked": all((i.get("development_controls") or {}).get("state") == "exposed" for i in v.get("instances", [])),
    "sglang_state": sd.get("state"),
    "sglang_metrics_marked": uls.get("surface") == ["/metrics"] and uls.get("listener") == "loopback",
}
print(json.dumps(checks, indent=1))
ok = checks["vllm_exposed"] and checks["vllm_not_production_safe"] and checks["vllm_instance_marked"] and checks["sglang_metrics_marked"] and checks["vllm_surface"]
sys.exit(0 if ok else 1)
PY
}

row_main() {
  local host=host-a rc=0 dep
  step before host_idle "$host" || return 1
  step deploy-v deploy v92-4 --activate || return 1
  step deploy-s deploy s92-4 --activate || return 1
  step ready-v wait_state v92-4 ready 900 || rc=1
  step ready-s wait_state s92-4 ready 900 || rc=1
  if [ "$rc" != 0 ]; then
    step errors engine_errors "$host"
  else
    step owned-v owned v92-4
    step owned-s owned s92-4
    for dep in v92-4 s92-4; do accounting "$dep" >"$EVID/accounting-ready-$dep.json"; done
    snap ready
    step infer-v infer v92-4 "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
    step infer-s infer s92-4 "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
    step status-marks status_marks || rc=1
    step router router_paths || rc=1
    step ingress ingress_paths "$host" || rc=1
    step engine-v engine_surface "$host" v92-4 vllm || rc=1
    step engine-s engine_surface "$host" s92-4 sglang || rc=1
    step engine-sockets engine_sockets "$host" || rc=1
  fi
  for dep in v92-4 s92-4; do
    step "stop-$dep" stop_dep "$dep" || rc=1
  done
  for dep in v92-4 s92-4; do
    step "stopped-$dep" wait_state "$dep" stopped 600 || rc=1
  done
  sleep 3
  step cleanup-v cleanup_check v92-4 "$host" "$EVID/owned-v92-4.json" "$EVID/accounting-ready-v92-4.json" v || rc=1
  step cleanup-s cleanup_check s92-4 "$host" "$EVID/owned-s92-4.json" "$EVID/accounting-ready-s92-4.json" s || rc=1
  step delete-v delete_dep v92-4 || rc=1
  step delete-s delete_dep s92-4 || rc=1
  return "$rc"
}
