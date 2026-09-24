# shellcheck shell=bash
# M73 (T08, T10, T37): one engine lifecycle through the shipped CLI, per fixture:
#   run_row.sh M73 --tag v92-4 -- v92-4
#   run_row.sh M73 --tag s92-4 -- s92-4
# Replaces the removed in-process suites (owner decision 2026-09-22):
# live_vllm.rs L1-L5 and L11, and live_sglang.rs SGL1-SGL3.
#
# Setup: server and the fixture's host enrolled and online; no other deployment
# active on that host (the leftover scan covers the whole host). The row deploys
# the variant <fixture>-lc so it never collides with M01/M05, and deletes it.
#
# Expected, step by step:
#   L1/SGL1  deploy --activate --wait reaches Ready; the owned identities include
#            the api process; the argv the kernel recorded is the protected entry
#            (vLLM: vllm_entry.py with --host 127.0.0.1, --served-model-name,
#            --tensor-parallel-size, --pipeline-parallel-size, --port, the user-args
#            marker, and for a deep fixture --enable-sleep-mode plus the guard
#            middleware; SGLang: sglang_entry.py with --public-settings-json and a
#            complete GPU-<uuid> CUDA_VISIBLE_DEVICES); no credential on argv.
#   L2/SGL2  a routed completion with the right answer, and a well-formed stream.
#   L3       the router refuses a caller without the key (401/403) and offers no
#            /metrics path (404); the engine port listens on loopback only and is
#            refused from control-host through the host's address; unkeyed engine calls
#            to /v1/models and the control routes return 401. The keyed half of the
#            old L3 needs the engine key, which the harness never reads; M28/M29
#            exercise the keyed control routes through the product's own park.
#   L4/SGL3  stop settles stopped with verified cleanup (identities absent, no
#            engine or GPU process, port free, nothing held).
#   L5       start again: Ready at a higher generation with a new binding and no pid
#            reused from the first launch. (The per-launch key rotation is not read;
#            M07 checks that a retired gate token is rejected.)
#   L11/SGL3 MemAvailable before deploy, at Ready and after the final stop:
#            Ready below before, and before minus after under 2 GiB.

M73_TAG=lc
M73_SLACK_KB=$((2 * 1024 * 1024))

deploy_lc() { FIXTURE_VARIANT=$M73_TAG deploy "$@"; }

# accounting and owned identities for one generation, kept under their own names.
keep_generation() { # keep_generation <deployment> <gen>
  local dep=$1 gen=$2
  owned "$dep" >/dev/null
  dry && return 0
  cp "$EVID/owned-$dep.json" "$EVID/owned-$gen.json"
  accounting "$dep" >"$EVID/accounting-ready-$gen.json"
}

engine_port() { # engine_port <accounting.json>
  python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1]))["endpoint_leases"][0]["port"])
except Exception: print("")' "$1"
}

# L1/SGL1: what the api process was actually given.
launch_argv() { # launch_argv <host> <engine> <gen>
  local host=$1 engine=$2 gen=$3 pid ticks boot
  if dry; then
    rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/proc_probe.py --pid <pid> --ticks <ticks> --boot <boot> --env CUDA_VISIBLE_DEVICES --env VLLM_SERVER_DEV_MODE"
    return 0
  fi
  read -r pid ticks boot < <(python3 -c 'import json,sys
ids = json.load(open(sys.argv[1]))
api = [i for i in ids if i.get("role") == "api"] or ids
print(api[0]["pid"], api[0]["start_ticks"], api[0]["boot_id"])' "$EVID/owned-$gen.json") || return 1
  rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/proc_probe.py --pid $pid --ticks $ticks --boot $boot --env CUDA_VISIBLE_DEVICES --env VLLM_SERVER_DEV_MODE" \
    >"$EVID/argv-$gen.json" || { cat "$EVID/argv-$gen.json"; return 1; }
  python3 - "$EVID/argv-$gen.json" "$engine" "$EVID/fixture-lc.json" <<'PY'
import json, sys
probe = json.load(open(sys.argv[1]))
engine = sys.argv[2]
fixture = json.load(open(sys.argv[3]))
argv = probe["argv"]
joined = " ".join(argv)
missing = []
if engine == "vllm":
    want = ["vllm_entry.py", "--host", "--served-model-name", "--tensor-parallel-size",
            "--pipeline-parallel-size", "--port", "--mllm-user-args"]
    if fixture.get("residency") == "deep":
        want += ["--enable-sleep-mode", "--middleware"]
    missing = [w for w in want if w not in joined]
    if "--host" in argv and argv[argv.index("--host") + 1] != "127.0.0.1":
        missing.append("--host 127.0.0.1")
else:
    missing = [w for w in ("sglang_entry.py", "--public-settings-json") if w not in joined]
    cvd = probe["env"].get("CUDA_VISIBLE_DEVICES", "")
    if not (cvd.startswith("GPU-") and len(cvd) == 40):
        missing.append(f"CUDA_VISIBLE_DEVICES complete GPU uuid (found {cvd!r})")
if probe["credential_in_argv"]:
    missing.append("no credential on argv")
print(json.dumps({"engine": engine, "argv": argv, "env": probe["env"], "missing": missing}))
sys.exit(1 if missing else 0)
PY
}

# L3, router half: no key is refused; /metrics is not a router path.
router_probe() {
  dry && { log_cmd control-host "GET 127.0.0.1:8443/v1/models without a key (want 401/403); GET /metrics with the key from MLLM_API_KEY (want 404)"; return 0; }
  python3 - <<'PY'
import http.client, json, os, sys
def get(path, key):
    c = http.client.HTTPConnection("127.0.0.1", 8443, timeout=10)
    c.request("GET", path, headers={"Authorization": "Bearer " + key} if key else {})
    status = c.getresponse().status
    c.close()
    return status
unkeyed = get("/v1/models", None)
metrics = get("/metrics", os.environ["MLLM_API_KEY"])
ok = unkeyed in (401, 403) and metrics == 404
print(json.dumps({"unkeyed_models": unkeyed, "keyed_metrics": metrics, "ok": ok}))
sys.exit(0 if ok else 1)
PY
}

# L3, engine half: loopback-only listener, off-host refusal, unkeyed 401s.
engine_probe() { # engine_probe <host> <engine> <gen>
  local host=$1 engine=$2 gen=$3 port ip paths rc=0
  ip=$(host_ip "$host")
  case $engine in
    vllm) paths="/v1/models /sleep /wake_up /collective_rpc /is_sleeping" ;;
    *) paths="/v1/models /release_memory_occupation /resume_memory_occupation /flush_cache /update_weights_from_disk" ;;
  esac
  if dry; then port="<port>"; else port=$(engine_port "$EVID/accounting-ready-$gen.json"); fi
  [ -n "$port" ] || { echo "no endpoint lease recorded"; return 1; }
  echo "## listeners on $port (want loopback only)"
  rsh "$host" "ss -ltnH 2>/dev/null | awk '{print \$4}' | grep -E ':$port\$'" | tee "$EVID/listen-$gen.txt"
  dry || { grep -vqE '^(127\.0\.0\.1|\[::1\]):' "$EVID/listen-$gen.txt" && { echo "FINDING: engine listens beyond loopback"; rc=1; }; }
  dry || [ -s "$EVID/listen-$gen.txt" ] || { echo "FINDING: engine port not listening"; rc=1; }
  echo "## off-host connect $ip:$port from control-host (want refused)"
  if x timeout 10 bash -c "exec 3<>/dev/tcp/$ip/$port" 2>/dev/null; then
    dry || { echo "FINDING: engine reachable off the loopback"; rc=1; }
  else
    echo "refused"
  fi
  echo "## unkeyed engine calls (want 401; /health alone may answer)"
  rsh "$host" "for p in /health $paths; do printf '%s %s\n' \$p \$(curl -s -m 10 -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:$port\$p); done" \
    | tee "$EVID/unkeyed-$gen.txt"
  dry || { awk '$1 != "/health" && $2 != "401" {bad=1} END {exit bad}' "$EVID/unkeyed-$gen.txt" || { echo "FINDING: an unkeyed engine call was not refused"; rc=1; }; }
  return "$rc"
}

# L5: a restart is a new launch.
restart_check() { # restart_check <deployment>
  dry && { log_cmd control-host "compare generation, binding and pids of owned-gen1.json and owned-gen2.json"; return 0; }
  python3 - "$EVID/owned-gen1.json" "$EVID/owned-gen2.json" "$EVID/accounting-ready-gen1.json" "$EVID/accounting-ready-gen2.json" <<'PY'
import json, sys
o1, o2, a1, a2 = (json.load(open(p)) for p in sys.argv[1:])
g1 = (a1["deployment"] or [{}])[0].get("current_generation")
g2 = (a2["deployment"] or [{}])[0].get("current_generation")
b1 = {i["binding_id"] for i in o1}
b2 = {i["binding_id"] for i in o2}
reused = sorted({i["pid"] for i in o1} & {i["pid"] for i in o2})
verdict = {"generation": [g1, g2], "bindings": [sorted(b1), sorted(b2)], "reused_pids": reused}
verdict["new_launch"] = bool(o2) and g1 is not None and g2 is not None and g2 > g1 and not (b1 & b2) and not reused
print(json.dumps(verdict))
sys.exit(0 if verdict["new_launch"] else 1)
PY
}

mem_check() {
  dry && { log_cmd control-host "judge mem.txt: ready < before and before - after < ${M73_SLACK_KB} kB"; return 0; }
  python3 - "$EVID/mem.txt" "$M73_SLACK_KB" <<'PY'
import json, sys
m = dict(line.split() for line in open(sys.argv[1]) if line.strip())
before, ready, after = (int(m[k]) for k in ("before", "ready", "after"))
slack = int(sys.argv[2])
verdict = {"before_kB": before, "ready_kB": ready, "after_kB": after, "delta_kB": before - after,
           "ready_below_before": ready < before, "returned": before - after < slack}
print(json.dumps(verdict))
sys.exit(0 if verdict["ready_below_before"] and verdict["returned"] else 1)
PY
}

row_main() {
  local fix=${1:?fixture, e.g. v92-4} dep host engine rc=0
  dep=$fix-$M73_TAG
  host=$(fixture_host "$fix")
  engine=$(fixture_engine "$fix")
  step variant variant "$fix" "$M73_TAG" || return 1
  dry || cp "$(FIXTURE_VARIANT=$M73_TAG fixture_file "$fix")" "$EVID/fixture-lc.json"
  echo "fixture $fix deployment $dep host $host engine $engine" | tee -a "$EVID/timeline.txt"
  step before host_idle "$host" || return 1
  step mem-before host_mem "$host" before

  # L1/SGL1
  step deploy deploy_lc "$fix" --activate --wait || { step engine-errors engine_errors "$host"; return 1; }
  step status status_dep "$dep"
  step inspect inspect_dep "$dep"
  step owned-gen1 keep_generation "$dep" gen1
  step argv launch_argv "$host" "$engine" gen1 || rc=1
  step mem-ready host_mem "$host" ready
  snap ready

  # L2/SGL2
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stream infer "$dep" "Count from 1 to 5 separated by commas." --stream --max-tokens 1024 || rc=1

  # L3
  step router-probe router_probe || rc=1
  step engine-probe engine_probe "$host" "$engine" gen1 || rc=1

  # L4/SGL3
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup-gen1 cleanup_check "$dep" "$host" "$EVID/owned-gen1.json" "$EVID/accounting-ready-gen1.json" gen1 || rc=1

  # L5
  step restart start_dep "$dep" || rc=1
  step restarted wait_state "$dep" ready 900 || rc=1
  step owned-gen2 keep_generation "$dep" gen2
  step restart-check restart_check "$dep" || rc=1
  step infer-gen2 infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stop-gen2 stop_dep "$dep" || rc=1
  step stopped-gen2 wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup-gen2 cleanup_check "$dep" "$host" "$EVID/owned-gen2.json" "$EVID/accounting-ready-gen2.json" gen2 || rc=1

  # L11/SGL3
  step mem-after host_mem "$host" after
  step mem-check mem_check || rc=1

  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
