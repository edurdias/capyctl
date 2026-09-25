# shellcheck shell=bash
# ENG2 (ADR 0018 §5): standalone with MLLM_VLLM_BIN and MLLM_SGLANG_BIN both
# set (refused before) publishes local-vllm and local-sglang, and serves a
# deployment on each in turn:
#   run_row.sh ENG2 --no-e0 -- host-a
#
# Expected:
#   a  the role starts; engine list shows local-vllm and local-sglang,
#      published.
#   b  engine add of nothing new is not needed; engine remove local-vllm is
#      refused (environment profile).
#   c  the role stops cleanly; its private state directory is removed.
# The host's matrix role is stopped for the row and restarted after it.

ENG2_DIR=
eng2_start() { # eng2_start <host>
  local host=$1
  ENG2_DIR=$RRD/eng2-standalone
  rsh "$host" "rm -rf $ENG2_DIR && mkdir -m 700 $ENG2_DIR && tmux new-session -d -s mx-eng2-$RUN \
env MLLM_STATE_DIR=$ENG2_DIR MLLM_VLLM_BIN=$(vllm_venv "$host")/bin/vllm MLLM_SGLANG_BIN=$SGLANG_VENV/bin/python3 \
MLLM_MODELS_ROOT=$MODELS_ROOT $REMOTE_TREE/scripts/live/matrix/role_exec.sh $ENG2_DIR/role.pid $ENG2_DIR/role.log \
$(rbin "$host") start standalone"
  rsh "$host" "for i in \$(seq 1 120); do [ -S $ENG2_DIR/control.sock ] && exit 0; sleep 1; done; exit 1"
}

eng2_list() { # eng2_list <host>
  local host=$1
  rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") engine list" | tee "$EVID/eng2-list.json"
  dry && return 0
  python3 - "$EVID/eng2-list.json" <<'PY'
import json, sys
line = [l for l in open(sys.argv[1]).read().splitlines() if l.startswith("{")][-1]
rows = {r["profile"]: r for r in json.loads(line)["engines"]}
ok = all(rows.get(p, {}).get("published") == "published" for p in ("local-vllm", "local-sglang"))
print(sorted(rows))
sys.exit(0 if ok else 1)
PY
}

eng2_stop() { # eng2_stop <host>
  local host=$1
  rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile $ENG2_DIR/role.pid --expect 'start standalone' --signal TERM; \
sleep 10; rm -rf $ENG2_DIR"
}

row_main() {
  local host=$1 rc=0
  step before host_idle "$host" || return 1
  step matrix-host-down "$MATRIX_DIR/roles.sh" host-down "$host" || return 1
  step start eng2_start "$host" || { rc=1; }
  step list eng2_list "$host" || rc=1
  step env-remove-refused refused_with invalid_config rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") engine remove local-vllm" || rc=1
  step stop eng2_stop "$host" || rc=1
  step idle host_idle "$host" || rc=1
  step matrix-host-up "$MATRIX_DIR/roles.sh" host-up "$host" || rc=1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  return "$rc"
}

# Serving a deployment on each standalone profile uses the standalone client's
# `deploy model --file` with a document naming `runtime_profile: local-vllm` or
# `local-sglang`; add those two steps once Task 15's CPU tests have fixed the
# standalone deployment shape, generating the files with
# `gen_deployment.py --document-json '{"runtime_profile":"local-vllm","placement":null}'`
# and deploying with
# `MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") deploy model --file <file> --activate --wait`.
