# shellcheck shell=bash
# Multi-node group helpers for rows MN1-MN8 (ADR 0028, design section 17.2),
# sourced by those rows after rowlib.sh. A group here is two tensor-parallel
# ranks: host A is the head (rank 0, serves the API through the router), host B
# the worker (rank 1), over the direct link. Fixtures come from gen_group.py:
# vg-30 (vLLM), sg-30 (SGLang), tg-n35 (TensorFold), each routed by its name.
#
# Every helper writes its evidence under $EVID and returns non-zero on a
# finding; DRY_RUN=1 prints the plan. Status fields are the ones the server
# emits (crates/capyctl-store/src/snapshot.rs): the instance carries
# `plan_generation`, `rendezvous` (head peer ip:port) and `members`, each with
# `host` (host id), `node_rank`, `role`, `state` (reserved, dispatching,
# launched, uncertain, settled, failed), `processes`, `reservation` ({phase,
# bytes} or null), `residency` and `last_error`. Ownership evidence for each
# member comes from the ledger (`ledger.py group` / `members`), read-only.

GROUP_HOSTS=("$HOST_A" "$HOST_B")
PARK_MIN_RELEASE=${PARK_MIN_RELEASE:-0.5}

group_fixture_name() {
  case $1 in
    vllm) echo vg-30 ;;
    sglang) echo sg-30 ;;
    tensorfold) echo tg-n35 ;;
    *) die "unknown group engine $1 (vllm, sglang or tensorfold)" ;;
  esac
}
group_model_dir() {
  case $1 in
    tensorfold) echo "${GROUP_MODEL_TENSORFOLD:-nemotron-3.5-lightning-30b-a3b-4bit-tf}" ;;
    *) echo "${GROUP_MODEL_VLLM_SGLANG:-qwen3-30b-a3b}" ;;
  esac
}

# group_fixture <engine> [gen_group.py args...]: write the group's fixture into
# the run's fixtures (deploy then finds it as <name>, or <name>.<tag> with a variant).
group_fixture() {
  local engine=$1; shift
  x python3 "$MATRIX_DIR/gen_group.py" --engine "$engine" --model-dir "$(group_model_dir "$engine")" \
    --out-dir "$RUNSTATE/fixtures" "$@"
}

# The configured name of an enrolled host id (empty when it is neither host).
host_of_id() {
  if [ -n "${HOST_ID_a:-}" ] && [ "$1" = "$HOST_ID_a" ]; then echo "$HOST_A"
  elif [ -n "${HOST_ID_b:-}" ] && [ "$1" = "$HOST_ID_b" ]; then echo "$HOST_B"
  fi
}

# group_preflight <engine>: what the group needs before a row deploys it, checked
# read-only; prints one PRECONDITION line per item and fails early on any the
# product would refuse at deploy or launch: both hosts online and reconciled,
# declaring `engine_groups`, each with its direct-link address as its peer
# address, the engine's profile published on both with one build fingerprint,
# and the bring-up checkpoint present on both with the same file list and sizes.
# The owner's host tuning (compaction, memlock, /dev/infiniband) is reported;
# the product only warns on it unless a host sets require_rdma.
group_preflight() {
  local engine=$1 dir rc=0 host sums=() out
  dir=$(group_model_dir "$engine")
  echo "PRECONDITION engine $engine profile $engine model $MODELS_ROOT/$dir hosts $HOST_A (head) $HOST_B (worker)"
  cli list hosts --format json >"$EVID/hosts-preflight.json" || { echo "PRECONDITION FAILED list hosts"; return 1; }
  if ! dry; then
    python3 - "$EVID/hosts-preflight.json" "$engine" "$HOST_A" "${HOST_A_DIRECT_ADDR:-}" "$HOST_B" "${HOST_B_DIRECT_ADDR:-}" <<'PY' || rc=1
import json, sys
listing = json.load(open(sys.argv[1]))
engine = sys.argv[2]
want = {sys.argv[3]: sys.argv[4], sys.argv[5]: sys.argv[6]}
hosts = {h.get("name"): h for h in listing.get("hosts", [])}
bad = False
builds = {}
for name, peer in want.items():
    h = hosts.get(name) or {}
    s = h.get("session") or {}
    caps = s.get("capabilities") or h.get("capabilities") or []
    group = s.get("group") or {}
    profile = next((p for p in s.get("profiles", []) if p.get("name") == engine), None)
    checks = {
        "online": bool(h.get("online")) and bool(s.get("reconciled")),
        "engine_groups": "engine_groups" in caps,
        "peer_address": bool(peer) and group.get("peer_address") == peer,
        "profile": profile is not None,
    }
    builds[name] = profile and profile.get("build_fingerprint")
    print(f"PRECONDITION host {name} online={checks['online']} engine_groups={checks['engine_groups']} "
          f"peer_address={group.get('peer_address')} (want {peer or 'HOST_*_DIRECT_ADDR unset'}) "
          f"findings={group.get('findings')} profile={engine}:{builds[name]} "
          f"installation={(profile or {}).get('installation')}")
    for item, ok in checks.items():
        if not ok:
            print(f"PRECONDITION FAILED {name} {item}")
            bad = True
values = set(builds.values())
if len(values) != 1 or None in values:
    print(f"PRECONDITION FAILED build fingerprints differ or are missing: {builds} (group_profile_mismatch)")
    bad = True
else:
    print(f"PRECONDITION build fingerprint equal on both hosts: {values.pop()}")
sys.exit(1 if bad else 0)
PY
  fi
  for host in "${GROUP_HOSTS[@]}"; do
    out=$(rsh_out "$host" "<listing-digest> <bytes>" "d=$MODELS_ROOT/$dir; if [ ! -f \$d/config.json ]; then echo MISSING; exit 0; fi; \
cd \$d && printf '%s %s\n' \"\$(find . -type f ! -path './.cache/*' -printf '%P %s\n' | LC_ALL=C sort | sha256sum | cut -c1-64)\" \"\$(du -sbL . | cut -f1)\"") || out=MISSING
    echo "PRECONDITION model $host $MODELS_ROOT/$dir listing+bytes: $out"
    if [ "$out" = MISSING ]; then
      echo "PRECONDITION FAILED model $MODELS_ROOT/$dir is not on $host: copy it there (same path, same files) before the row"
      rc=1
    fi
    sums+=("$out")
  done
  if ! dry && [ "${sums[0]}" != "${sums[1]}" ]; then
    echo "PRECONDITION FAILED model $MODELS_ROOT/$dir differs between $HOST_A and $HOST_B (file list or sizes); the server would refuse group_checkpoint_mismatch"
    rc=1
  fi
  for host in "${GROUP_HOSTS[@]}"; do
    rsh "$host" "echo \"PRECONDITION tuning $host compaction_proactiveness=\$(cat /proc/sys/vm/compaction_proactiveness 2>/dev/null || echo none) memlock=\$(ulimit -l) \
uverbs=\$(ls -l /dev/infiniband/uverbs* 2>/dev/null | awk '{print \$1\":\"\$NF}' | tr '\n' ' ')\"; \
echo \"PRECONDITION direct link $host \$(ip -br addr 2>/dev/null | grep -F ' $(direct_addr "$host")/' || echo 'address $(direct_addr "$host") not on any interface')\"; \
echo \"PRECONDITION rdma $host \$(python3 $REMOTE_TREE/scripts/live/matrix/rdma_counters.py | cut -c1-400)\"" || rc=1
  done
  return "$rc"
}

# group_state <deployment>: observed_state now.
group_state() {
  status_dep "$1" 2>/dev/null | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin); d=d.get("deployment",d); print(d.get("observed_state",""))
except Exception: print("")'
}

# wait_states <deployment> "<state> [state...]" [timeout_s]: poll until observed_state is one of them.
wait_states() {
  local dep=$1 want=$2 timeout=${3:-900} i state=""
  dry && { log_cmd control-host "poll status deployment $dep until observed_state in ($want) (<= ${timeout}s)"; return 0; }
  for ((i = 0; i < timeout; i += 2)); do
    state=$(group_state "$dep")
    if [ -n "$state" ]; then
      case " $want " in *" $state "*) echo "$dep $state after ${i}s"; return 0 ;; esac
    fi
    sleep 2
  done
  echo "$dep not in ($want) after ${timeout}s (last: $state)" >&2
  return 1
}

# group_status <deployment> <label>: status JSON to status-<dep>-<label>.json and
# one evidence line per instance and member.
group_status() {
  local dep=$1 label=$2 f=$EVID/status-$1-$2.json
  if dry; then status_dep "$dep" >/dev/null; return 0; fi
  status_dep "$dep" >"$f" || { cat "$f"; return 1; }
  python3 - "$f" "${HOST_ID_a:-}" "$HOST_A" "${HOST_ID_b:-}" "$HOST_B" <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d = d.get("deployment", d)
names = {sys.argv[2]: sys.argv[3], sys.argv[4]: sys.argv[5]}
print("deployment", d.get("name"), "observed", d.get("observed_state"), "desired", d.get("desired_state"),
      "ready_instances", d.get("ready_instances"), "admission", d.get("admission_enabled"), "dispatch", d.get("dispatch_enabled"))
for i in d.get("instances", []):
    print(" instance", i.get("index"), "observed", i.get("observed_state"), "plan_generation", i.get("plan_generation"),
          "rendezvous", i.get("rendezvous"), "peer_transport", i.get("peer_transport"), "topology", i.get("topology"),
          "last_error", i.get("last_error"))
    for m in i.get("members", []):
        r = m.get("reservation")
        print("  member rank", m.get("node_rank"), names.get(m.get("host"), m.get("host")), m.get("role"),
              "state", m.get("state"), "processes", m.get("processes"),
              "reservation", f"{r.get('phase')}:{r.get('bytes')}" if r else None,
              "residency", m.get("residency"), "last_error", m.get("last_error"), "warnings", m.get("warnings"))
PY
}

# member_env <deployment> <label> <NAME...>: for each member, the named environment
# variables of the process its launch spawned (the head's `api`, a worker's
# `worker-<r>`), read by proc_probe.py from its recorded identity (pid, start ticks,
# boot id) in members-<dep>-<label>.json. Only the named variables are read.
member_env() {
  local dep=$1 label=$2 rank hid pid ticks boot host envs=()
  shift 2
  for name in "$@"; do envs+=(--env "$name"); done
  dry && { log_cmd control-host "proc_probe.py ${envs[*]} on each member's spawned process"; return 0; }
  while read -r rank hid pid ticks boot; do
    host=$(host_of_id "$hid")
    [ -n "$host" ] || { echo "rank $rank: unknown host $hid"; return 1; }
    rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/proc_probe.py --pid $pid --ticks $ticks --boot $boot ${envs[*]}" \
      >"$EVID/env-$dep-$label-rank$rank.json" || { cat "$EVID/env-$dep-$label-rank$rank.json"; return 1; }
    python3 -c 'import json,sys; d=json.load(open(sys.argv[2])); print("rank", sys.argv[1], json.dumps(d.get("env")))' \
      "$rank" "$EVID/env-$dep-$label-rank$rank.json"
  done < <(python3 -c 'import json,sys
for i in json.load(open(sys.argv[1])):
    if "/" not in i["role"] and not i["role"].startswith("helper-"):
        print(i["rank"], i["host_id"], i["pid"], i["start_ticks"], i["boot_id"])' "$EVID/members-$dep-$label.json")
}

# group_ledger <deployment> <label>: `ledger.py group` to group-<dep>-<label>.json
# and a summary: plans, members per generation, and what is still held.
group_ledger() {
  local dep=$1 label=$2 f=$EVID/group-$1-$2.json
  if dry; then x python3 "$MATRIX_DIR/ledger.py" group --db "$SERVER_DB" --deployment "$dep"; return 0; fi
  python3 "$MATRIX_DIR/ledger.py" group --db "$SERVER_DB" --deployment "$dep" >"$f" || return 1
  python3 - "$f" "${HOST_ID_a:-}" "$HOST_A" "${HOST_ID_b:-}" "$HOST_B" <<'PY'
import json, sys
g = json.load(open(sys.argv[1]))
names = {sys.argv[2]: sys.argv[3], sys.argv[4]: sys.argv[5]}
for p in g["plans"]:
    canary = p.get("canary") or {}
    print("plan generation", p["generation"], p["state"], "rendezvous", f"{p['rendezvous_host']}:{p['rendezvous_port']}",
          "failed_rank", p.get("failed_rank"), "failure_code", p.get("failure_code"),
          "canary_tokens", canary.get("tokens"))
for m in g["members"]:
    print("member generation", m["generation"], "rank", m["rank"], names.get(m["host_id"], m["host_id"]), m["state"],
          "identities", [(i["role"], i["pid"]) for i in m["identities"]])
for o in g["member_owners"]:
    fp = o["footprint"]
    print("charge", o["owner_id"].rsplit("/", 1)[-1], "phase", fp.get("phase"), "allocations", fp.get("allocations"))
print("held", json.dumps(g["held"], sort_keys=True))
PY
}

# keep_group <deployment> <label>: status, ledger view, the head's owned
# identities and accounting (keep_owned), and every unsettled member's
# identities (members-<dep>-<label>.json), all under the label.
keep_group() {
  local dep=$1 label=$2
  group_status "$dep" "$label"
  group_ledger "$dep" "$label"
  keep_owned "$dep" "$label" >/dev/null
  if dry; then x python3 "$MATRIX_DIR/ledger.py" members --db "$SERVER_DB" --deployment "$dep"; return 0; fi
  python3 "$MATRIX_DIR/ledger.py" members --db "$SERVER_DB" --deployment "$dep" >"$EVID/members-$dep-$label.json"
  python3 -c 'import json,sys
for i in json.load(open(sys.argv[1])): print("identity rank", i["rank"], i["host_id"], i["member_state"], i["role"], i["pid"])' \
    "$EVID/members-$dep-$label.json"
}

# group_ready_check <deployment> <label>: from status-<dep>-<label>.json, the
# group is Ready with rank 0 on host A as head and rank 1 on host B as worker,
# both launched and charged in the ready phase.
group_ready_check() {
  dry && { log_cmd control-host "judge status-$1-$2.json: ready, rank 0 head on $HOST_A, rank 1 worker on $HOST_B, both launched, reservations ready"; return 0; }
  python3 - "$EVID/status-$1-$2.json" "${HOST_ID_a:-}" "${HOST_ID_b:-}" <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d = d.get("deployment", d)
a, b = sys.argv[2], sys.argv[3]
inst = (d.get("instances") or [{}])[0]
members = {m.get("node_rank"): m for m in inst.get("members", [])}
want = {0: (a, "head"), 1: (b, "worker")}
findings = []
if d.get("observed_state") != "ready":
    findings.append(f"observed_state {d.get('observed_state')}")
for rank, (host, role) in want.items():
    m = members.get(rank)
    if not m:
        findings.append(f"no member rank {rank}")
        continue
    if m.get("host") != host or m.get("role") != role:
        findings.append(f"rank {rank} is {m.get('role')} on {m.get('host')}, want {role} on {host}")
    if m.get("state") != "launched":
        findings.append(f"rank {rank} state {m.get('state')}")
    if (m.get("reservation") or {}).get("phase") != "ready":
        findings.append(f"rank {rank} reservation {m.get('reservation')}")
print(json.dumps({"ready": not findings, "findings": findings}))
sys.exit(1 if findings else 0)
PY
}

# wait_member <deployment> <rank> <state|!state> <timeout_s>: poll status until
# that member's state is (or, with !, is no longer) the given one.
wait_member() {
  local dep=$1 rank=$2 want=$3 timeout=$4 i state=""
  dry && { log_cmd control-host "poll status deployment $dep until member rank $rank state $want (<= ${timeout}s)"; return 0; }
  for ((i = 0; i < timeout; i += 2)); do
    state=$(status_dep "$dep" 2>/dev/null | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin); d=d.get("deployment",d)
    m=[m for i in d.get("instances",[]) for m in i.get("members",[]) if str(m.get("node_rank"))==sys.argv[1]]
    print(m[0].get("state","") if m else "")
except Exception: print("")' "$rank")
    case $want in
      !*) if [ -n "$state" ] && [ "$state" != "${want#!}" ]; then echo "rank $rank $state after ${i}s"; return 0; fi ;;
      *) if [ "$state" = "$want" ]; then echo "rank $rank $state after ${i}s"; return 0; fi ;;
    esac
    sleep 2
  done
  echo "rank $rank not $want after ${timeout}s (last: $state)" >&2
  return 1
}

# wait_group_settled <deployment> <timeout_s>: poll the ledger until every plan
# and member is settled and no member charge, reservation or worker lease is held.
wait_group_settled() {
  local dep=$1 timeout=$2 i held=""
  dry && { log_cmd control-host "poll ledger.py group $dep until held is empty (<= ${timeout}s)"; return 0; }
  for ((i = 0; i < timeout; i += 2)); do
    held=$(python3 "$MATRIX_DIR/ledger.py" group --db "$SERVER_DB" --deployment "$dep" 2>/dev/null | python3 -c 'import json,sys
try:
    h=json.load(sys.stdin)["held"]; print("" if not any(h.values()) else json.dumps(h,sort_keys=True))
except Exception: print("unreadable")')
    if [ -z "$held" ]; then echo "$dep settled after ${i}s"; return 0; fi
    sleep 2
  done
  echo "$dep still holds after ${timeout}s: $held" >&2
  return 1
}

# ports_free <host> <port...>: nothing listens on those ports on the host.
ports_free() {
  local host=$1 pat
  shift
  [ $# -gt 0 ] || return 0
  pat=$(IFS='|'; echo "$*")
  rsh "$host" "if ss -ltnH | awk '{print \$4}' | grep -E ':($pat)\$'; then echo LEFTOVER_PORT; fi; true"
}

# group_cleanup_check <deployment> <label> <ready-label>: verified cleanup of the
# whole group on both hosts (ADR 0028 §11): every member identity and the head's
# identities recorded at <ready-label> are gone on their own hosts; neither host
# holds an engine process, GPU compute process or rendezvous directory; the
# rendezvous port, the head's engine port and any worker port are free; and the
# server holds no member charge, reservation, worker lease, unsettled member or
# plan, and nothing for the deployment (accounting). The ledger records that
# each host settled its own member.
group_cleanup_check() {
  local dep=$1 label=$2 rl=$3 rc=0 pid ticks boot hid host probe port portsa=() portsb=()
  local members=$EVID/members-$dep-$rl.json owned=$EVID/owned-$dep-$rl.json
  if dry; then
    log_cmd control-host "for each identity in $members and $owned: ssh <its host> signal_owned.py --pid P --ticks T --boot B --signal 0 (want absent)"
    for host in "${GROUP_HOSTS[@]}"; do host_clean "$host"; ports_free "$host" "<port>"; done
    x python3 "$MATRIX_DIR/ledger.py" group --db "$SERVER_DB" --deployment "$dep"
    x python3 "$MATRIX_DIR/ledger.py" accounting --db "$SERVER_DB" --deployment "$dep"
    return 0
  fi
  : >"$EVID/cleanup-identities-$label.txt"
  while read -r pid ticks boot hid; do
    host=$(host_of_id "$hid")
    if [ -z "$host" ]; then echo "identity $pid on unknown host $hid"; rc=1; continue; fi
    probe=$(rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pid $pid --ticks $ticks --boot $boot --signal 0" || true)
    echo "$host $pid $probe" | tee -a "$EVID/cleanup-identities-$label.txt"
    case $probe in *'"outcome": "absent"'*) ;; *) rc=1 ;; esac
  done < <(python3 -c 'import json,sys
for path in sys.argv[1:]:
    try: ids = json.load(open(path))
    except FileNotFoundError: continue
    for i in ids: print(i["pid"], i["start_ticks"], i["boot_id"], i.get("host_id") or "-")' "$members" "$owned")
  [ -s "$EVID/cleanup-identities-$label.txt" ] || { echo "no identities were recorded at $rl"; rc=1; }
  # The ports the group held at Ready, on their hosts.
  while read -r hid port; do
    case $(host_of_id "$hid") in "$HOST_A") portsa+=("$port") ;; "$HOST_B") portsb+=("$port") ;; esac
  done < <(python3 - "$EVID/group-$dep-$rl.json" "$EVID/accounting-$dep-$rl.json" "${HOST_ID_a:-}" <<'PY'
import json, sys
g = json.load(open(sys.argv[1]))
a = json.load(open(sys.argv[2]))
head = sys.argv[3]
plan = next((p for p in reversed(g["plans"])), None)
if plan:
    print(head, plan["rendezvous_port"])
for w in g["worker_leases"]:
    print(w["host_id"], w["port"])
for e in a["endpoint_leases"]:
    print(e["host_id"], e["port"])
PY
)
  echo "ports held at $rl: $HOST_A ${portsa[*]:-none}, $HOST_B ${portsb[*]:-none}"
  for host in "${GROUP_HOSTS[@]}"; do
    {
      host_clean "$host"
      if [ "$host" = "$HOST_A" ]; then ports_free "$host" "${portsa[@]}"; else ports_free "$host" "${portsb[@]}"; fi
    } | tee "$EVID/cleanup-host-$(host_short "$host")-$label.txt"
    grep -q LEFTOVER "$EVID/cleanup-host-$(host_short "$host")-$label.txt" && rc=1
  done
  group_ledger "$dep" "$label" | tee "$EVID/cleanup-group-$label.txt"
  python3 -c 'import json,sys; h=json.load(open(sys.argv[1]))["held"]; sys.exit(1 if any(h.values()) else 0)' \
    "$EVID/group-$dep-$label.json" || { echo "FINDING: the group still holds"; rc=1; }
  accounting "$dep" >"$EVID/accounting-$dep-$label.json"
  python3 - "$EVID/accounting-$dep-$label.json" <<'PY' || rc=1
import json, sys
a = json.load(open(sys.argv[1]))
held = {k: a[k] for k in ("reservations", "resource_owners", "lifecycle_claims", "request_leases_deployment", "retained_bindings", "endpoint_leases") if a[k]}
print(json.dumps({"held": held, "deployment": a["deployment"]}))
sys.exit(1 if held else 0)
PY
  return "$rc"
}

# journal_hosts <deployment> <label>: which host recorded each journal entry
# (from evidence-<dep>-<label>.json): each member settles on its own host's evidence.
journal_hosts() {
  dry && return 0
  python3 - "$EVID/evidence-$1-$2.json" "${HOST_ID_a:-}" "$HOST_A" "${HOST_ID_b:-}" "$HOST_B" <<'PY'
import json, sys
e = json.load(open(sys.argv[1]))
names = {sys.argv[2]: sys.argv[3], sys.argv[4]: sys.argv[5]}
for j in e["journal"]:
    print("journal", j["recorded_at"], names.get(j["host_id"], j["host_id"]), j["state"], j["operation_id"])
PY
}

# journal_count <deployment> <label> <state>: journal entries in that state
# (`parked`, `restored`) in evidence-<dep>-<label>.json.
journal_count() {
  if dry; then echo 0; return 0; fi
  python3 -c 'import json,sys
print(sum(1 for j in json.load(open(sys.argv[1]))["journal"] if j["state"] == sys.argv[2]))' "$EVID/evidence-$1-$2.json" "$3"
}

# mem_both <label>: MemAvailable on both hosts, as a-<label> and b-<label>.
mem_both() {
  local host
  for host in "${GROUP_HOSTS[@]}"; do host_mem "$host" "$(host_short "$host")-$1"; done
}

# rdma_read <label>: each host's RDMA port counters (rdma_counters.py, read-only).
rdma_read() {
  local host out
  for host in "${GROUP_HOSTS[@]}"; do
    out=$(rsh_out "$host" '{"present": false}' "python3 $REMOTE_TREE/scripts/live/matrix/rdma_counters.py") || return 1
    dry || printf '%s\n' "$out" >"$EVID/rdma-$(host_short "$host")-$1.json"
    echo "rdma $host $1 $(printf '%s' "$out" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps(d.get("total", d)))')"
  done
}

# rdma_transport <before> <after>: which transport carried the group's traffic,
# from the counter rise on both hosts (design section 17.2: counters rise on both
# hosts, else socket). `rdma` needs data on both hosts' ports and, where the
# driver exposes RDMA operation counters, a rise in them too (a RoCE port also
# counts Ethernet). Writes transport.json and prints `transport: rdma|socket`
# with the devices that carried traffic; socket transport is recorded, not failed.
rdma_transport() {
  local before=$1 after=$2 host
  if dry; then log_cmd control-host "rdma_counters.py delta rdma-<a|b>-$before.json rdma-<a|b>-$after.json; write transport.json"; return 0; fi
  for host in a b; do
    python3 "$MATRIX_DIR/rdma_counters.py" delta "$EVID/rdma-$host-$before.json" "$EVID/rdma-$host-$after.json" >"$EVID/rdma-$host-delta.json"
  done
  python3 - "$EVID/rdma-a-delta.json" "$EVID/rdma-b-delta.json" "$EVID/transport.json" <<'PY'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
def carried(d):
    return bool(d.get("carried")) and (d.get("rdma_operations") or not d.get("operations_known"))
transport = "rdma" if carried(a) and carried(b) else "socket"
basis = "rdma operation counters" if a.get("operations_known") and b.get("operations_known") else "port data counters"
verdict = {"transport": transport, "basis": basis,
           "host_a": {"carried": a.get("carried"), "rdma_operations": a.get("rdma_operations"), "devices": a.get("devices")},
           "host_b": {"carried": b.get("carried"), "rdma_operations": b.get("rdma_operations"), "devices": b.get("devices")}}
json.dump(verdict, open(sys.argv[3], "w"), indent=1)
print(f"transport: {transport} ({basis}); devices host A {a.get('carried')} host B {b.get('carried')}")
PY
}

# completions <route> <n> <label>: n routed completions with distinct, checkable
# answers; prints "completions <label> <ok>/<n>" and fails unless all are right.
completions() {
  local route=$1 n=$2 label=$3 i ok=0 a
  for ((i = 1; i <= n; i++)); do
    a=$((16 + i))
    if infer "$route" "What is $a+25? Answer with only the number." --expect "$((a + 25))" --max-tokens 1024; then ok=$((ok + 1)); fi
  done
  echo "completions $label $ok/$n"
  dry || [ "$ok" = "$n" ]
}

# host_offline_wait <host> <timeout_s>: the server sees the host's session gone.
host_offline_wait() {
  local host=$1 timeout=$2 i
  dry && { log_cmd control-host "poll list hosts until $host is offline (<= ${timeout}s)"; return 0; }
  for ((i = 0; i < timeout; i += 2)); do
    if cli list hosts --format json 2>/dev/null | python3 -c 'import json,sys
h=[h for h in json.load(sys.stdin).get("hosts",[]) if h.get("name")==sys.argv[1]]
sys.exit(0 if h and not h[0].get("online") else 1)' "$host"; then echo "$host offline after ${i}s"; return 0; fi
    sleep 2
  done
  echo "$host still online after ${timeout}s" >&2
  return 1
}

# group_stop_settle <deployment> <label>: stop the group, wait until it is stopped
# and settled on both hosts, with the status and ledger kept under the label.
group_stop_settle() {
  local dep=$1 label=$2 rc=0
  step "stop-$label" stop_dep "$dep" || rc=1
  step "stopped-$label" wait_states "$dep" "stopped" 900 || rc=1
  step "settled-$label" wait_group_settled "$dep" 300 || rc=1
  step "status-$label" group_status "$dep" "$label"
  return "$rc"
}
