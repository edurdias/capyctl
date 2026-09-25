# shellcheck shell=bash
# M48 (T15, T16, T26, T27; G01-G15): seeded random-walk soak on both hosts.
#   KEEP_FAILED=1 SOAK_SEED=<n> SOAK_STEPS=200 run_row.sh M48
#   resume: KEEP_FAILED=1 SOAK_RESUME=1 SOAK_SEED=<n> SOAK_FROM_STEP=<k> run_row.sh M48 --tag r<k>
#
# Setup: server and both hosts online, host-b on the tight policy
# (roles.sh host-doc host-b tight; host-down; host-up) and host-a on
# normal; nothing deployed. KEEP_FAILED=1 keeps the soak state for M49, which
# is the final cleanup.
#
# Deployments (q4 mostly; both engines on both hosts; every engine launched with
# its tool parser through accept_extra_args, rows/TC.sh):
#   va-4-sk   vLLM q4 on host-a (hermes)        sa-14-sk  SGLang q14 on host-a (qwen25)
#   sb-4-sk   SGLang q4 on host-b (qwen25)      vb-14-sk  vLLM q14 on host-b (hermes)
#   vb-4-sk   vLLM q4 on host-b (hermes), deployed stopped
#   sa-4-rep  SGLang q4, two instances spread over both hosts, replica route
#              qwen3-4b (qwen25); sa-4-rep.one.yaml is its count-only revision
# host-a (normal, 97.35 GiB) holds all three of its charges at once (94 GiB);
# host-b (tight, 84 GiB) holds any two of its three single-instance deployments,
# so the walk's switch operation starts two and requests the third, which must
# park or stop one of them.
#
# soak.py then walks SOAK_STEPS steps (seed SOAK_SEED, resumable with
# SOAK_FROM_STEP) and checks the invariants after every step; see its docstring.
# Evidence: $EVID/soak/{steps.jsonl,violations.jsonl,summary-*.json,steps/}.

SOAK_TC_VLLM='{"accept_extra_args": true, "extra_args": ["--enable-auto-tool-choice", "--tool-call-parser", "hermes"]}'
SOAK_TC_SGLANG='{"accept_extra_args": true, "extra_args": ["--tool-call-parser", "qwen25"]}'
SOAK_REP_ROUTE=qwen3-4b

soak_plan() { # soak_plan <out.json>
  python3 - "$1" "$RUNSTATE" "${POLICY_a:-normal}" "${POLICY_b:-normal}" <<'PY'
import json, os, sys
out, runstate, pa, pb = sys.argv[1:]
A, B = os.environ["HOST_A"], os.environ["HOST_B"]
fx = runstate + "/fixtures"
def doc(path):
    return json.load(open(path))
def req(d):
    return int(d["engine_config"]["memory"]["request"].rstrip("B"))
deps = {}
for name, fixture, engine, model, hosts in (
        ("va-4-sk", "va-4", "vllm", "4", [A]),
        ("sa-14-sk", "sa-14", "sglang", "14", [A]),
        ("sb-4-sk", "sb-4", "sglang", "4", [B]),
        ("vb-4-sk", "vb-4", "vllm", "4", [B]),
        ("vb-14-sk", "vb-14", "vllm", "14", [B])):
    path = f"{fx}/{fixture}.sk.yaml"
    d = doc(path)
    deps[name] = {"file": path, "route": d["routes"][0], "engine": engine, "model": model, "hosts": hosts,
                  "instances": 1, "request_bytes": req(d)}
rep = f"{fx}/sa-4.rep.yaml"
d = doc(rep)
one = dict(d, instances=1)
json.dump(one, open(f"{fx}/sa-4.repone.yaml", "w"), indent=1)
deps[d["name"]] = {"file": rep, "file_one": f"{fx}/sa-4.repone.yaml", "route": d["routes"][0], "engine": "sglang",
                   "model": "4", "hosts": [A, B], "instances": 2, "request_bytes": req(d)}
limits = {}
for host, policy in ((A, pa), (B, pb)):
    h = doc(f"{runstate}/host-{host}-{policy}.yaml")
    rp = h["resource_policy"]
    limits[host] = {"policy": policy, "managed_limit": int(str(rp["domains"]["unified"]["managed_limit"]).rstrip("B")),
                    "max_parked": int(rp["max_parked"])}
json.dump({"deployments": deps, "replica": d["name"], "limits": limits}, open(out, "w"), indent=1)
print(json.dumps({"deployments": sorted(deps), "limits": limits}))
PY
}

soak_setup() {
  step variant-va-4 variant va-4 sk --engine-config-json "$SOAK_TC_VLLM" || return 1
  step variant-sa-14 variant sa-14 sk --engine-config-json "$SOAK_TC_SGLANG" || return 1
  step variant-sb-4 variant sb-4 sk --engine-config-json "$SOAK_TC_SGLANG" || return 1
  step variant-vb-14 variant vb-14 sk --engine-config-json "$SOAK_TC_VLLM" || return 1
  step variant-vb-4 variant vb-4 sk --engine-config-json "$SOAK_TC_VLLM" || return 1
  step variant-rep variant sa-4 rep --route "$SOAK_REP_ROUTE" --engine-config-json "$SOAK_TC_SGLANG" \
    --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  step plan soak_plan "$EVID/plan.json" || return 1
}

row_main() {
  local rc=0 seed=${SOAK_SEED:-$(date +%s)} steps=${SOAK_STEPS:-200} from=${SOAK_FROM_STEP:-1}
  echo "seed $seed steps $steps from $from" | tee -a "$EVID/timeline.txt"
  [ "${POLICY_b:-}" = tight ] || { echo "$HOST_B is not on the tight policy"; return 1; }
  if [ "${SOAK_RESUME:-0}" != 1 ]; then
    step before-a host_idle "$HOST_A" || return 1
    step before-b host_idle "$HOST_B" || return 1
    soak_setup || return 1
    # Pre-soak MemAvailable, the baseline M49 compares against.
    step mem-a host_mem "$HOST_A" "before-$HOST_A"
    step mem-b host_mem "$HOST_B" "before-$HOST_B"
    dry || grep '^before-' "$EVID/mem.txt" >"$RUNSTATE/soak-baseline-mem.txt"
    FIXTURE_VARIANT=sk step deploy-sb-4 deploy sb-4 --activate --wait || rc=1
    FIXTURE_VARIANT=sk step deploy-vb-14 deploy vb-14 --activate --wait || rc=1
    FIXTURE_VARIANT=sk step deploy-vb-4 deploy vb-4 || rc=1
    FIXTURE_VARIANT=sk step deploy-va-4 deploy va-4 --activate --wait || rc=1
    FIXTURE_VARIANT=sk step deploy-sa-14 deploy sa-14 --activate --wait || rc=1
    FIXTURE_VARIANT=rep step deploy-rep deploy sa-4 || rc=1
    [ "$rc" = 0 ] || { step errors-a engine_errors "$HOST_A"; step errors-b engine_errors "$HOST_B"; return 1; }
  else
    # Resume a stopped walk on the deployments it left: same fixtures and plan,
    # and any plan deployment that does not exist yet is deployed stopped.
    soak_setup || return 1
    local f
    for f in va-4 sa-14 sb-4 vb-14 vb-4; do
      status_dep "$f-sk" >/dev/null 2>&1 || { FIXTURE_VARIANT=sk step "deploy-$f" deploy "$f" || rc=1; }
    done
    status_dep sa-4-rep >/dev/null 2>&1 || { FIXTURE_VARIANT=rep step deploy-rep deploy sa-4 || rc=1; }
    [ "$rc" = 0 ] || return 1
  fi
  dry && return 0
  export LRD SERVER_CFG SERVER_DB MLLM EVID RUNSTATE RRD REMOTE_TREE HOST_ID_a HOST_ID_b MLLM_API_KEY
  step check-0 python3 "$MATRIX_DIR/soak.py" --plan "$EVID/plan.json" --seed "$seed" --check-only || true
  python3 "$MATRIX_DIR/soak.py" --plan "$EVID/plan.json" --seed "$seed" --steps "$steps" --from-step "$from" \
    --max-hours "${SOAK_MAX_HOURS:-8}" 2>&1 | tee -a "$EVID/soak.out"
  rc=${PIPESTATUS[0]}
  return "$rc"
}
