# shellcheck shell=bash
# M48 (T15, T16, T26, T27; G01-G15): seeded random-walk soak on both Sparks.
#   KEEP_FAILED=1 SOAK_SEED=<n> SOAK_STEPS=200 run_row.sh M48
#   resume: KEEP_FAILED=1 SOAK_RESUME=1 SOAK_PLAN=<plan.json> SOAK_SEED=<n> SOAK_FROM_STEP=<k> run_row.sh M48 --tag r<k>
#
# Setup: server and both hosts online, host-b on the tight policy
# (roles.sh host-doc host-b tight; host-down; host-up) and host-a on
# normal; nothing deployed. KEEP_FAILED=1 keeps the soak state for M49, which
# is the final cleanup.
#
# Deployments (q4 mostly; both engines on both hosts; every engine launched with
# its tool parser through accept_extra_args, rows/TC.sh):
#   v92-4-sk   vLLM q4 on host-a (hermes)        s92-14-sk  SGLang q14 on host-a (qwen25)
#   s17-4-sk   SGLang q4 on host-b (qwen25)      v17-14-sk  vLLM q14 on host-b (hermes)
#   s92-4-rep  SGLang q4, two instances spread over both hosts, replica route
#              qwen3-4b (qwen25); s92-4-rep.one.yaml is its count-only revision
# host-a (normal, 97.35 GiB) holds all three of its charges at once (94 GiB);
# host-b (tight, 84 GiB) holds two of its three, so a request for the third
# switches automatically.
#
# soak.py then walks SOAK_STEPS steps (seed SOAK_SEED, resumable with
# SOAK_FROM_STEP) and checks the invariants after every step; see its docstring.
# Evidence: $EVID/soak/{steps.jsonl,violations.jsonl,summary-*.json,steps/}.

SOAK_TC_VLLM='{"accept_extra_args": true, "extra_args": ["--enable-auto-tool-choice", "--tool-call-parser", "hermes"]}'
SOAK_TC_SGLANG='{"accept_extra_args": true, "extra_args": ["--tool-call-parser", "qwen25"]}'
SOAK_REP_ROUTE=qwen3-4b

soak_plan() { # soak_plan <out.json>
  python3 - "$1" "$RUNSTATE" "${POLICY_92:-normal}" "${POLICY_17:-normal}" <<'PY'
import json, sys
out, runstate, p92, p17 = sys.argv[1:]
fx = runstate + "/fixtures"
def doc(path):
    return json.load(open(path))
def req(d):
    return int(d["engine_config"]["memory"]["request"].rstrip("B"))
deps = {}
for name, fixture, engine, model, hosts in (
        ("v92-4-sk", "v92-4", "vllm", "4", ["host-a"]),
        ("s92-14-sk", "s92-14", "sglang", "14", ["host-a"]),
        ("s17-4-sk", "s17-4", "sglang", "4", ["host-b"]),
        ("v17-14-sk", "v17-14", "vllm", "14", ["host-b"])):
    path = f"{fx}/{fixture}.sk.yaml"
    d = doc(path)
    deps[name] = {"file": path, "route": d["routes"][0], "engine": engine, "model": model, "hosts": hosts,
                  "instances": 1, "request_bytes": req(d)}
rep = f"{fx}/s92-4.rep.yaml"
d = doc(rep)
one = dict(d, instances=1)
json.dump(one, open(f"{fx}/s92-4.repone.yaml", "w"), indent=1)
deps[d["name"]] = {"file": rep, "file_one": f"{fx}/s92-4.repone.yaml", "route": d["routes"][0], "engine": "sglang",
                   "model": "4", "hosts": ["host-a", "host-b"], "instances": 2, "request_bytes": req(d)}
limits = {}
for host, policy in (("host-a", p92), ("host-b", p17)):
    h = doc(f"{runstate}/host-{host}-{policy}.yaml")
    rp = h["resource_policy"]
    limits[host] = {"policy": policy, "managed_limit": int(str(rp["domains"]["unified"]["managed_limit"]).rstrip("B")),
                    "max_parked": int(rp["max_parked"])}
json.dump({"deployments": deps, "replica": d["name"], "limits": limits}, open(out, "w"), indent=1)
print(json.dumps({"deployments": sorted(deps), "limits": limits}))
PY
}

soak_setup() {
  step variant-v92-4 variant v92-4 sk --engine-config-json "$SOAK_TC_VLLM" || return 1
  step variant-s92-14 variant s92-14 sk --engine-config-json "$SOAK_TC_SGLANG" || return 1
  step variant-s17-4 variant s17-4 sk --engine-config-json "$SOAK_TC_SGLANG" || return 1
  step variant-v17-14 variant v17-14 sk --engine-config-json "$SOAK_TC_VLLM" || return 1
  step variant-rep variant s92-4 rep --route "$SOAK_REP_ROUTE" --engine-config-json "$SOAK_TC_SGLANG" \
    --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  step plan soak_plan "$EVID/plan.json" || return 1
}

row_main() {
  local rc=0 seed=${SOAK_SEED:-$(date +%s)} steps=${SOAK_STEPS:-200} from=${SOAK_FROM_STEP:-1}
  echo "seed $seed steps $steps from $from" | tee -a "$EVID/timeline.txt"
  [ "${POLICY_17:-}" = tight ] || { echo "host-b is not on the tight policy"; return 1; }
  if [ "${SOAK_RESUME:-0}" != 1 ]; then
    step before-92 host_idle host-a || return 1
    step before-17 host_idle host-b || return 1
    soak_setup || return 1
    # Pre-soak MemAvailable, the baseline M49 compares against.
    step mem-92 host_mem host-a before-host-a
    step mem-17 host_mem host-b before-host-b
    dry || grep '^before-' "$EVID/mem.txt" >"$RUNSTATE/soak-baseline-mem.txt"
    FIXTURE_VARIANT=sk step deploy-s17-4 deploy s17-4 --activate --wait || rc=1
    FIXTURE_VARIANT=sk step deploy-v17-14 deploy v17-14 --activate --wait || rc=1
    FIXTURE_VARIANT=sk step deploy-v92-4 deploy v92-4 --activate --wait || rc=1
    FIXTURE_VARIANT=sk step deploy-s92-14 deploy s92-14 --activate --wait || rc=1
    FIXTURE_VARIANT=rep step deploy-rep deploy s92-4 || rc=1
    [ "$rc" = 0 ] || { step errors-92 engine_errors host-a; step errors-17 engine_errors host-b; return 1; }
  else
    # Resume a stopped walk on the same deployments (SOAK_PLAN: the earlier plan.json).
    cp "${SOAK_PLAN:?SOAK_PLAN is the plan.json of the walk being resumed}" "$EVID/plan.json"
  fi
  dry && return 0
  export LRD SERVER_CFG SERVER_DB MLLM EVID RUNSTATE RRD REMOTE_TREE HOST_ID_92 HOST_ID_17 MLLM_API_KEY
  step check-0 python3 "$MATRIX_DIR/soak.py" --plan "$EVID/plan.json" --seed "$seed" --check-only || true
  python3 "$MATRIX_DIR/soak.py" --plan "$EVID/plan.json" --seed "$seed" --steps "$steps" --from-step "$from" \
    --max-hours "${SOAK_MAX_HOURS:-8}" 2>&1 | tee -a "$EVID/soak.out"
  rc=${PIPESTATUS[0]}
  return "$rc"
}
