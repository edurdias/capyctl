#!/usr/bin/env bash
# E0 base evidence for matrix rows (matrix section 3; plan unit W2).
#
#   e0.sh static <dir>          commit, snapshot digest, binary SHA-256 per machine, engine
#                               versions and environment digests, boot ids, driver
#   e0.sh checkpoints <dir>     checkpoint payload SHA-256 per model and host (slow; once per run);
#                               equality across hosts only: it is not the product's manifest digest
#   e0.sh vparity <dir>         G12: read-only vLLM environment parity, host-a venv2 vs host-b 0.29 venv
#   e0.sh snap <label> <dir>    status, hosts, ledger (read-only), per-host MemAvailable,
#                               nvidia-smi compute processes, engine processes, listeners,
#                               liveness of every owned process identity
#   e0.sh owned <deployment> <dir>   owned identities of one deployment and their liveness
#
# Everything is read-only: the CLI's status/list views and SELECTs on the
# server database. Nothing secret is written. DRY_RUN=1 prints the plan.
. "$(dirname "$0")/lib.sh"

MODEL_DIRS=(qwen3-4b-instruct qwen3-14b qwen3-30b-a3b qwen3.8-27b qwen3.8-27b-nvfp4)

# Distribution list digest of a venv: sorted name==version of every installed
# distribution, read with importlib.metadata (no pip or uv needed).
venv_digest() {
  printf '%s\n' "$1/bin/python3 -c 'import importlib.metadata as m, hashlib; l=sorted({f\"{d.name.lower()}=={d.version}\" for d in m.distributions()}); print(len(l), hashlib.sha256(\"\\n\".join(l).encode()).hexdigest())'"
}

static() {
  local dir=$1 host rc=0
  x mkdir -p "$dir"
  if ! dry; then
    {
      echo "commit $(git -C "$REPO" rev-parse HEAD)"
      echo "snapshot_tree $(cat "$SNAPSHOT/tree.sha256" 2>/dev/null || echo none)"
      echo "snapshot_commit $(cat "$SNAPSHOT/commit" 2>/dev/null || echo none)"
      echo "dirty_files $(cat "$SNAPSHOT/dirty-count" 2>/dev/null || echo unknown)"
      echo "cargo_lock $(cat "$SNAPSHOT/cargo-lock.sha256" 2>/dev/null || echo unknown)"
      echo "binary_control-host $(sha256sum "$CAPYCTL" | cut -c1-64)"
      echo "run $RUN"
    } >"$dir/static.control-host.txt"
  fi
  for host in "${MATRIX_HOSTS[@]}"; do
    local vv
    vv=$(vllm_venv "$host")
    rsh "$host" "echo host \$(hostname); echo boot \$(cat /proc/sys/kernel/random/boot_id); echo kernel \$(uname -r); \
echo driver \$(nvidia-smi --query-gpu=driver_version --format=csv,noheader); \
echo binary \$(sha256sum $RBIN | cut -c1-64); echo tree \$(cd $REMOTE_TREE && $TREE_DIGEST_SH); \
echo sglang_version \$($SGLANG_VENV/bin/python3 -c 'import sglang; print(sglang.__version__)' 2>/dev/null | tail -1); \
echo sglang_env \$($(venv_digest "$SGLANG_VENV")); \
echo vllm_version \$($vv/bin/python3 -c 'import importlib.metadata as m; print(m.version(\"vllm\"))'); \
echo vllm_venv $vv; echo vllm_env \$($(venv_digest "$vv")); \
python3 $REMOTE_TREE/scripts/live/matrix/check_runtime.py $REMOTE_TREE/runtime sglang_entry.py vllm_entry.py capyctl_vllm_guard.py pinned_file_observation.py" \
      >"$( dry && echo /dev/null || echo "$dir/static.$host.txt")" || { echo "static: runtime check refused on $host" >&2; rc=1; }
  done
  return "$rc"
}

checkpoints() {
  local dir=$1 host script
  x mkdir -p "$dir"
  # Payload digest: every file under the model directory except the Hugging Face
  # download cache metadata, by relative path (as recorded in Phase B).
  script="cd $MODELS_ROOT && for m in ${MODEL_DIRS[*]}; do ( cd \$m && d=\$(find . -path ./.cache -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-64); echo \"\$m \$d\" ) & done; wait"
  for host in "${MATRIX_HOSTS[@]}"; do
    rsh "$host" "$script" >"$( dry && echo /dev/null || echo "$dir/checkpoints.$host.txt")"
  done
  dry && { log_cmd control-host "merge checkpoints.<host>.txt into $dir/checkpoints.json and $RUNSTATE/payload-digests.json"; return 0; }
  python3 - "$dir" "${MATRIX_HOSTS[@]}" <<'PY'
import json, sys
out_dir, hosts = sys.argv[1], sys.argv[2:]
result = {}
for host in hosts:
    result[host] = dict(line.split() for line in open(f"{out_dir}/checkpoints.{host}.txt") if line.strip())
dirs = sorted(set().union(*[set(v) for v in result.values()]))
result["_equal_across_hosts"] = {d: len({result[h].get(d) for h in hosts}) == 1 for d in dirs}
json.dump(result, open(f"{out_dir}/checkpoints.json", "w"), indent=1, sort_keys=True)
print(json.dumps(result["_equal_across_hosts"]))
PY
  # Kept as E0 evidence only (payload-digests.json), never as $RUNSTATE/checkpoints.json:
  # this is a payload digest, not the product's checkpoint manifest digest
  # (capyctl-agent checkpoint.rs), so a fixture that declared it as content_fingerprint
  # was refused at first placement (found live by the M48 soak, 2026-09-24).
  mkdir -p "$RUNSTATE" && cp "$dir/checkpoints.json" "$RUNSTATE/payload-digests.json"
}

vparity() {
  local dir=$1 host vv
  x mkdir -p "$dir"
  for host in "${MATRIX_HOSTS[@]}"; do
    vv=$(vllm_venv "$host")
    # RECORD digests with the venv-path shebang lines removed (they differ by path only).
    rsh "$host" "echo host \$(hostname) venv $vv; echo env \$($(venv_digest "$vv")); \
$vv/bin/python3 -c 'import glob,hashlib,os,sys
sp=glob.glob(sys.argv[1]+\"/lib/python3*/site-packages\")[0]
for r in sorted(glob.glob(sp+\"/*.dist-info/RECORD\")):
    lines=[l for l in open(r,\"rb\").read().splitlines() if not l.startswith(b\"../../../bin/\")]
    print(\"R\", os.path.basename(os.path.dirname(r)), hashlib.sha256(b\"\\n\".join(lines)).hexdigest())' $vv" \
      >"$( dry && echo /dev/null || echo "$dir/vparity.$host.txt")"
  done
  dry && return 0
  if diff <(grep -E '^(env|R) ' "$dir/vparity.$HOST_A.txt") <(grep -E '^(env|R) ' "$dir/vparity.$HOST_B.txt") >"$dir/vparity.diff"; then
    echo "vLLM parity: identical distributions and RECORD digests" | tee "$dir/vparity.verdict"
  else
    echo "vLLM parity: DIFFERS (see vparity.diff)" | tee "$dir/vparity.verdict"; return 1
  fi
}

host_state_script() {
  printf '%s\n' "echo \"# \$(hostname) \$(date -u +%FT%T.%3NZ) boot=\$(cat /proc/sys/kernel/random/boot_id)\"; grep MemAvailable /proc/meminfo; \
echo '## nvidia-smi compute'; nvidia-smi --query-compute-apps=pid,process_name,used_memory --format=csv,noheader; \
echo '## engine procs'; ps -eo pid,pgid,ppid,lstart,rss,args --sort=pid | grep -E 'sglang|vllm|VLLM|EngineCore' | grep -vE 'grep|tailscaled|bash -c' | cut -c1-220; \
echo '## listeners'; ss -ltnp 2>/dev/null | grep -E ':($INGRESS_PORT|81[0-9][0-9])\\b'; \
echo '## roles'; for f in $RRD/host.pid $RRD/memhog.pid; do [ -f \$f ] && echo \"\$f \$(cat \$f) \$(python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile \$f --signal 0 2>&1)\"; done; true"
}

# Liveness of every owned identity in a ledger snapshot, checked on its host.
check_owned() { # check_owned <ledger.json> <out>
  local ledger=$1 out=$2 host line
  dry && { log_cmd control-host "for each live binding identity in $ledger: ssh <host> signal_owned.py --pid P --ticks T --boot B --signal 0"; return 0; }
  : >"$out"
  for host in "${MATRIX_HOSTS[@]}"; do
    local hid
    hid=$(host_id "$host")
    python3 - "$ledger" "$hid" <<'PY' >"$out.$host.ids"
import json, sys
snap = json.load(open(sys.argv[1]))
for b in snap["runtime_bindings"]:
    if b.get("host_id") == sys.argv[2]:
        for i in b["identities"]:
            print(b["deployment_id"], i["role"], i["pid"], i["start_ticks"], i["boot_id"])
PY
    while read -r dep role pid ticks boot; do
      line=$(rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pid $pid --ticks $ticks --boot $boot --signal 0" || true)
      echo "$host $dep $role $line" >>"$out"
    done <"$out.$host.ids"
    rm -f "$out.$host.ids"
  done
}

snap() {
  local label=$1 dir=$2 host
  x mkdir -p "$dir"
  local p=$dir/$label
  if dry; then
    cli list deployments --format json >/dev/null; cli list hosts --format json >/dev/null
    x python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"
  else
    cli list deployments --format json >"$p.deployments.json" 2>&1 || true
    cli list hosts --format json >"$p.hosts.json" 2>&1 || true
    python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" >"$p.ledger.json" 2>&1 || true
  fi
  for host in "${MATRIX_HOSTS[@]}"; do
    rsh "$host" "$(host_state_script)" >"$( dry && echo /dev/null || echo "$p.$host.txt")" || true
  done
  check_owned "$p.ledger.json" "$p.owned.txt"
}

owned() {
  local dep=$1 dir=$2
  x mkdir -p "$dir"
  if dry; then x python3 "$MATRIX_DIR/ledger.py" owned --db "$SERVER_DB" --deployment "$dep"; return 0; fi
  python3 "$MATRIX_DIR/ledger.py" owned --db "$SERVER_DB" --deployment "$dep" | tee "$dir/owned-$dep.json"
}

cmd=${1:-}; shift || true
case $cmd in
  static|checkpoints|vparity|snap|owned) load_run; "$cmd" "$@" ;;
  *) sed -n '2,17p' "$0"; exit 2 ;;
esac
