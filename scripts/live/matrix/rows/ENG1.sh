# shellcheck shell=bash
# ENG1 (ADR 0018 §1, §3): engine add of the existing vLLM and SGLang
# environments on a host running under systemd, published live, then a
# deployment on each new profile serves:
#   run_row.sh ENG1 --tag a -- a va-4 sa-4
#   run_row.sh ENG1 --tag b -- b vb-4 sb-4
#
# Expected:
#   a  engine detect (no --path) lists both home-level environments
#      (metadata only; owner decision 2026-09-25).
#   a2 host.yaml is byte-identical before and after the adds; the profiles
#      live in $RRD/engines.yaml beside it.
#   b  engine add of each exits 0 with published=published within 10 s.
#   c  list engines on the server shows vllm and sglang on the host, custom=false.
#   d  the standard fixtures (profiles vllm and sglang, revision 1) reach
#      Ready and answer; stop with verified cleanup; deleted.
#   e  the host is returned to its tmux role and full document.

eng_remote() { # eng_remote <host> <engine args...>: run `mllm engine` on the host
  local host=$1
  shift
  rsh "$host" "$(rbin "$host") engine $* --config $RRD/host.yaml"
}

eng_published() { # eng_published <json file>: add answered published
  dry && return 0
  python3 - "$1" <<'PY'
import json, sys
line = [l for l in open(sys.argv[1]).read().splitlines() if l.startswith("{")][-1]
reply = json.loads(line)
print(reply.get("published"), reply.get("version"), reply.get("custom"))
sys.exit(0 if reply.get("published") == "published" and reply.get("custom") is False else 1)
PY
}

eng_bare_systemd() { # eng_bare_systemd <host>: profile-less document, systemd role
  local host=$1
  "$MATRIX_DIR/roles.sh" host-down "$host" &&
    rsh "$host" "rm -f $RRD/engines.yaml $RRD/engines.yaml.lock" &&
    "$MATRIX_DIR/roles.sh" host-doc-bare "$host" "${POLICY:-normal}" &&
    "$MATRIX_DIR/roles.sh" host-up-systemd "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180
}

eng_restore() { # eng_restore <host>: back to the tmux role and full document
  local host=$1
  "$MATRIX_DIR/roles.sh" host-down-systemd "$host" || true
  # The full matrix document declares vllm and sglang itself; the same names
  # in engines.yaml would be refused at start.
  rsh "$host" "rm -f $RRD/engines.yaml $RRD/engines.yaml.lock" || return 1
  "$MATRIX_DIR/roles.sh" host-doc "$host" "${POLICY:-normal}" &&
    "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180
}

eng_add() { # eng_add <host> <path>
  local host=$1 path=$2 out
  out="$EVID/add-$(basename "$(dirname "$(dirname "$path")")")-$host.json"
  dry && { eng_remote "$host" add "$path"; return 0; }
  eng_remote "$host" add "$path" | tee "$out" && eng_published "$out"
}

eng_listed() { # eng_listed <host> <profile...>: list engines shows them
  local host=$1
  shift
  cli list engines --format json >"$EVID/engines.json" || return 1
  dry && return 0
  python3 - "$EVID/engines.json" "$host" "$@" <<'PY'
import json, sys
rows = json.load(open(sys.argv[1]))["engines"]
host, wanted = sys.argv[2], sys.argv[3:]
have = {r["profile"]: r for r in rows if r["host"] == host}
missing = [p for p in wanted if p not in have or have[p]["custom"]]
print({p: (have[p]["version"], have[p]["custom"]) for p in have})
sys.exit(1 if missing else 0)
PY
}

eng_serves() { # eng_serves <fixture>
  local fix=$1 host dep rc=0
  host=$(fixture_host "$fix"); dep=$fix
  step "deploy-$fix" deploy "$fix" --activate --wait || return 1
  step "owned-$fix" keep_owned "$dep" ready
  step "infer-$fix" infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step "stop-$fix" stop_dep "$dep" || rc=1
  step "stopped-$fix" wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step "cleanup-$fix" cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step "delete-$fix" delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() {
  local host
  host=$(resolve_host "$1") || return 1
  local vfix=$2 sfix=$3 rc=0
  step before host_idle "$host" || return 1
  step bare-systemd eng_bare_systemd "$host" || return 1
  step host-yaml-before rsh "$host" "sha256sum $RRD/host.yaml > $RRD/host-yaml.sum" || rc=1
  step detect eng_remote "$host" detect || rc=1
  step add-vllm timed add-vllm eng_add "$host" "$(vllm_venv "$host")/bin/vllm" || rc=1
  step add-sglang timed add-sglang eng_add "$host" "$SGLANG_VENV/bin/python3" || rc=1
  step listed eng_listed "$host" vllm sglang || rc=1
  step host-yaml-unchanged rsh "$host" "sha256sum -c $RRD/host-yaml.sum" || rc=1
  step engines-file rsh "$host" "head -1 $RRD/engines.yaml" || rc=1
  eng_serves "$vfix" || rc=1
  eng_serves "$sfix" || rc=1
  step restore eng_restore "$host" || rc=1
  return "$rc"
}
