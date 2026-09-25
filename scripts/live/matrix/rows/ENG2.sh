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
#   c  a deployment on each environment profile (`runtime_profile:
#      local-vllm`, `local-sglang`) reaches Ready through the standalone
#      client and answers, then is deleted with --stop.
#      The environment vLLM profile carries the host-fixed `--max-model-len
#      4096` default, so its deployment states no context length (one that
#      does is refused invalid_config, "already set `--max-model-len`").
#   d  `engine add` of the vLLM environment under another name (`vllm-reg`)
#      publishes it beside the environment profiles. A registered profile
#      carries no arguments, so no `--max-model-len` default: a deployment
#      stating its context length (16384) reaches Ready and answers; one with
#      no context length is recorded (the engine's own limit applies), with
#      the engine's reason from its private log. Both deleted with --stop.
#   e  the role stops cleanly; its private state directory and the
#      engines.yaml the add wrote are removed.
# The host's matrix role is stopped for the row and restarted after it.

ENG2_DIR=
eng2_start() { # eng2_start <host>
  local host=$1
  ENG2_DIR=$RRD/eng2-standalone
  rsh "$host" "rm -rf $ENG2_DIR && mkdir -m 700 $ENG2_DIR && tmux new-session -d -s mx-eng2-$RUN \
env MLLM_STATE_DIR=$ENG2_DIR MLLM_VLLM_BIN=$(vllm_venv "$host")/bin/vllm MLLM_SGLANG_BIN=$SGLANG_VENV/bin/python3 \
MLLM_MODELS_ROOT=$MODELS_ROOT $REMOTE_TREE/scripts/live/matrix/role_exec.sh $ENG2_DIR/role.pid $ENG2_DIR/role.log \
$(rbin "$host") start standalone --debug-engine-logs"
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


# The standalone role's config home engines.yaml (ADR 0018 §5) must not exist
# before the row: the row writes it and removes it.
eng2_config_home_free() { rsh "$1" "! test -e \$HOME/.config/mllm/engines.yaml"; }

eng2_serve() { # eng2_serve <host> <fixture> <tag> <profile> [gen_deployment args...]
  local host=$1 fix=$2 tag=$3 profile=$4 dep file
  shift 4
  dep=$fix-$tag
  variant "$fix" "$tag" --document-json "{\"runtime_profile\": \"$profile\", \"host\": null}" "$@" || return 1
  file=$(FIXTURE_VARIANT=$tag fixture_file "$fix")
  rcopy "$file" "$host:$ENG2_DIR/$dep.yaml" || return 1
  rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR timeout 1200 $(rbin "$host") deploy model --file $ENG2_DIR/$dep.yaml --activate --wait --output json | tail -c 1500; echo; \
MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") status deployment $dep --output json | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get(\"deployment\",d)
print(json.dumps({k: d.get(k) for k in (\"name\", \"observed_state\", \"conditions\")}))'"
}

eng2_infer() { # eng2_infer <host> <route>: one request through the standalone router
  rsh "$1" "KEY=\$(sed -n 's/^api_key: //p' $ENG2_DIR/identity/credentials); curl -s -m 120 -H \"Authorization: Bearer \$KEY\" -H 'Content-Type: application/json' \
http://127.0.0.1:8443/v1/chat/completions -d '{\"model\":\"$2\",\"messages\":[{\"role\":\"user\",\"content\":\"What is 17+25? Answer with only the number.\"}],\"max_tokens\":1024,\"temperature\":0}' \
| python3 -c 'import json,sys; c=json.load(sys.stdin)[\"choices\"][0][\"message\"][\"content\"]; print(repr(c[-200:])); sys.exit(0 if \"42\" in c else 1)'"
}

eng2_engine_reason() { # eng2_engine_reason <host>: context-length lines of the private engine logs
  rsh "$1" "grep -rhaiE 'max_model_len|max model len|model length|KV cache' $ENG2_DIR/logs 2>/dev/null | grep -viE 'key|token=|secret|bearer' | tail -n 6; true"
}

eng2_delete() { rsh "$1" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$1") delete deployment $2 --stop --output json"; }

eng2_argv() { # eng2_argv <host>: the running vLLM api process's argv tail (no credential is on argv)
  rsh "$1" "for p in \$(pgrep -f 'vllm_entr[y]'); do tr '\\0' ' ' < /proc/\$p/cmdline | grep -o -- '--mllm-user-args.*' ; done; true"
}

row_main() {
  local host=$1 rc=0
  step before host_idle "$host" || return 1
  step config-home-free eng2_config_home_free "$host" || return 1
  step matrix-host-down "$MATRIX_DIR/roles.sh" host-down "$host" || return 1
  step start eng2_start "$host" || { rc=1; }
  step list eng2_list "$host" || rc=1
  step env-remove-refused refused_with invalid_config rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") engine remove local-vllm" || rc=1
  step serve-local-vllm eng2_serve "$host" v92-4 lv local-vllm --engine-config-json '{"context_length": null}' || rc=1
  step argv-local-vllm eng2_argv "$host"
  step infer-local-vllm eng2_infer "$host" v92-4-lv || rc=1
  step delete-local-vllm eng2_delete "$host" v92-4-lv || rc=1
  step serve-local-sglang eng2_serve "$host" s92-4 ls local-sglang || rc=1
  step infer-local-sglang eng2_infer "$host" s92-4-ls || rc=1
  step delete-local-sglang eng2_delete "$host" s92-4-ls || rc=1
  step add-registered rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") engine add $(vllm_venv "$host")/bin/vllm --name vllm-reg" || rc=1
  step list-registered rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") engine list" || rc=1
  step serve-registered eng2_serve "$host" v92-4 reg vllm-reg || rc=1
  step argv-registered eng2_argv "$host"
  step infer-registered eng2_infer "$host" v92-4-reg || rc=1
  step delete-registered eng2_delete "$host" v92-4-reg || rc=1
  # No context length and no --max-model-len default: the model's own limit.
  step serve-registered-noctx eng2_serve "$host" v92-4 regnc vllm-reg --engine-config-json '{"context_length": null}'
  step argv-registered-noctx eng2_argv "$host"
  step infer-registered-noctx eng2_infer "$host" v92-4-regnc
  step reason-registered-noctx eng2_engine_reason "$host"
  step delete-registered-noctx eng2_delete "$host" v92-4-regnc || rc=1
  step stop eng2_stop "$host" || rc=1
  step engines-file-removed rsh "$host" "rm -f \$HOME/.config/mllm/engines.yaml \$HOME/.config/mllm/engines.yaml.lock" || rc=1
  step idle host_idle "$host" || rc=1
  step matrix-host-up "$MATRIX_DIR/roles.sh" host-up "$host" || rc=1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  return "$rc"
}

