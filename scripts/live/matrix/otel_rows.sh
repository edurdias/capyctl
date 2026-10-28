# shellcheck shell=bash disable=SC2034  # settings are used by the scripts that source this file
# Shared helpers of the ADR 0037 live rows OT1-OT11 (rows/OT*.sh), sourced by
# them after rowlib.sh. Every row but OT6 stops the host's matrix role and runs a
# standalone role on the host with its own state and config directories (as the
# LC and ENG2 rows do), restarting the matrix role at the end. OT6 runs through
# the matrix server and host roles.
#
# The approved environments (ADR 0037 "Live qualification"), in the home
# directory on each host and named in hosts.local.env: vLLM 0.32.0
# (OT_VLLM_VENV_DIR), SGLang 0.5.22 (OT_SGLANG_VENV_DIR) and llama.cpp v0.7.0
# (OT_LLAMACPP_DIR, built by scripts/live/llamacpp-build.sh). Models under the
# host's models directory: Qwen3-4B for chat on each engine (OT_CHAT_MODEL, and
# its Q4_K_M GGUF in OT_GGUF_MODEL for llama.cpp) and Clef-Flash for decisions on
# SGLang (OT_DECISION_MODEL). The standalone role registers them with `engine
# add` as v32, s22 and lc into its own XDG_CONFIG_HOME, so ~/.config/capyctl is
# never written. This tree must be synced to the host (sync.sh): the rows run
# otel_probe.py there.
#
# The rows record evidence; whether a row passed is judged from it against the
# ADR's table. otel_spans.py writes each row's summaries (spans-<label>.json).
# shellcheck source=scripts/live/matrix/otel_collector.sh
. "$MATRIX_DIR/otel_collector.sh"

OT_VLLM_VENV=$REMOTE_HOME/${OT_VLLM_VENV_DIR:-vllm-0.32.0-venv}
OT_SGLANG_VENV=$REMOTE_HOME/${OT_SGLANG_VENV_DIR:-sglang-0.5.22-venv}
OT_LC_BIN=$REMOTE_HOME/${OT_LLAMACPP_DIR:-llamacpp-v0.7.0}/build/bin/llama-server
OT_LC_COMMIT=620d222
OT_CHAT_MODEL=${OT_CHAT_MODEL:-qwen3-4b-instruct}
OT_GGUF_MODEL=${OT_GGUF_MODEL:-Qwen3-4B-GGUF}
OT_GGUF=$MODELS_ROOT/$OT_GGUF_MODEL/Qwen3-4B-Q4_K_M.gguf
OT_DECISION_MODEL=${OT_DECISION_MODEL:-clef-flash}
OT_PROBE=$REMOTE_TREE/scripts/live/matrix/otel_probe.py
# The standalone role: its state directory is new at every start; its config
# directory (XDG_CONFIG_HOME, which holds engines.yaml) lives for the row.
OT_DIR=$RRD/ot-standalone
OT_STATE=$OT_DIR/state
OT_CFG=$OT_DIR/cfg
# Profile names the rows register (ot_engines).
OT_V=v32 OT_S=s22 OT_L=lc
OT_HOST=
OT_STARTS=0
OT_ROLE_LOG=
# Seconds to wait after the last request before stopping anything: CapyCTL
# exports every 5 s (§8), and so do the engines' batch span processors.
OT_EXPORT_WAIT_S=${OT_EXPORT_WAIT_S:-12}
# Role settings the rows combine (§12; standalone: the server block carries the
# endpoint, the host block only `engines`).
OT_SET_ON=(--set "server.observability.tracing.endpoint=$OT_ENDPOINT")
OT_SET_ENGINES=(--set host.observability.tracing.engines=on)
OT_SET_TIMING=(--set server.observability.timing_header=true)

# ot_pre <host> [vllm] [sglang] [llamacpp] [decisions]: the collector, the
# approved environments and models the row uses, and the probe. Fails early with
# the missing piece named; records versions, GPU and memory.
ot_pre() {
  local host=$1 need sglang_checked=0
  shift
  otelcol_pre "$host" || return 1
  local script="rc=0
[ -f $OT_PROBE ] || { echo 'PRECONDITION: $OT_PROBE missing: sync this tree to the host (sync.sh)' >&2; rc=1; }"
  for need in "$@"; do
    case $need in
      vllm) script+="
v=\$($OT_VLLM_VENV/bin/python3 -c 'import importlib.metadata as m; print(m.version(\"vllm\"))' 2>/dev/null); echo \"vllm: \${v:-none} ($OT_VLLM_VENV)\"
[ \"\$v\" = 0.32.0 ] || { echo 'PRECONDITION: no vLLM 0.32.0 environment at $OT_VLLM_VENV' >&2; rc=1; }
[ -d $MODELS_ROOT/$OT_CHAT_MODEL ] || { echo 'PRECONDITION: no $MODELS_ROOT/$OT_CHAT_MODEL' >&2; rc=1; }" ;;
      sglang|decisions)
        if [ "$sglang_checked" = 0 ]; then sglang_checked=1; script+="
v=\$($OT_SGLANG_VENV/bin/python3 -c 'import importlib.metadata as m; print(m.version(\"sglang\"))' 2>/dev/null); echo \"sglang: \${v:-none} ($OT_SGLANG_VENV)\"
[ \"\$v\" = 0.5.22 ] || { echo 'PRECONDITION: no SGLang 0.5.22 environment at $OT_SGLANG_VENV' >&2; rc=1; }"
        fi
        if [ "$need" = sglang ]; then script+="
[ -d $MODELS_ROOT/$OT_CHAT_MODEL ] || { echo 'PRECONDITION: no $MODELS_ROOT/$OT_CHAT_MODEL' >&2; rc=1; }"
        else script+="
[ -d $MODELS_ROOT/$OT_DECISION_MODEL ] || { echo 'PRECONDITION: no $MODELS_ROOT/$OT_DECISION_MODEL (Clef-Flash)' >&2; rc=1; }"
        fi ;;
      llamacpp) script+="
line=\$(timeout 60 $OT_LC_BIN --version 2>&1 >/dev/null | grep -m1 '^version: ' || true); echo \"llama-server: \${line:-none} ($OT_LC_BIN)\"
case \$line in *$OT_LC_COMMIT*) ;; *) echo 'PRECONDITION: no llama-server v0.7.0 ($OT_LC_COMMIT) at $OT_LC_BIN: run LLAMACPP_TAG=v0.7.0 scripts/live/llamacpp-build.sh source' >&2; rc=1 ;; esac
[ -s $OT_GGUF ] || { echo 'PRECONDITION: no $OT_GGUF' >&2; rc=1; }
[ \"\$(ls $MODELS_ROOT/$OT_GGUF_MODEL | wc -l)\" = 1 ] || { echo 'PRECONDITION: $MODELS_ROOT/$OT_GGUF_MODEL must hold one GGUF only' >&2; rc=1; }
[ ! -e /etc/llama.cpp/config.ini ] || { echo 'PRECONDITION: /etc/llama.cpp/config.ini exists' >&2; rc=1; }" ;;
      *) die "ot_pre: unknown need $need" ;;
    esac
  done
  script+="
nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
grep -E 'MemTotal|MemAvailable' /proc/meminfo
exit \$rc"
  rsh "$host" "$script"
}

# ot_begin <host> [needs...]: the common opening: the host's evidence directory,
# preconditions, an idle host, the matrix role down.
ot_begin() {
  local host=$1
  OT_HOST=$host
  step evidence-dir ot_hev_init "$host" || return 1
  step precondition ot_pre "$@" || return 1
  step before host_idle "$host" || return 1
  step matrix-host-down "$MATRIX_DIR/roles.sh" host-down "$host" || return 1
}

# ot_end <host>: the common closing: the standalone role stopped (its
# deployments deleted with --stop first), every collector stopped, the host's
# evidence fetched, the host idle, the matrix role back and online.
ot_end() {
  local host=$1 rc=0
  step stop-standalone ot_stop "$host" || rc=1
  otelcol_stop_all || rc=1
  step clear-standalone rsh "$host" "rm -rf $OT_DIR"
  step fetch ot_fetch "$host" || rc=1
  step idle host_idle "$host" || rc=1
  step matrix-host-up "$MATRIX_DIR/roles.sh" host-up "$host" || rc=1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  return "$rc"
}

# ot_start <host> [env assignments...] -- [start standalone flags...]: a
# standalone role on the host with a new state directory, the row's config
# directory and its role log in the host's evidence directory.
ot_start() {
  local host=$1 envs=() flags=()
  shift
  while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
  [ "${1:-}" = -- ] && shift
  flags=("$@")
  OT_STARTS=$((OT_STARTS + 1))
  OT_ROLE_LOG=$OT_HEV/role-$OT_STARTS.log
  rsh "$host" "mkdir -p -m 700 $OT_DIR && mkdir -p -m 700 $OT_CFG && rm -rf $OT_STATE && mkdir -m 700 $OT_STATE && \
$MX_TMUX new-session -d -s mx-ot-$OT_STARTS-$RUN env XDG_CONFIG_HOME=$OT_CFG CAPYCTL_STATE_DIR=$OT_STATE CAPYCTL_MODELS_ROOT=$MODELS_ROOT ${envs[*]} \
$REMOTE_TREE/scripts/live/matrix/role_exec.sh $OT_DIR/role.pid $OT_ROLE_LOG \
$(rbin "$host") start standalone ${flags[*]}" || return 1
  rsh "$host" "for i in \$(seq 1 120); do [ -S $OT_STATE/control.sock ] && exit 0; sleep 1; done; tail -n 30 $OT_ROLE_LOG; exit 1"
}

# ot_stop <host>: delete every deployment of the standalone role with --stop
# (verified cleanup), then SIGTERM the role by its recorded identity (it flushes
# its span queue for at most 2 s, §8) and wait for it to exit.
ot_stop() {
  local host=$1
  rsh "$host" "[ -f $OT_DIR/role.pid ] || { echo 'no standalone role running'; exit 0; }
for d in \$(XDG_CONFIG_HOME=$OT_CFG CAPYCTL_STATE_DIR=$OT_STATE timeout 60 $(rbin "$host") list deployments --format json 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d if isinstance(d,list) else d.get(\"deployments\",[])
print(\" \".join(x[\"name\"] for x in d if not x[\"name\"].startswith(\"deleted/\")))'); do
  XDG_CONFIG_HOME=$OT_CFG CAPYCTL_STATE_DIR=$OT_STATE timeout 900 $(rbin "$host") delete deployment \$d --stop --format json
done
python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile $OT_DIR/role.pid --expect 'start standalone' --signal TERM
read -r pid _ < $OT_DIR/role.pid
for i in \$(seq 1 60); do [ -d /proc/\$pid ] || { rm -f $OT_DIR/role.pid; echo 'standalone role stopped'; exit 0; }; sleep 1; done
echo 'standalone role still running after 60 s' >&2; exit 1"
}

# ot_cli <host> <capyctl args...>: the CLI against the row's standalone role.
ot_cli() { local host=$1; shift; rsh "$host" "XDG_CONFIG_HOME=$OT_CFG CAPYCTL_STATE_DIR=$OT_STATE timeout 1800 $(rbin "$host") $*"; }

# ot_add <host> <path> <profile> [engine add options...]: `engine add`, with
# CUDA_HOME set as `engine add` records it when the machine has a toolkit.
ot_add() {
  local host=$1 path=$2 name=$3
  shift 3
  rsh "$host" "if [ -x /usr/local/cuda/bin/nvcc ]; then export CUDA_HOME=/usr/local/cuda; fi; \
XDG_CONFIG_HOME=$OT_CFG CAPYCTL_STATE_DIR=$OT_STATE timeout 600 $(rbin "$host") engine add $path --name $name $*"
}

# ot_engines <host> [vllm] [sglang] [llamacpp]: register the approved
# environments the row uses, then `engine list`.
ot_engines() {
  local host=$1 e rc=0
  shift
  for e in "$@"; do
    case $e in
      vllm) step "add-$OT_V" ot_add "$host" "$OT_VLLM_VENV" "$OT_V" || rc=1 ;;
      sglang) step "add-$OT_S" ot_add "$host" "$OT_SGLANG_VENV" "$OT_S" || rc=1 ;;
      llamacpp) step "add-$OT_L" ot_add "$host" "$OT_LC_BIN" "$OT_L" || rc=1 ;;
      *) die "ot_engines: unknown engine $e" ;;
    esac
  done
  step engine-list ot_cli "$host" engine list --format json || rc=1
  return "$rc"
}

# ot_doc <name> <vllm|sglang|llamacpp|decisions> [yaml lines...]: a deployment
# document in the short form, copied to the host. Chat runs Qwen3-4B (8192
# tokens) on each engine, without LMCache on vLLM and SGLang so that no LMCache
# server joins the launch; decisions run Clef-Flash on SGLang. Extra lines are
# appended verbatim: two-space indented lines continue engine_config (chat
# documents end with it). OT_DOC_PROFILE overrides the profile.
ot_doc() {
  local name=$1 kind=$2 file line
  shift 2
  file=$EVID/$name.yaml
  {
    printf 'schema_version: 1\nkind: deployment\nname: %s\n' "$name"
    case $kind in
      vllm) printf 'engine: %s\nmodel: %s\nlmcache: off\nengine_config:\n  context_length: 8192\n' "${OT_DOC_PROFILE:-$OT_V}" "$OT_CHAT_MODEL" ;;
      sglang) printf 'engine: %s\nmodel: %s\nlmcache: off\nengine_config:\n  context_length: 8192\n' "${OT_DOC_PROFILE:-$OT_S}" "$OT_CHAT_MODEL" ;;
      llamacpp)
        printf 'engine: %s\nmodel: %s\ndevices: [{id: gpu0, sharing: shared}]\n' "${OT_DOC_PROFILE:-$OT_L}" "$OT_GGUF_MODEL"
        printf 'engine_config:\n  context_length: 8192\n  max_concurrent_requests: 4\n  kv_cache_dtype: f16\n  llamacpp:\n    n_gpu_layers: all\n' ;;
      decisions) printf 'engine: %s\nmodel: %s\napi: decisions\n' "${OT_DOC_PROFILE:-$OT_S}" "$OT_DECISION_MODEL" ;;
      *) die "ot_doc: unknown kind $kind" ;;
    esac
    for line in "$@"; do printf '%s\n' "$line"; done
  } >"$file"
  cat "$file"
  rcopy "$file" "$OT_HOST:$OT_DIR/$name.yaml"
}

# ot_deploy <host> <name> [--no-activate]: deploy the copied document; with
# --activate --wait unless --no-activate (deployed, not started).
ot_deploy() {
  local host=$1 name=$2 activate=(--activate --wait)
  [ "${3:-}" = --no-activate ] && activate=()
  ot_cli "$host" deploy model --file "$OT_DIR/$name.yaml" "${activate[@]}" --format json
}

ot_delete() { ot_cli "$1" delete deployment "$2" --stop --format json; }

# ot_status <host> <name>: status JSON (kept in the step's output), then the
# role's tracing and each instance's engine tracing on one line.
ot_status() {
  local host=$1 name=$2 out
  out=$(ot_cli "$host" status deployment "$name" --format json) || { printf '%s\n' "$out"; return 1; }
  printf '%s\n' "$out"
  dry && return 0
  printf '%s' "$out" | python3 -c 'import json,sys
t=sys.stdin.read(); d=json.loads(t[t.index("{"):]); d=d.get("deployment",d)
print("TRACING", json.dumps({"name": d.get("name"), "observed_state": d.get("observed_state"), "tracing": d.get("tracing"),
  "engine_tracing": [i.get("engine_tracing") for i in d.get("instances") or []]}))'
}

# ot_wait_state <host> <name> <observed_state> [timeout_s]
ot_wait_state() {
  local host=$1 name=$2 want=$3 timeout=${4:-900}
  rsh "$host" "for i in \$(seq 0 2 $timeout); do s=\$(XDG_CONFIG_HOME=$OT_CFG CAPYCTL_STATE_DIR=$OT_STATE $(rbin "$host") status deployment $name --format json 2>/dev/null | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin); print(d.get(\"deployment\",d).get(\"observed_state\",\"\"))
except Exception: print(\"\")'); [ \"\$s\" = $want ] && { echo '$name $want after '\$i' s'; exit 0; }; sleep 2; done; echo '$name not $want after $timeout s (last: '\$s')' >&2; exit 1"
}

# ot_req <host> <otel_probe.py chat args...>: one request through the standalone
# router, recorded in requests.jsonl in the host's evidence directory.
ot_req() { local host=$1; shift; rsh "$host" "python3 $OT_PROBE chat --dir $OT_STATE --out $OT_HEV/requests.jsonl $*"; }

# ot_load <host> <label> <otel_probe.py load args...>: load into
# load-<label>.jsonl with its summary load-<label>.summary.json.
ot_load() {
  local host=$1 label=$2
  shift 2
  rsh "$host" "python3 $OT_PROBE load --dir $OT_STATE --label $label --out $OT_HEV/load-$label.jsonl --summary $OT_HEV/load-$label.summary.json $*"
}

# ot_bg <host> <name> <command>: a bounded command in the background on the host
# (its output in bg-<name>.out); ot_bg_wait <host> <name> <timeout_s> waits for it.
ot_bg() {
  rsh "$1" "nohup $3 >$OT_HEV/bg-$2.out 2>&1 </dev/null & echo \$! >$OT_HEV/bg-$2.pid"
}
ot_bg_wait() {
  rsh "$1" "read -r pid <$OT_HEV/bg-$2.pid; for i in \$(seq 0 2 $3); do [ -d /proc/\$pid ] || { tail -c 4000 $OT_HEV/bg-$2.out; exit 0; }; sleep 2; done; echo 'bg $2 still running after $3 s' >&2; exit 1"
}

# ot_procs <host> <label>: tracing options and tracing variable names of every
# engine process and CapyCTL role on the host (procs-<label>.json).
ot_procs() { rsh "$1" "python3 $OT_PROBE procs | tee $OT_HEV/procs-$2.json"; }

# ot_conns <host> <label>: TCP sockets on the collector's ports, with the owning
# process (conns-<label>.json).
ot_conns() { rsh "$1" "python3 $OT_PROBE conns --ports 4317,$OTELCOL_PORT | tee $OT_HEV/conns-$2.json"; }

# ot_export_wait: let the role and the engines export what they hold.
ot_export_wait() {
  dry && { log_cmd control-host "sleep $OT_EXPORT_WAIT_S (span export interval)"; return 0; }
  sleep "$OT_EXPORT_WAIT_S"
}

# ot_summary <label> [otel_spans.py summary args...]: spans-<label>.json from
# the fetched collector file and the requests of this row; a short summary on
# standard output.
ot_summary() {
  local label=$1 hev
  shift
  hev=$EVID/host-$(host_short "$OT_HOST")
  x python3 "$MATRIX_DIR/otel_spans.py" summary --collector "$hev/collector-$label.jsonl" \
    --requests "$hev/requests.jsonl" --out "$EVID/spans-$label.json" "$@"
}

# ot_fits <host> <a> <b> <limit bytes>: each startup request within the managed
# limit and the two together above it (else the row cannot force a switch).
ot_fits() {
  local host=$1 a=$2 b=$3 limit=$4
  ot_cli "$host" status deployment "$a" --format json >"$EVID/fits-$a.json"
  ot_cli "$host" status deployment "$b" --format json >"$EVID/fits-$b.json"
  dry && return 0
  python3 - "$EVID/fits-$a.json" "$EVID/fits-$b.json" "$limit" <<'PY'
import json, sys
def request(path):
    text = open(path).read()
    doc = json.loads(text[text.index("{"):])
    doc = doc.get("deployment", doc)
    startup = doc.get("startup") or {}
    return startup.get("bytes")
a, b, limit = request(sys.argv[1]), request(sys.argv[2]), int(sys.argv[3])
ok = a is not None and b is not None and a <= limit and b <= limit and a + b > limit
print(json.dumps({"a_request": a, "b_request": b, "managed_limit": limit, "pressure": ok}))
sys.exit(0 if ok else 1)
PY
}
