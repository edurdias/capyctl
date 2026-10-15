# shellcheck shell=bash
# LC1 (ADR 0029, design note "Testing"): `engine add` lists llama.cpp 0.6.0 with
# the digest and commit, on a GB10 lab host (owner approval 2026-10-14):
#   run_row.sh LC1 --no-e0 -- a
#
# Expected:
#   a  preconditions: llama-server v0.6.0 (d812350) built by
#      scripts/live/llamacpp-build.sh, its --version line and the sha256 of the
#      binary and its libraries recorded; the GGUF in place.
#   b  a standalone role without CAPYCTL_LLAMACPP_BIN; `engine add <llama-server>`
#      prints `Registered llamacpp (llamacpp 0.6.0)` (its wall time includes the
#      version check, CUDA initialisation included: live check 10).
#   c  `engine list` shows `llamacpp`, version 0.6.0, build fingerprint
#      `0.6.0+d812350...`, custom yes, deep park disabled.
#   d  a deployment on the registered profile reaches Ready and answers; deleted
#      with --stop; the role stops and engines.yaml is removed.
# shellcheck source=scripts/live/matrix/llamacpp_rows.sh
. "$MATRIX_DIR/llamacpp_rows.sh"

lc1_list_check() { # lc1_list_check <host>
  lc "$1" engine list --format json | tee "$EVID/lc1-engine-list.json"
  dry && return 0
  python3 - "$EVID/lc1-engine-list.json" "$LC_VERSION" "$LC_COMMIT" <<'PY'
import json, sys
line = [l for l in open(sys.argv[1]).read().splitlines() if l.startswith("{")][-1]
rows = {r["profile"]: r for r in json.loads(line)["engines"]}
row = rows.get("llamacpp", {})
print(json.dumps(row, indent=1))
text = json.dumps(row)
ok = sys.argv[2] in text and ("+" + sys.argv[3]) in text
sys.exit(0 if ok else 1)
PY
}

row_main() {
  local host rc=0
  host=$(resolve_host "${1:-a}") || return 1
  lc_begin "$host" || return 1
  LC_NO_ENV_BIN=1 step start lc_start "$host" || rc=1
  step engine-add timed engine-add lc "$host" engine add "$LC_BIN" || rc=1
  step engine-list lc1_list_check "$host" || rc=1
  step engine-detect lc "$host" engine detect --format json
  step doc lc_doc lc1-qwen3-4b llamacpp || rc=1
  step deploy timed deploy-ready lc_deploy "$host" lc1-qwen3-4b || rc=1
  step status lc_status "$host" lc1-qwen3-4b
  step infer lc_probe "$host" chat --route lc1-qwen3-4b --expect 42 || rc=1
  step delete lc_delete "$host" lc1-qwen3-4b || rc=1
  sleep 3
  step no-engine lc_no_engine "$host" || rc=1
  lc_end "$host" || rc=1
  return "$rc"
}
