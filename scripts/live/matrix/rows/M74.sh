# shellcheck shell=bash
# M74 (T14, T20): the Initialize deadline is the deployment's `timeouts.initialize`
# (ADR 0014 amendment A1), per fixture:
#   run_row.sh M74 --tag va-14 -- va-14
#   run_row.sh M74 --tag sa-14 -- sa-14
# Replaces the removed in-process scenario live_vllm.rs L9 (owner decision
# 2026-09-22). L9 could only assert an admission refusal, because the readiness
# bound was not reachable from a deployment document; `timeouts.initialize` and
# `--initialize-timeout` now make it so.
#
# Setup: server and the fixture's host enrolled and online; no other deployment
# active on that host. The fixture must take longer than 30 s to become Ready
# (q14 by default); if it is Ready inside 30 s the row is void, not a pass.
#
#   a  variant <fixture>-itover declares `timeouts.initialize` above its request
#      deadline (3600 s against 900 s): `deploy` is refused before anything is
#      recorded (`status deployment` then finds nothing).
#   b  variant <fixture>-it30 declares `timeouts.initialize: 30s` (the floor) and
#      deploys without activation; `inspect deployment --effective-config`
#      records 30 000 ms with provenance `declared`.
#   c  `start deployment <dep> --initialize-timeout 2h` is refused (beyond the
#      request deadline) before anything is sent.
#   d  `start deployment <dep>`: the Initialize expires at 30 s while the engine
#      loads. The launch is terminated and the deployment reads closed, never
#      uncertain: admission shut, no binding, charge, claim or lease, the failed
#      operation recorded, no engine or GPU process left on the host.
#   e  `start deployment <dep> --initialize-timeout 10m`: the override wins over
#      the declared 30 s; Ready, a correct routed answer, stop with verified
#      cleanup; then the variant is deleted.

M74_SETTLE=300

deploy_as() { local tag=$1; shift; FIXTURE_VARIANT=$tag deploy "$@"; }

effective_timeouts() { # effective_timeouts <deployment>
  # The server role does not implement the effective-config view (found live
  # 2026-09-23); status carries the same timeouts block.
  dry && { status_dep "$1" >/dev/null; return 0; }
  status_dep "$1" >"$EVID/effective-$1.json" || return 1
  python3 - "$EVID/effective-$1.json" <<'PY'
import json, sys
def timeouts(node):
    if isinstance(node, dict):
        block = node.get("timeouts")
        if isinstance(block, dict) and "initialize_ms" in block:
            return block
        node = list(node.values())
    if isinstance(node, list):
        for value in node:
            found = timeouts(value)
            if found is not None:
                return found
    return None
t = timeouts(json.load(open(sys.argv[1]))) or {}
print(json.dumps({"timeouts": t}))
ok = t.get("initialize_ms") == 30000 and (t.get("provenance") or {}).get("initialize") == "declared"
sys.exit(0 if ok else 1)
PY
}

# d: the start's own operation must fail; Ready inside the declared window voids the row.
expire_start() { # expire_start <deployment>
  local dep=$1 receipt op out
  if dry; then start_dep "$dep" >/dev/null; wait_operation "<operation_id>" "$M74_SETTLE"; return 0; fi
  receipt=$(start_dep "$dep") || { echo "$receipt"; return 1; }
  echo "$receipt"
  op=$(printf '%s' "$receipt" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("operation_id",""))')
  [ -n "$op" ] || { echo "no operation id in the start receipt"; return 1; }
  out=$(wait_operation "$op" "$M74_SETTLE") || { echo "$out"; return 1; }
  echo "$out"
  case $out in *" succeeded"*) echo "VOID: Ready inside the declared 30 s; use a larger fixture"; return 1 ;; esac
}

row_main() {
  local fix=${1:?fixture, e.g. va-14} host dep rc=0
  host=$(fixture_host "$fix")
  dep=$fix-it30
  echo "fixture $fix deployment $dep host $host" | tee -a "$EVID/timeline.txt"
  step before host_idle "$host" || return 1

  # a
  step variant-over variant "$fix" itover --document-json '{"request_deadline": "900s", "timeouts": {"initialize": "3600s"}}' || rc=1
  step over-deploy refused deploy_as itover "$fix" || rc=1
  step over-absent refused status_dep "$fix-itover" || rc=1

  # b
  step variant-30 variant "$fix" it30 --document-json '{"timeouts": {"initialize": "30s"}}' || return 1
  step deploy deploy_as it30 "$fix" || return 1
  step effective effective_timeouts "$dep" || rc=1

  # c
  step override-over refused start_dep "$dep" --initialize-timeout 2h || rc=1

  # d
  step expire expire_start "$dep" || rc=1
  step status status_dep "$dep"
  step closed closure_check "$dep" "$host" expired || rc=1
  step errors engine_errors "$host"

  # e
  step override start_dep "$dep" --initialize-timeout 10m || rc=1
  step ready wait_state "$dep" ready 1200 || rc=1
  step owned owned "$dep"
  dry || accounting "$dep" >"$EVID/accounting-ready.json"
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}
