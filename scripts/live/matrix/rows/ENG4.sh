# shellcheck shell=bash
# ENG4 (ADR 0018 §3, owner decision 2026-09-25): engine add beside an rc.3 agent, and a
# new agent against an rc.3 server:
#   MLLM_RC3_LOCAL=~/rc3/mllm MLLM_RC3_REMOTE=~/rc3/mllm run_row.sh ENG4 --no-e0 -- b
#
# Preconditions (checked, never installed or downloaded): MLLM_RC3_LOCAL
# (control-host) and MLLM_RC3_REMOTE (on the host) are existing rc.3 binaries whose
# `--version` prints 0.1.0-rc.3.
#
# Expected:
#   a  rc.3 host role, new CLI binary: engine add exits 22 agent_unreachable
#      and engines.yaml is written (revision 1). An rc.3 agent never reads
#      engines.yaml, so after an rc.3 restart the profile is still not
#      published; after the host role is upgraded to the new binary it is
#      published at start (list hosts).
#   b  rc.3 server, new host role: engine add exits 0 with
#      published=restart_required; after a host restart it is published.
#   c  both sides are returned to the new binaries.
. "$MATRIX_DIR/rows/ENG1.sh"

eng4_preconditions() {
  local host=$1
  [ -n "${MLLM_RC3_LOCAL:-}" ] && [ -n "${MLLM_RC3_REMOTE:-}" ] || { echo "set MLLM_RC3_LOCAL and MLLM_RC3_REMOTE"; return 1; }
  dry && return 0
  "$MLLM_RC3_LOCAL" --version | grep -q '0.1.0-rc.3' || { echo "MLLM_RC3_LOCAL is not rc.3"; return 1; }
  rsh "$host" "$MLLM_RC3_REMOTE --version | grep -q '0.1.0-rc.3'" || { echo "MLLM_RC3_REMOTE is not rc.3"; return 1; }
}

eng4_host_has() { # eng4_host_has <hosts.json> <host> <profile>: that host's entry names the profile
  python3 - "$@" <<'PY2'
import json, sys
hosts = [h for h in json.load(open(sys.argv[1]))["hosts"] if h["name"] == sys.argv[2]]
found = bool(hosts) and f'"{sys.argv[3]}"' in json.dumps(hosts[0])
print(json.dumps({"host": sys.argv[2], "profile": sys.argv[3], "listed": found}))
sys.exit(0 if found else 1)
PY2
}

eng4_rc3_agent() { # (a)
  local host=$1 short
  short=$(host_short "$host")
  "$MATRIX_DIR/roles.sh" host-down "$host" && rsh "$host" "rm -f $RRD/engines.yaml" &&
    "$MATRIX_DIR/roles.sh" host-doc-bare "$host" "${POLICY:-normal}" || return 1
  env "MLLM_REMOTE_BIN_$short=$MLLM_RC3_REMOTE" "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  refused_with agent_unreachable rsh "$host" "$RBIN engine add $(vllm_venv "$host")/bin/vllm --config $RRD/host.yaml" || return 1
  "$MATRIX_DIR/roles.sh" host-down "$host" &&
    env "MLLM_REMOTE_BIN_$short=$MLLM_RC3_REMOTE" "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  # rc.3 ignores engines.yaml: not published.
  # Only this host's entry counts: the other host's full document declares vllm.
  if ! dry; then
    cli list hosts --output json >"$EVID/rc3-agent-hosts.json" || return 1
    if eng4_host_has "$EVID/rc3-agent-hosts.json" "$host" vllm; then
      echo "UNEXPECTED: an rc.3 agent published engines.yaml"; return 1
    fi
  fi
  "$MATRIX_DIR/roles.sh" host-down "$host" && "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  dry && return 0
  cli list hosts --output json >"$EVID/upgraded-agent-hosts.json" && eng4_host_has "$EVID/upgraded-agent-hosts.json" "$host" vllm
}

eng4_rc3_server() ( # (b) a subshell: every server-side command uses the rc.3 binary
  local host=$1 out
  "$MATRIX_DIR/roles.sh" down || true
  export MLLM_LOCAL_BIN=$MLLM_RC3_LOCAL
  "$MATRIX_DIR/roles.sh" up "${POLICY:-normal}" || return 1
  # `up` started a new run: its run root, server document and binary.
  load_run
  "$MATRIX_DIR/roles.sh" host-down "$host" && rsh "$host" "rm -f $RRD/engines.yaml" &&
    "$MATRIX_DIR/roles.sh" host-doc-bare "$host" "${POLICY:-normal}" &&
    "$MATRIX_DIR/roles.sh" host-up "$host" && "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  out=$(eng_remote "$host" add "$(vllm_venv "$host")/bin/vllm") || return 1
  printf '%s\n' "$out" | tee "$EVID/rc3-server-add.json"
  dry || printf '%s\n' "$out" | grep -q '"published":"restart_required"' || return 1
  "$MATRIX_DIR/roles.sh" host-down "$host" && "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  dry && return 0
  cli list hosts --output json >"$EVID/rc3-server-hosts.json" && eng4_host_has "$EVID/rc3-server-hosts.json" "$host" vllm
)

row_main() {
  local host
  host=$(resolve_host "$1") || return 1
  local rc=0
  step preconditions eng4_preconditions "$host" || return 1
  step rc3-agent eng4_rc3_agent "$host" || rc=1
  step rc3-server eng4_rc3_server "$host" || rc=1
  step restore-engines rsh "$host" "rm -f $RRD/engines.yaml" || true
  step restore-server "$MATRIX_DIR/roles.sh" down || true
  step restore-up "$MATRIX_DIR/roles.sh" up "${POLICY:-normal}" || rc=1
  return "$rc"
}
