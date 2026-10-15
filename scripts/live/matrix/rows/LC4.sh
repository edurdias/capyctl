# shellcheck shell=bash
# LC4 (ADR 0029, design note "Testing") on a GB10 lab host: a llama.cpp
# deployment and a vLLM deployment switch under memory pressure, in one
# standalone role with both engines (CAPYCTL_LLAMACPP_BIN and CAPYCTL_VLLM_BIN,
# the host's owner-approved vLLM 0.30.0 environment):
#   run_row.sh LC4 --no-e0 -- a [managed limit, default 40GiB]
#
# The managed limit is chosen so that each deployment fits alone and the two do
# not fit together (both requests are read from status `startup.bytes` and checked;
# live 2026-10-15 on GB10: va-4 38894047072 B by default before its first start,
# 27111981056 B once measured, llama.cpp 17261230336 B measured; at 44GiB the
# measured pair fitted and nothing was parked, so 40GiB). A parked vLLM keeps its
# parked residual.
#
# Expected:
#   a  the llama.cpp deployment is Ready and answers.
#   b  the vLLM deployment (va-4 as ENG2 deploys it, profile local-vllm) is
#      deployed and activated; a request to it makes room: llama.cpp is stopped
#      (restart-only), vLLM reaches Ready and answers.
#   c  a request to llama.cpp parks vLLM and relaunches llama.cpp (a new
#      generation), which answers; then a request to vLLM again wakes vLLM.
#   d  states, timings and memory recorded between each switch; both deleted with
#      --stop; nothing of either engine left.
# shellcheck source=scripts/live/matrix/llamacpp_rows.sh
. "$MATRIX_DIR/llamacpp_rows.sh"

# lc4_fits <host> <llama dep> <vllm dep> <limit bytes>: each request below the
# limit, their sum above it (else the row proves nothing).
lc4_fits() {
  local host=$1 a=$2 b=$3 limit=$4
  lc_status "$host" "$a" >"$EVID/lc4-status-$a.json"
  lc_status "$host" "$b" >"$EVID/lc4-status-$b.json"
  dry && return 0
  python3 - "$EVID/lc4-status-$a.json" "$EVID/lc4-status-$b.json" "$limit" <<'PY'
import json, re, sys
def request(path):
    text = open(path).read()
    doc = json.loads(text[text.index("{"):])
    found = []
    def walk(node, key=""):
        if isinstance(node, dict):
            for k, v in node.items(): walk(v, k)
        elif isinstance(node, list):
            for v in node: walk(v, key)
        elif key in ("request_bytes", "bytes") and isinstance(node, int):
            found.append(node)
    walk(doc)
    return max(found) if found else None
a, b, limit = request(sys.argv[1]), request(sys.argv[2]), int(sys.argv[3])
ok = a is not None and b is not None and a <= limit and b <= limit and a + b > limit
print(json.dumps({"llamacpp_request": a, "vllm_request": b, "managed_limit": limit, "pressure": ok}))
sys.exit(0 if ok else 1)
PY
}

lc4_states() { # lc4_states <host> <deps...>
  local host=$1 d
  shift
  for d in "$@"; do
    lc "$host" status deployment "$d" --format json | python3 -c 'import json,sys
t=sys.stdin.read(); d=json.loads(t[t.index("{"):]); d=d.get("deployment",d)
out={k: d.get(k) for k in ("name","observed_state","desired_state","generation")}
s=d.get("startup") or {}; out["startup_bytes"]=s.get("bytes"); out["startup_provenance"]=s.get("provenance")
out["instances"]=[{k: i.get(k) for k in ("generation","lifecycle","observed_state","reservation_owner")} for i in d.get("instances") or []]
print(json.dumps(out))' || true
  done
}

lc4_vllm_doc() { # lc4_vllm_doc <host>: va-4 for the standalone local-vllm profile
  local host=$1 file
  variant va-4 lc4 --document-json '{"runtime_profile": "local-vllm", "host": null}' --engine-config-json '{"context_length": null}' || return 1
  file=$(FIXTURE_VARIANT=lc4 fixture_file va-4)
  rcopy "$file" "$host:$LC_DIR/va-4-lc4.yaml"
}

row_main() {
  local host rc=0 limit=${2:-40GiB} limit_bytes llama=lc4-qwen3-4b vllm=va-4-lc4
  host=$(resolve_host "${1:-a}") || return 1
  limit_bytes=$(python3 -c 'import sys; v=sys.argv[1]; print(int(v[:-3]) << 30 if v.endswith("GiB") else int(v))' "$limit")
  lc_begin "$host" vllm || return 1
  step start lc_start "$host" "CAPYCTL_VLLM_BIN=$(vllm_venv "$host")/bin/vllm" -- \
    --set "host.resource_policy.memory.system.managed_limit=$limit" || rc=1
  step engine-list lc "$host" engine list --format json || rc=1
  step sampler lc_sampler_start "$host" lc4
  step doc-llama lc_doc "$llama" local-llamacpp || rc=1
  step deploy-llama timed ready-llama-1 lc_deploy "$host" "$llama" || rc=1
  step infer-llama-1 lc_probe "$host" chat --route "$llama" --expect 42 || rc=1
  step doc-vllm lc4_vllm_doc "$host" || rc=1
  step deploy-vllm lc "$host" deploy model --file "$LC_DIR/$vllm.yaml" --format json || rc=1
  if ! step pressure lc4_fits "$host" "$llama" "$vllm" "$limit_bytes"; then
    step delete-vllm lc_delete "$host" "$vllm"
    step delete-llama lc_delete "$host" "$llama"
    step memory lc_sampler_stop "$host" lc4
    lc_end "$host"
    return 1
  fi
  step activate-vllm lc "$host" start deployment "$vllm" --format json
  step states-0 lc4_states "$host" "$llama" "$vllm"
  step mark-0 lc_mark "$host" before-switch
  step switch-to-vllm timed switch-to-vllm lc_probe "$host" chat --route "$vllm" --expect 42 --timeout 1200 || rc=1
  step states-1 lc4_states "$host" "$llama" "$vllm"
  step mark-1 lc_mark "$host" vllm-serving
  step switch-to-llama timed switch-to-llama lc_probe "$host" chat --route "$llama" --expect 42 --timeout 1200 || rc=1
  step states-2 lc4_states "$host" "$llama" "$vllm"
  # (c): vLLM made room for llama.cpp, so it is no longer Ready.
  if ! dry && ! grep -q '"name": "va-4-lc4", "observed_state": "parked"' "$EVID/$(printf %02d "$STEP")-states-2.out"; then
    echo "LC4: vLLM not parked after the switch to llama.cpp" >&2; rc=1
  fi
  step mark-2 lc_mark "$host" llama-serving
  step switch-to-vllm-2 timed switch-to-vllm-2 lc_probe "$host" chat --route "$vllm" --expect 42 --timeout 1200 || rc=1
  step states-3 lc4_states "$host" "$llama" "$vllm"
  step errors lc_engine_lines "$host"
  step delete-vllm lc_delete "$host" "$vllm" || rc=1
  step delete-llama lc_delete "$host" "$llama" || rc=1
  sleep 10
  step memory lc_sampler_stop "$host" lc4 || rc=1
  step no-engine lc_no_engine "$host" || rc=1
  lc_end "$host" || rc=1
  return "$rc"
}
