# shellcheck shell=bash
# Helpers available to row scripts (rows/<ROW>.sh), sourced by run_row.sh after
# lib.sh. Every helper writes its evidence under $EVID and returns non-zero on
# failure; a row decides what a failure means. HTTP 200 alone is never a pass.

STEP=0
GOLDENS=${GOLDENS:-$RUNSTATE/goldens.json}

# step <name> <command...>: run one numbered step, keep stdout and stderr.
step() {
  local name=$1; shift
  STEP=$((STEP + 1))
  local base
  base=$(printf '%s/%02d-%s' "$EVID" "$STEP" "$name")
  printf '%s step %02d %s\n' "$(now)" "$STEP" "$name" | tee -a "$EVID/timeline.txt" >&2
  if dry; then "$@"; return 0; fi
  local rc=0
  "$@" >"$base.out" 2>"$base.err" || rc=$?
  printf '%s step %02d %s rc=%s\n' "$(now)" "$STEP" "$name" "$rc" >>"$EVID/timeline.txt"
  return "$rc"
}

# FIXTURE_VARIANT=<tag> selects <name>.<tag>.yaml (gen_deployment.py --variant):
# a new deployment <name>-<tag> with a different engine_config (or a plain rerun).
fixture_file() { echo "$RUNSTATE/fixtures/$1${FIXTURE_VARIANT:+.$FIXTURE_VARIANT}.yaml"; }
# Model key (4, 14, 27, 27f, 30), engine and host of a fixture name.
fixture_model() { echo "${1#*-}"; }
fixture_engine() { case ${1:0:1} in v) echo vllm ;; s) echo sglang ;; esac; }
fixture_host() { short_host "$(echo "$1" | cut -c2-3)"; }

deploy() { # deploy <fixture> [--activate] [--wait] [--request-id ULID]
  local f=$1; shift
  [ -f "$(fixture_file "$f")" ] || dry || die "no fixture $f (roles.sh fixtures)"
  cli deploy model --file "$(fixture_file "$f")" "$@" --output json
}
start_dep() { cli start deployment "$1" --output json "${@:2}"; }
stop_dep() { cli stop deployment "$1" --output json "${@:2}"; }
park_dep() { cli park deployment "$1" --output json; }
delete_dep() { cli delete deployment "$1" --output json; }
status_dep() { cli status deployment "$1" --output json; }
inspect_dep() { cli inspect deployment "$1" --output json; }

# variant <fixture> <tag> [gen_deployment.py args...]: write <fixture>.<tag>.yaml
# from the run's fixture inputs; it deploys as a new deployment <fixture>-<tag>
# (select it with FIXTURE_VARIANT=<tag>). Rows that exercise failure shapes or
# declared timeouts state them here with --document-json/--engine-config-json.
variant() {
  local fix=$1 tag=$2 extra=()
  shift 2
  [ -f "$RUNSTATE/measured.json" ] && extra+=(--measured "$RUNSTATE/measured.json")
  [ -f "$RUNSTATE/checkpoints.json" ] && extra+=(--checkpoints "$RUNSTATE/checkpoints.json")
  x python3 "$MATRIX_DIR/gen_deployment.py" --hosts-json "$RUNSTATE/hosts.json" --out-dir "$RUNSTATE/fixtures" \
    "${extra[@]}" --variant "$tag" "$@" "$fix"
}

# refused <command...>: the command is expected to fail. Succeeds only when it
# does; an accepted command is the finding.
refused() {
  if "$@"; then echo "UNEXPECTED: accepted"; return 1; fi
  echo "refused as expected"
}

# wait_state <deployment> <observed_state> [timeout_s]: poll status.
wait_state() {
  local dep=$1 want=$2 timeout=${3:-900} i state failed_for=0
  dry && { log_cmd control-host "poll status deployment $dep until observed_state=$want (<= ${timeout}s)"; return 0; }
  for ((i = 0; i < timeout; i += 2)); do
    state=$(status_dep "$dep" 2>/dev/null | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin); d=d.get("deployment",d); print(d.get("observed_state",""))
except Exception: print("")')
    if [ "$state" = "$want" ]; then echo "$dep $want after ${i}s"; return 0; fi
    # A failed deployment does not leave `failed` by waiting (found live
    # 2026-09-24, M53: its stop was refused and the poll ran to the end).
    if [ "$state" = failed ] && [ "$want" != failed ]; then
      failed_for=$(( ${failed_for:-0} + 2 ))
      if [ "$failed_for" -ge "${WAIT_FAILED_S:-60}" ]; then
        echo "$dep failed for ${failed_for}s while waiting for $want" >&2; failed_for=0; return 1
      fi
    else
      failed_for=0
    fi
    sleep 2
  done
  echo "$dep not $want after ${timeout}s (last: $state)" >&2
  return 1
}

# wait_operation <operation_id> <timeout_s>: poll the server's operations
# (read-only) until the operation is terminal, and print "<id> <state> <error>".
# Rows that expect a failure use it: the state before the operation ran is
# already terminal, so polling the deployment's status could not tell them apart.
wait_operation() {
  local op=$1 timeout=$2 i line
  dry && { log_cmd control-host "poll ledger.py snapshot until operation $op is succeeded, failed or cancelled (<= ${timeout}s)"; return 0; }
  for ((i = 0; i < timeout; i += 2)); do
    line=$(python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" 2>/dev/null | python3 -c 'import json,sys
ops = [o for o in json.load(sys.stdin)["operations_recent"] if o["id"] == sys.argv[1]]
if ops and ops[0]["state"] in ("succeeded", "failed", "cancelled"):
    print(ops[0]["id"], ops[0]["state"], ops[0]["error_code"] or "-")' "$op" || true)
    if [ -n "$line" ]; then echo "$line after ${i}s"; return 0; fi
    sleep 2
  done
  echo "operation $op not terminal after ${timeout}s" >&2
  return 1
}

# host_mem <host> <label>: one MemAvailable sample (kB) into mem.txt.
host_mem() {
  local host=$1 label=$2 kb
  kb=$(rsh_out "$host" 0 "awk '/MemAvailable/{print \$2}' /proc/meminfo")
  dry || echo "$label $kb" >>"$EVID/mem.txt"
  echo "$label MemAvailable ${kb} kB"
}

# The server's accounting for one deployment (read-only SELECTs).
accounting() { python3 "$MATRIX_DIR/ledger.py" accounting --db "$SERVER_DB" --deployment "$1"; }

# Engine error lines from the host's private logs, without anything credential-shaped.
engine_errors() {
  local host=$1
  rsh "$host" "grep -rhaE 'Error|error:|Traceback|Exception|raise |refus|FAILED|CUDA out of memory|not supported|NotImplemented|Unsupported' \
$RRD/host --include='*.log' --include='*.txt' --include='*.err' --include='*.out' 2>/dev/null \
| grep -viE 'key|token=|secret|bearer|password|credential' | tail -n 60; \
echo '## host.log'; grep -aE 'ERROR|WARN|refus|fail' $RRD/host.log | grep -viE 'key|secret|bearer|password' | tail -n 30"
}

# The host holds no engine: no engine process, no GPU compute process, no
# SGLang rendezvous directory (found live 2026-09-23: a signalled stop left
# /tmp/mllm-rdzv-*) and, when a port is given, nothing listening on it. Prints LEFTOVER_* on a finding. Rows
# that use it run with no other deployment active on that host.
host_clean() { # host_clean <host> [port]
  local host=$1 port=${2:-}
  rsh "$host" "echo '## engine procs'; pgrep -af '$ENGINE_PGREP' | grep -vE 'pgrep|tailscaled|bash -c' && echo LEFTOVER_ENGINE; \
echo '## compute apps'; nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader | grep . && echo LEFTOVER_GPU; \
echo '## port ${port:-none}'; [ -n '$port' ] && ss -ltn | grep -E ':$port\\b' && echo LEFTOVER_PORT; \
echo '## rendezvous'; ls -d /tmp/mllm-rdzv-* $RRD/host/rendezvous/* 2>/dev/null && echo LEFTOVER_RDZV; true"
}

# host_idle <host>: host_clean as a precondition; fails on any leftover.
host_idle() {
  local out
  out=$(host_clean "$@")
  echo "$out"
  dry && return 0
  ! grep -q LEFTOVER <<<"$out"
}

# cleanup_check <deployment> <host> [owned.json] [ready-accounting.json] [label]
# Verified cleanup: every identity recorded at Ready is gone on its host, no
# engine process or GPU compute process remains, the engine port is free, and
# the server holds no charge, claim, lease or retained binding for the
# deployment. Defaults are M16's file names; a row that restarts passes each
# generation's own files and a label.
cleanup_check() {
  local dep=$1 host=$2 ownedf=${3:-$EVID/owned-$1.json} acct=${4:-$EVID/accounting-ready.json} label=${5:-}
  local sfx=${label:+-$label} rc=0 pid ticks boot port probe
  if dry; then
    log_cmd control-host "for each identity in $ownedf: ssh $host signal_owned.py --pid P --ticks T --boot B --signal 0 (want absent)"
    host_clean "$host" "<port>"
    x python3 "$MATRIX_DIR/ledger.py" accounting --db "$SERVER_DB" --deployment "$dep"
    return 0
  fi
  port=$(python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1]))["endpoint_leases"][0]["port"])
except Exception: print("")' "$acct")
  if [ -f "$ownedf" ]; then
    # signal_owned.py exits 1 for an absent identity (the outcome wanted here),
    # so judge the recorded outcome, not the exit status (pipefail would).
    while read -r pid ticks boot; do
      probe=$(rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pid $pid --ticks $ticks --boot $boot --signal 0" || true)
      echo "$probe" >>"$EVID/cleanup-identities$sfx.txt"
      case $probe in *'"outcome": "absent"'*) ;; *) rc=1 ;; esac
    done < <(python3 -c 'import json,sys
for i in json.load(open(sys.argv[1])): print(i["pid"], i["start_ticks"], i["boot_id"])' "$ownedf")
  else
    echo "no owned identities were recorded at Ready"
  fi
  host_clean "$host" "$port" | tee "$EVID/cleanup-host$sfx.txt"
  grep -q LEFTOVER "$EVID/cleanup-host$sfx.txt" && rc=1
  accounting "$dep" >"$EVID/accounting-stopped$sfx.json"
  python3 - "$EVID/accounting-stopped$sfx.json" <<'PY' || rc=1
import json, sys
a = json.load(open(sys.argv[1]))
held = {k: a[k] for k in ("reservations", "resource_owners", "lifecycle_claims", "request_leases_deployment", "retained_bindings", "endpoint_leases") if a[k]}
print(json.dumps({"held": held, "request_leases_total": a["request_leases_total"], "deployment": a["deployment"]}))
sys.exit(1 if held or a["request_leases_total"] else 0)
PY
  return "$rc"
}

# closure_check <deployment> <host> [label]: a launch that failed closed the
# deployment rather than leaving it uncertain (SPEC section 13.2, ADR 0011):
# admission shut, no binding retained (an uncertain one is the finding), no
# charge, claim or lease, the failure recorded on the deployment's operations,
# and nothing of the engine left on the host.
closure_check() {
  local dep=$1 host=$2 label=${3:-closed} rc=0
  if dry; then
    x python3 "$MATRIX_DIR/ledger.py" accounting --db "$SERVER_DB" --deployment "$dep"
    x python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"
    host_clean "$host"
    return 0
  fi
  accounting "$dep" >"$EVID/accounting-$label.json"
  python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" >"$EVID/ledger-$label.json"
  python3 - "$EVID/accounting-$label.json" "$EVID/ledger-$label.json" <<'PY' || rc=1
import json, sys
a = json.load(open(sys.argv[1]))
snap = json.load(open(sys.argv[2]))
dep = (a["deployment"] or [{}])[0]
ops = [o for o in snap["operations_recent"] if o["deployment_id"] == a["deployment_id"]]
uncertain = [b for b in a["retained_bindings"] if b.get("state") == "uncertain"]
held = {k: a[k] for k in ("reservations", "resource_owners", "lifecycle_claims", "retained_bindings", "endpoint_leases") if a[k]}
failed_ops = [o for o in ops if o["state"] in ("failed", "cancelled")]
verdict = {
    "observed_state": dep.get("observed_state"),
    "admission_enabled": dep.get("admission_enabled"),
    "uncertain_bindings": uncertain,
    "held": held,
    "operations": ops,
    "closed": not uncertain and not held and not dep.get("admission_enabled") and bool(failed_ops),
}
print(json.dumps(verdict, indent=1))
sys.exit(0 if verdict["closed"] else 1)
PY
  host_clean "$host" | tee "$EVID/host-$label.txt"
  grep -q LEFTOVER "$EVID/host-$label.txt" && rc=1
  return "$rc"
}

# infer <route> <prompt> [--expect TEXT] [--stream] [--max-tokens N]
infer() {
  local route=$1 prompt=$2; shift 2
  x python3 "$MATRIX_DIR/infer.py" --route "$route" --prompt "$prompt" --out "$EVID/requests.jsonl" "$@"
}

# load <loadgen args...>: records into requests-load-<n>.jsonl and a summary.
LOADN=0
load() {
  LOADN=$((LOADN + 1))
  x python3 "$MATRIX_DIR/loadgen.py" --out "$EVID/load-$LOADN.jsonl" --summary "$EVID/load-$LOADN.summary.json" "$@"
}

# i1 <route> <fixture>: I1 model identity. The first call for a model in the run
# captures its golden; later calls check against it. The checkpoint half compares
# the deployment's recorded content_fingerprint with the host's checkpoint digest.
i1() { # i1 <route> <fixture> [deployment name, default the fixture]
  local route=$1 fixture=$2 depname=${3:-$2} model engine host mode
  model=$(fixture_model "$fixture"); engine=$(fixture_engine "$fixture"); host=$(fixture_host "$fixture")
  mode=check
  if ! dry && ! python3 -c 'import json,sys; sys.exit(0 if sys.argv[2] in json.load(open(sys.argv[1])) else 1)' "$GOLDENS" "$model@$engine" 2>/dev/null; then
    mode=capture
  fi
  x python3 "$MATRIX_DIR/identity_probe.py" "$mode" --route "$route" --model "$model" --engine "$engine" \
    --goldens "$GOLDENS" --out "$EVID/i1.jsonl"
  if dry; then x python3 "$MATRIX_DIR/ledger.py" fingerprint --db "$SERVER_DB" --deployment "$depname"; return 0; fi
  python3 "$MATRIX_DIR/ledger.py" fingerprint --db "$SERVER_DB" --deployment "$depname" >"$EVID/i1-fingerprint-$fixture.json"
  python3 - "$EVID/i1-fingerprint-$fixture.json" "$RUNSTATE/checkpoints.json" "$host" "$model" "$MATRIX_DIR/models.json" <<'PY' | tee -a "$EVID/i1-digest.txt"
import json, sys
fp = json.load(open(sys.argv[1]))["content_fingerprint"]
try:
    checkpoints = json.load(open(sys.argv[2]))
except FileNotFoundError:
    print(f"digest unchecked: no checkpoints.json (e0.sh checkpoints); recorded {fp}"); sys.exit(0)
models = json.load(open(sys.argv[5]))["models"]
key = sys.argv[4]
spec = models[key] if "dir" in models[key] else {**models[models[key]["extends"]], **models[key]}
want = "sha256:" + checkpoints[sys.argv[3]][spec["dir"]]
print(f"digest {'match' if fp == want else 'MISMATCH'} recorded={fp} expected={want}")
sys.exit(0 if fp == want else 1)
PY
}

# owned <deployment>: owned identities and their liveness on the host.
# e0.sh and fault.sh honour DRY_RUN themselves, so they are called directly.
owned() { "$MATRIX_DIR/e0.sh" owned "$1" "$EVID"; }
# snap <label>: an E0 snapshot mid-row.
snap() { "$MATRIX_DIR/e0.sh" snap "$1" "$EVID"; }
# fault <fault.sh args...>: faults are recorded in the row's faults.log.
fault() { "$MATRIX_DIR/fault.sh" "$@"; }

# --- Phase C and later helpers (2026-09-23) ---------------------------------

# keep_owned <deployment> <label>: owned identities and accounting under a label.
keep_owned() {
  local dep=$1 label=$2
  owned "$dep" >/dev/null
  dry && return 0
  cp "$EVID/owned-$dep.json" "$EVID/owned-$dep-$label.json"
  accounting "$dep" >"$EVID/accounting-$dep-$label.json"
  python3 -c 'import json,sys
a=json.load(open(sys.argv[1]))
print(json.dumps({"deployment":a["deployment"],"charges":[(o["owner_id"],o["footprint"].get("phase"),[x[1] for x in o["footprint"].get("allocations",[])]) for o in a["resource_owners"]],"retained_bindings":a["retained_bindings"]}))' "$EVID/accounting-$dep-$label.json"
}

# alive_check <host> <owned.json>: every recorded identity is still the same live process.
alive_check() {
  local host=$1 ownedf=$2 pid ticks boot probe rc=0 hid h
  dry && return 0
  while read -r pid ticks boot hid; do
    h=$host
    [ "$hid" = "${HOST_ID_92:-}" ] && h=host-a
    [ "$hid" = "${HOST_ID_17:-}" ] && h=host-b
    probe=$(rsh "$h" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pid $pid --ticks $ticks --boot $boot --signal 0" || true)
    echo "$h $pid $probe"
    case $probe in *'"outcome": "absent"'*|'') rc=1 ;; esac
  done < <(python3 -c 'import json,sys
for i in json.load(open(sys.argv[1])): print(i["pid"], i["start_ticks"], i["boot_id"], i.get("host_id") or "-")' "$ownedf")
  return "$rc"
}

# same_identities <a.json> <b.json>: identical (pid, start ticks, boot) sets, non-empty.
same_identities() {
  dry && return 0
  python3 - "$1" "$2" <<'PY'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:])
key = lambda s: sorted((i["pid"], i["start_ticks"], i["boot_id"]) for i in s)
same = bool(a) and key(a) == key(b)
print(json.dumps({"same": same, "a": key(a), "b": key(b)}))
sys.exit(0 if same else 1)
PY
}

# evidence <deployment> <label>: operations, steps with recorded evidence, journal (read-only, redacted).
evidence() {
  dry && { x python3 "$MATRIX_DIR/ledger.py" evidence --db "$SERVER_DB" --deployment "$1"; return 0; }
  python3 "$MATRIX_DIR/ledger.py" evidence --db "$SERVER_DB" --deployment "$1" >"$EVID/evidence-$1-$2.json"
  python3 -c 'import json,sys
e=json.load(open(sys.argv[1]))
for o in e["operations"]: print("op", o["kind"], o["state"], o["error_code"] or "-", o["updated_at"])
for s in e["steps"]:
    st=s["step"] if isinstance(s["step"],dict) else {}
    print("step", st.get("kind"), s["state"], "evidence" if s["evidence"] else "-")' "$EVID/evidence-$1-$2.json"
}

# mem_release <before-label> <ready-label> <parked-label> <min-fraction>: the park returned
# at least that fraction of the Ready drop (MemAvailable, from mem.txt).
mem_release() {
  dry && return 0
  python3 - "$EVID/mem.txt" "$@" <<'PY'
import json, sys
m = {}
for line in open(sys.argv[1]):
    if line.strip():
        k, v = line.split(); m[k] = int(v)
b, r, p, frac = m[sys.argv[2]], m[sys.argv[3]], m[sys.argv[4]], float(sys.argv[5])
drop, back = b - r, p - r
v = {"before_kB": b, "ready_kB": r, "parked_kB": p, "ready_drop_kB": drop, "released_kB": back,
     "released_fraction": round(back / drop, 3) if drop > 0 else None}
v["ok"] = drop > 0 and back >= frac * drop
print(json.dumps(v))
sys.exit(0 if v["ok"] else 1)
PY
}

# timed <label> <command...>: run and record wall time in times.txt.
timed() {
  local label=$1 t0 t1 rc=0; shift
  t0=$(now_ms); "$@" || rc=$?; t1=$(now_ms)
  dry || echo "$label $((t1 - t0)) rc=$rc" >>"$EVID/times.txt"
  echo "$label took $((t1 - t0)) ms rc=$rc"
  return "$rc"
}

# engine_log_lines <host> <pattern>: matching lines from private engine logs (debug-engine-logs),
# credential-shaped lines removed.
engine_log_lines() {
  local host=$1 pat=$2
  rsh "$host" "grep -rhaE '$pat' $RRD/host --include='*.log' --include='*.txt' --include='*.err' --include='*.out' 2>/dev/null | grep -viE 'key|token|secret|bearer|password|credential' | tail -n 40; true"
}

# cleanup_check_partial <deployment> <host> <owned.json>: verified cleanup of one
# deployment while others keep running on the host: its identities are gone, its
# endpoint port is free, and the server holds nothing for it.
cleanup_check_partial() {
  local dep=$1 host=$2 ownedf=$3 rc=0 pid ticks boot probe hid h
  dry && return 0
  # Each identity is probed on the host that owns it (host_id), else on <host>.
  while read -r pid ticks boot hid; do
    h=$host
    [ "$hid" = "${HOST_ID_92:-}" ] && h=host-a
    [ "$hid" = "${HOST_ID_17:-}" ] && h=host-b
    probe=$(rsh "$h" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pid $pid --ticks $ticks --boot $boot --signal 0" || true)
    echo "$h $pid $probe"
    case $probe in *'"outcome": "absent"'*) ;; *) rc=1 ;; esac
  done < <(python3 -c 'import json,sys
for i in json.load(open(sys.argv[1])): print(i["pid"], i["start_ticks"], i["boot_id"], i.get("host_id") or "-")' "$ownedf")
  accounting "$dep" >"$EVID/accounting-$dep-stopped.json"
  python3 - "$EVID/accounting-$dep-stopped.json" <<'PY' || rc=1
import json, sys
a = json.load(open(sys.argv[1]))
held = {k: a[k] for k in ("reservations", "resource_owners", "lifecycle_claims", "request_leases_deployment", "retained_bindings", "endpoint_leases") if a[k]}
print(json.dumps({"held": held, "deployment": a["deployment"]}))
sys.exit(1 if held else 0)
PY
  return "$rc"
}

# residue_check <deployment id>: after `delete deployment`, the server's ledger
# snapshot holds nothing for that id. The deleted deployment's tombstone row
# (SPEC §6.3, W6: name `deleted/<id>`, kept for its history) is not residue;
# every other row naming the id is.
residue_check() {
  local depid=$1
  dry && return 0
  python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" | python3 -c 'import json,sys
s=json.load(sys.stdin); dep=sys.argv[1]
def tomb(k,r): return k=="deployments" and r.get("id")==dep and r.get("name")=="deleted/"+dep
ops=[o for o in s.get("operations_recent",[]) if o.get("deployment_id")==dep]
hits={k:[r for r in v if isinstance(r,dict) and dep in json.dumps(r) and not tomb(k,r)] for k,v in s.items() if isinstance(v,list) and k!="operations_recent"}
hits={k:v for k,v in hits.items() if v}
print(json.dumps({"ops":ops,"held":hits})); sys.exit(1 if hits else 0)' "$depid"
}
