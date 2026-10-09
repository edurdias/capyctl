# Multi-node engine groups — design

Date: 2026-10-05. Status: draft for owner review. It supersedes
`docs/specs/2026-09-25-multi-host-groups-design.md` and its plan: the owner's
2026-09-25 decisions still hold, decision 12 changed on 2026-10-05 (SGLang, vLLM
and TensorFold together), and the code was renamed to CapyCTL and moved on since.

It will be recorded as ADR 0028, amending SPEC §11, §15, §16.4 and §20, ADR 0013
(decision 1, the multi-host refusal), ADR 0012 (peer exposure of a group run),
ADR 0023 (TensorFold multi-rank was out of scope) and the discrete-GPU design's
`multi_gpu_unsupported` rule.

Implementation plan: `docs/plans/2026-10-05-multi-node-groups.md`.
Implementation status: not started.

## Problem

A model that does not fit one machine must run as one engine spread over several
machines: tensor parallel (TP) splits each layer across ranks, pipeline parallel (PP)
splits the layers into stages. CapyCTL runs every engine on one host today:

- The domain has a two-host `GroupPlan` and wire messages (`Prepare`, `Launch` with a
  `GroupLaunchPlan`), but nothing builds or sends them. The agent's native path
  refuses both (`NativeHostExecution::authorize` and `render_launch` answer
  `Unauthorized`).
- Configuration has no `topology`; ADR 0013 decision 1 refuses a multi-host group;
  `multi_gpu_unsupported` refuses more than one device on a discrete host.
- Every multi-node engine flag is reserved for all three engines, the single-rank
  rendezvous is pinned to loopback (`runtime/loopback_rendezvous.py`), and the
  vLLM frozen plan pins `tensor_parallel_size: 1`.
- Group reservation, fan-out launch, per-rank settlement and recovery (units U6, U7
  and U9 of the 2026-09-21 two-host plan) were never built.

Users notice: half of the pinned model catalog (models 8 to 16) needs two or more
machines, and a downstream integration lists "Multi-machine serving: multi-host
groups are not started" as an open gap.

## Owner decisions (binding)

From 2026-09-25, still valid:

1. **Engines.** Engine virtual environments are registered with `capyctl engine add
   --name`, the same name on every host. Bring-up first uses a model the stock
   engine loads.
2. **Residency, amended 2026-10-06.** vLLM and SGLang groups deep-park from day one:
   group-wide sleep and wake with evidence from every rank. TensorFold groups are
   restart-only (no sleep or wake), as the engine's group-support data says.
3. **Peer exposure.** Trust the network. CapyCTL does no firewall check. Rendezvous
   and NCCL ports are open on all interfaces during a run. CapyCTL binds to the
   direct link where the engine allows it. Recorded as a known risk.
4. **NCCL.** CapyCTL sets no NCCL setting; NCCL chooses the transport.
5. **Readiness.** Head health only. Other hosts record their rank's process
   identities for ownership and stop evidence.
6. **Failure.** Any rank failure stops the whole group. Each host releases only on
   its own evidence.
7. **Ownership.** One owner per rank. N-rank groups, TP × PP across N hosts.
8. **Placement.** Named hosts only, in rank order, head first. The scheduler never
   chooses hosts for a group.
9. **Rendezvous port.** Allocated by the server from a host-declared range (default
   25000–25099), recorded in the group plan, released only after every host proves
   stop.
10. **Host tuning.** The compaction sysctl, memlock and `/dev/infiniband` access are
    set by the owner as root. CapyCTL checks them, warns or refuses, and never
    changes them.
11. **Weights.** Every named host downloads the model source in parallel. Digests are
    compared across hosts before launch. Free disk is checked first.
12. **Engines, changed 2026-10-05.** SGLang, vLLM and TensorFold together, in one
    group mechanism. Use the latest installed builds: vLLM 0.30.0, SGLang 0.5.21,
    TensorFold 0.6.5. Ideally every catalog model runs on every engine that can
    serve it.
13. **Success bar.** Through the router, within about 10% of the reference recipe's
    averages. Peaks are reported, not required. For vLLM the reference is a
    same-build baseline (decided 2026-10-06; §17.2).

Engine environment (2026-09-25): settings an engine reads only from the
environment are declared in YAML (the profile's `env`, the deployment's
`engine_config.env`) or by a flag (`capyctl engine add --env K=V`, `capyctl deploy
--engine-env K=V`). Names are checked against the profile's approved list.
CapyCTL-owned names (`NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP`) are never
settable.

Design choices the owner settled on the 2026-09-25 design (PR #42), carried
forward unchanged: addresses are not transport (CapyCTL renders the head's peer
address and each host's own peer address, nothing named `NCCL_*` or `GLOO_*`); host
checks warn by default and refuse under `require_rdma: true`; group eviction is
all-or-nothing across the named hosts; `instances: 1` and one device per host in the
first version; a post-wake canary for groups.

Standing rules that shape this design: every setting comes three ways (YAML, CLI
flag, environment variable; flag > env > YAML > default); standalone is a server
plus one host, so a group (two or more hosts) needs the server and host roles;
existing engine installations only, no engine install and no containers yet.

## Design

### 1. Vocabulary

- **Group**: one engine instance whose ranks run on more than one host. It is still
  one ADR 0013 instance: one generation, one route endpoint, one lifecycle claim.
- **Member**: the part of a group on one host. Member 0 is the **head**: it serves
  HTTP. Every other member is a **worker**: it serves no inference.
- **Node rank**: the member's position in `placement.hosts` (head = 0).
- **Local ranks**: devices one member uses. First version: 1.
- **Group plan**: the durable, server-written record of one group incarnation:
  engine, topology, every member (host, node rank, role, devices, model path, peer
  address), the head's service port, each worker's loopback port where its engine
  opens one, the rendezvous port and the generation. A plan is never edited; a
  relaunch is a new generation with a new plan.

### 2. Deployment configuration

```yaml
schema_version: 1
kind: deployment
name: flash-next
engine: sglang                  # profile name; must resolve on every named host
model: {hf: RadixArk/Qwen3.8-Flash-Next-NVFP4@7b719225}
topology:
  tensor_parallel: 2
  pipeline_parallel: 1
placement:
  hosts: [host-a, host-b]       # exact hosts, rank order, head first
residency: deep
resources: {gpu: 100GiB, ram: 4GiB}   # per member, charged on each member's host
engine_config:
  context_length: 131072
  env: {SGLANG_ENABLE_JIT_DEEPGEMM: "0"}  # names approved by every host's profile
timeouts: {initialize: 1800s}
```

Rules, checked at deploy time (codes in §16):

- `topology` is optional. Absent, or `tensor_parallel × pipeline_parallel = 1`, means
  a single-host instance with a byte-identical command identity, effective
  configuration and stored digests.
- With a world size `W = tensor_parallel × pipeline_parallel > 1`, `placement.hosts`
  is required, has at least two entries and no duplicates (after trimming), and its
  length `N` divides `W`; local ranks are `W / N`. First version: `W / N = 1`
  (`group_shape_unsupported`).
- `host:`, `placement.selector`, `strategy` and `max_per_host` are refused with a
  topology (`group_placement_required`). `instances` must be 1
  (`group_instances_unsupported`).
- `devices` states one device per host; a named id applies to every host.
- `resources` (and `engine_config.memory`) are **per member**; §5.
- Users never write a multi-node flag. They stay reserved; CapyCTL renders them from
  `topology` (§10).
- The engine profile name must resolve on every named host with the same build
  fingerprint (`group_profile_mismatch`). Ranks of different builds cannot form a
  group.
- The engine must support the shape (`group_shape_unsupported:<engine>`, table
  below). A standalone role refuses a topology (`group_placement_required`: one
  host).
- Shapes beyond two hosts and PP > 1 are allowed, with no gate and no warning,
  wherever the engine's support data admits them (decided 2026-10-06). Live
  qualification covers two hosts.

| Shape and feature | vLLM 0.30.0 | SGLang 0.5.21 | TensorFold 0.6.5 |
|---|---|---|---|
| TP across hosts | yes | yes | `tp` 2 only |
| PP across hosts | yes | yes | no |
| Hosts | any N dividing TP × PP | any N dividing TP × PP | exactly 2 |
| Deep park | yes (sleep mode on every rank) | yes (memory saver on every rank) | no (restart-only engine) |
| Worker listens on | nothing (headless) | a loopback dummy health port | nothing |

The effective revision of a group records the per-host resolutions it was built
from.

### 2.1 Engine environment

Implemented here for every deployment, single-host or group (none of it exists
yet: today a profile `env` is a closed list and a deployment `env` is refused).

| Level | YAML | CLI flag | Saved in |
|---|---|---|---|
| Engine profile | `env:` in `engines.yaml` or `host.yaml` | `capyctl engine add ... --env K=V` | the profile |
| Deployment | `engine_config.env:` | `capyctl deploy ... --engine-env K=V` | the deployment revision |
| Approval | `security.approved_env:` (names or globs) | `capyctl engine add ... --approve-env GLOB` | the profile |

Rules:

1. **CapyCTL-owned names are never settable**, at either level, whatever
   `approved_env` says: `NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP` (the owner's
   list); the names CapyCTL already renders or closes (`PATH`, `LD_*`,
   `PYTHONPATH`, `CUDA_HOME`, `CUDA_VISIBLE_DEVICES`, `CAPYCTL_*`); and, by the same
   rule, the address and rendezvous names of the other two engines: `SGLANG_HOST_IP`,
   `HOST_IP`, `SGLANG_LOCAL_IP_NIC`, `SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE`,
   `TF_COMM_BACKEND`, `TF_NCCL_LIB`. Refused `engine_env_reserved:<name>`; an
   approval entry that matches only owned names is refused too.
2. **Approval.** A deployment name must match an `approved_env` entry of the profile
   it resolves against (`engine_env_not_approved:<name>`). The profile's own `env` is
   host-authored and needs no approval, except for rule 1. The existing safe names
   (`MAX_JOBS`, `FLASHINFER_NVCC_THREADS`, `TOKENIZERS_PARALLELISM`,
   `PYTHONUNBUFFERED`, `RUST_LOG`) keep their rules at both levels.
3. **Globs.** An entry is upper-case `A`–`Z`, `0`–`9`, `_`, optionally ending in one
   `*`. A bare `*` is refused. Rule 1 is checked on the concrete name.
4. **Precedence.** The deployment's value overrides the profile's; the effective
   configuration shows each variable with its source.
5. **One way per document.** A name given both in the file and by a flag is
   `engine_env_conflict:<name>`.
6. **Groups.** Every named host resolves against its own profile; a name not
   approved on one host refuses the group. The rendered environments of all members
   are equal apart from the per-host address variable (§10).
7. **Values** are bounded (4 KiB each, 64 names per level), single-line, and shown in
   status and the effective configuration. They are not secret storage.
8. **Changes.** A deployment env change is a new revision; a profile env change
   changes the recipe fingerprint.

### 3. Host groups policy

Every setting three ways (flag > env > YAML > default):

| Setting | YAML (`host.yaml`) | Flag (`capyctl start host`) | Environment | Default |
|---|---|---|---|---|
| Peer address | `resource_policy.groups.peer_address` | `--peer-address <ip>` | `CAPYCTL_PEER_ADDRESS` | none |
| Rendezvous ports | `resource_policy.groups.rendezvous_port_range` (`start`, `end`) | `--rendezvous-ports <start-end>` | `CAPYCTL_RENDEZVOUS_PORTS` | `25000-25099` |
| Refuse on missing RDMA tuning | `resource_policy.groups.require_rdma` | `--require-rdma` | `CAPYCTL_REQUIRE_RDMA` | `false` |

- A host without a peer address cannot be named in a group (`peer_address_missing`).
- The peer address must not be loopback, unspecified or multicast. The agent verifies
  at start that it is assigned to a local interface and reports it in its inventory;
  a mismatch refuses group work on that host (`peer_address_not_local`).
- Port ranges need `1024 <= start <= end`.
- A host document without the block keeps its policy digest.

### 4. Group plan

`GroupPlan::two_host` becomes `GroupPlan::new`, validating N members:

- members in node-rank order 0..N-1, distinct hosts, exactly one head (rank 0);
- member ids `head`, `worker-1`, ..., `worker-<N-1>`;
- the same engine, profile fingerprint and checkpoint fingerprint on every member;
- distinct peer addresses, none loopback, unspecified or multicast;
- the head has a nonzero service port; a worker has a loopback port only when its
  engine opens one (SGLang);
- a nonzero rendezvous port and a positive generation;
- `tensor_parallel × pipeline_parallel = N × local_ranks`.

`MemberPlan` gains `role`, `model_path` (the member host's materialized path) and
`worker_port: Option<u16>`; `service_port` becomes `Option<u16>`. The plan carries
`engine` (`vllm`, `sglang`, `tensorfold`), the topology and the generation. It is
written durably with the reservation (§5) before any host is contacted.

### 5. Reservations, memory per rank, rendezvous port

- **Owners per member**: `deployment:<id>/instance:<k>/member:<r>`, each charged on
  its own host's memory domains and device. The instance row points at the group
  plan; the plan names the owners.
- **Memory per rank.** A member's footprint is the deployment's `resources` (or the
  phases derived from `engine_config.memory`), the same for every member, charged on
  its host like a single-host instance (ADR 0007 phases, ADR 0013 managed limit and
  free reserve). The head's API process is inside that figure; the operator sizes
  for the head. On unified-memory hosts (GB10) the device and system domains are the
  same pool, as today. TensorFold's memory cap (ADR 0025) applies per rank.
- **All or nothing.** Every member is reserved in one store transaction under
  ADR 0007 (fresh observations, epoch compare-and-swap on every named host). Either
  every member is reserved or none is, so two group plans sharing a host cannot
  deadlock on partial acquisition. This is atomic accounting in the server's store,
  not a cross-host atomic launch.
- **Rendezvous port** is drawn in the same transaction: the lowest port in the
  head's range not held by an unsettled group plan on that host
  (`rendezvous_ports_exhausted`). SGLang worker loopback ports come from each
  worker host's existing `endpoint_port_range`, through the existing endpoint lease
  table.
- **Eviction.** If a member does not fit, the planner computes a victim set on each
  named host with the existing per-host rules and evicts only when every host has
  one. A group chosen as a victim is parked or stopped whole.
- Each member settles on its own host's evidence only (§11).

### 6. Weights: parallel download and digest agreement

1. The model source is materialized on every named host at once through the
   existing path (`MaterializeSource`). Each host checks free space against the
   source's size first (`insufficient_space`, naming the host).
2. When every host has finished, each reports its checkpoint digest. They must be
   equal (`group_checkpoint_mismatch`, naming each host and digest). Nothing launches
   on a mismatch.
3. The store's checkpoint digest record is keyed by deployment and revision with one
   host today; it becomes one row per host.
4. Each member's own `model_path` goes into the plan. Before the reservation, a
   SGLang group with deep park whose members' paths differ is refused
   (`group_model_path_mismatch`, naming the head's path and the member's path):
   CapyCTL's SGLang wake posts the head's path to every rank (§12). Nothing is
   reserved, so nothing is released. Restart-only SGLang groups, vLLM and TensorFold
   keep differing paths (decided 2026-10-06).

### 7. Prepare and host checks

After the reservation commits, `Prepare(GroupPlan)` goes to every member. It has no
process effect. Each host checks: the profile resolves with the recorded
fingerprint; the model path holds the recorded digest; the peer address is local; on
the head, the rendezvous port is free on the peer address and the service port on
loopback; on the head of a SGLang group whose deployment enables DP attention, also
the seven fixed ports SGLang derives from the rendezvous port `P` and binds there
(`P+1` to `P+6` and the handshake at `P+13`, as SGLang 0.5.21 computes them; SGLang
moves the six to `P-7` to `P-2` when `P+6` exceeds 65535 but never moves `P+13`, so
any `P` above 65522 is refused; the per-rank ephemeral PUSH sockets are not checked;
checked when the `Launch` re-runs these checks, because only the `Launch` carries the
deployment document); on a SGLang worker, its loopback port is free; and the host
tuning:

| Check | How | Default | `require_rdma: true` |
|---|---|---|---|
| `compaction` | `/proc/sys/vm/compaction_proactiveness` nonzero | warn | warn |
| `memlock` | the agent's `RLIMIT_MEMLOCK` below unlimited | warn | refuse |
| `infiniband` | `/dev/infiniband/uverbs*` absent or not read-write for the service user | warn | refuse |

Warnings are `host_tuning_warning:<item>`, refusals `host_tuning_missing:<item>`.
CapyCTL reads these and never writes them. Any refusal releases every member's
reservation (nothing was launched) and records the refusal per host. `Prepare` is
idempotent and retried under the same command id.

### 8. Fan-out launch

Once every member has prepared, `Launch(GroupPlan)` goes to every member
concurrently. Workers are not held back until the head is ready: every engine's
initialization waits for all ranks (vLLM and SGLang at the torch store, TensorFold at
its TCP store, which waits up to 600 s). The TensorFold runbook's "start rank 1 first"
is a manual-start convenience; a concurrent start meets the same rendezvous.

Each host journals the launch before spawning, records its member's process
identities (head: API server and engine processes; worker: its process tree) and
reports them. A host that cannot launch reports a typed failure. Any member failing
to launch, or exiting before readiness, stops the group (§11).

### 9. Readiness and the router

The group is READY when the head passes the existing native readiness check for its
engine (model readiness, not HTTP liveness) and then one bounded 1-token completion
through the head (decided 2026-10-06). A TP or PP forward cannot complete without
every rank, so the completion proves the collective works. A failed or timed-out
probe is a launch failure and stops the group (§11). The controller sends the probe
to the head's agent as an additive extension of the existing member probe; the agent
runs it on loopback with the per-launch key and returns the generated token ids. It
never goes through ingress or the router, and worker hosts never receive one. One probe
function serves readiness, the request-stall check (§11) and the wake canary (§12).
Worker hosts never probe readiness; SGLang's nonzero-rank health server always
answers 200 and is never read. The router routes only to the head's ingress, opens
the route only on READY (T30), and treats a group as one replica in balancing and
failover. The initialization timeout is the deployment's `timeouts.initialize`.

### 10. Engine adapters: one mechanism, three renderings

The controller, store, agent and protocol know only the group plan. Each adapter
turns one member of a plan into its engine's command through one shared input:

```rust
pub struct GroupMemberArgs {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
    pub nnodes: u32,
    pub node_rank: u32,
    pub head_address: IpAddr,
    pub rendezvous_port: u16,
    pub own_address: IpAddr,
    pub worker_port: Option<u16>,
}
```

and declares its support (`GroupSupport`: shapes, deep park, worker listener) in
one function the configuration layer reads for the §2 checks.

| | vLLM 0.30.0 | SGLang 0.5.21 | TensorFold 0.6.5 |
|---|---|---|---|
| Parallel sizes | `--tensor-parallel-size T --pipeline-parallel-size P` | `--tp-size T --pp-size P` | `--tp 2` |
| Rank and world | `--nnodes N --node-rank r` | `--nnodes N --node-rank r` | `--rank r` |
| Rendezvous | `--master-addr <head> --master-port <port>` | `--dist-init-addr <head>:<port>` | `--master <head> --master-port <port>` |
| Backend | `--distributed-executor-backend mp` (Ray is refused upstream for multi-node) | default | NCCL (built in) |
| Worker | `--headless`; no host, port, key or middleware | same command, `--host 127.0.0.1 --port <worker_port>` | no host or port |
| Own address | `VLLM_HOST_IP=<own>` | `SGLANG_HOST_IP=<own>` | none (the store is on the head) |
| Deep park flag | `--enable-sleep-mode` on every rank | `--enable-memory-saver` on every rank | not applicable |
| Head serves | loopback API, keys and guard, as single-rank | same | same |

Common rules:

- Nothing named `NCCL_*` or `GLOO_*` is rendered or inherited (decision 4). The
  single-rank loopback pin (`GLOO_SOCKET_IFNAME=lo`, `NCCL_SOCKET_IFNAME=lo`,
  `VLLM_HOST_IP=127.0.0.1`, SGLang's file store) is not applied to a group launch.
- The protected entries (`runtime/vllm_entry.py`, `runtime/sglang_server_args.py`
  and `runtime/sglang_entry.py`) gain a group mode: CapyCTL passes the rendered
  multi-node values (`CAPYCTL_GROUP_EXPECTED`) and the entry refuses any drift
  (`group_drift:<field>`). The vLLM frozen plan's `tensor_parallel_size: 1` pin and
  SGLang's `tp_size: 1, nnodes: 1, node_rank: 0` pins take the plan's values.
  TensorFold has no protected entry today; its argv is checked in the adapter.
- The recipe's own flags stay deployment `engine_config` or approved extra args, as
  for a single-rank launch, and must be equal on every member (TensorFold requires
  equal context, drafting and `--parallel`).

Per-engine notes:

- **vLLM.** With `nnodes > 1` vLLM chooses `mp`; `--headless` runs a bare executor
  that joins the head's broadcast queue. The queue binds `VLLM_HOST_IP`, which is why
  CapyCTL renders it; otherwise vLLM picks the default-route interface. Sleep and
  wake are collective calls from the head that reach every rank.
- **SGLang.** Rank > 0 starts a dummy health server on `--host:--port`, hence the
  worker loopback port (two groups on one host would otherwise collide on the
  default 30000). Rank > 0 ignores SIGTERM and waits for rank 0's shutdown, so its
  stop normally escalates to SIGKILL (§11). Release and resume go to rank 0, which
  broadcasts to every TP rank; cross-node behavior is inferred from source and proven
  only by the live row.
- **TensorFold.** Two ranks, one GPU each. Rank 1 prints "rank 1 ready ... following
  rank 0" and serves nothing. It has no sleep, so a TensorFold group is
  `restart_only`: park stops both ranks and wake relaunches the group. Which
  checkpoint formats load at two ranks is family-specific (§17.2).

### 11. Stop, failure and settlement

- **Stop** (operator, idle, switch victim): close the head's ingress and drain as
  today, then send `Terminate` to every member concurrently. Each host terminates its
  journaled process tree with the existing SIGTERM, bounded wait, SIGKILL escalation,
  and reports gone-evidence. An escalation on a SGLang worker is expected and
  recorded, not an error.
- **Rank failure.** Any member's exit, launch failure or failed readiness (the
  readiness probe included) marks the group failed (`group_member_failed`, with host
  and node rank) and stops every other member. No rank replacement: an NCCL group
  cannot re-admit a rank.
- **Request stall** (decided 2026-10-06). Most group faults leave every process
  alive (a dropped link, a GPU fault, a rank stuck in NCCL). When a group request gets
  no first token within the stall timeout, the controller runs one probe through the
  head (§9), bounded at 60 s. If the probe fails too, the group is failed
  (`group_stalled`) and stopped on every host; if it passes, nothing is stopped. An
  idle group is never probed: there is no periodic polling. The timeout is a server
  setting, three ways: `groups.stall_timeout` in the server document,
  `--group-stall-timeout` on `capyctl start server`, `CAPYCTL_GROUP_STALL_TIMEOUT`;
  default 120 s.
- **Settlement.** Each member's reservation is released only on its own host's
  gone-evidence. The instance leaves its lifecycle step only when every member has
  settled.
- **Unreachable host.** Its member stays charged and `uncertain`
  (`group_member_uncertain`). Lease expiry never frees it. It settles when the host
  reconnects and reconciles its journal, or when the host is revoked and recovered
  (ADR 0016). A host that comes back with an empty journal settles only on recorded
  identities.
- **Rendezvous port** is released only after every member settles: a surviving rank
  may still hold it.
- **Recovery.** Under `recovery: reconcile`, a failed group relaunches as a new
  generation and plan only after every member of the old one has settled. Attempts
  count per instance.

### 12. Park and wake

The head's agent is the lead: it invokes each collective once through the head's
loopback control endpoint. Workers never receive a sleep call.

- **Park (deep), vLLM and SGLang.** Close ingress, drain, then the head calls
  `/sleep` (vLLM, admin key) or `/release_memory_occupation` (SGLang). The park
  settles only when the call succeeded **and** every member's host reports its
  member's resident memory at or below the member's parked budget
  (`process_residency`, now per member). A member that reports nothing within the park
  deadline keeps its full charge and the group is uncertain; a member still resident
  fails the park. Either stops the group. The collective is never repeated.
- **Wake.** The head calls `/wake_up` or `/resume_memory_occupation`; every host
  reports its member resident again; the head readiness check passes; and the canary
  matches: a short deterministic completion through the head probe (§9,
  `temperature: 0`, 8 tokens) whose tokens equal exactly the reference recorded at
  first readiness, compared by one function. A mismatch is `group_wake_mismatch` and
  stops the group, which relaunches under recovery. The exact-token rule is revisited
  at MN4 with live evidence (decided 2026-10-06).
- **SGLang wake path.** CapyCTL's SGLang wake posts the head's model path, and every
  rank loads it from its own disk, which is why differing paths are refused for a
  deep SGLang group (§6).
- **Restart-only groups.** Park and wake follow the deployment's effective residency,
  not the engine. A group whose effective residency is `restart_only`, on any engine,
  parks by a group stop and wakes by a group relaunch. TensorFold supports only
  `restart_only`, so that is its default; `residency: deep` on a TensorFold group is
  refused `capability_missing:deep_park` at deploy. A `deep` group uses the collective
  park and wake above. SGLang groups have no restart-only fallback (decided
  2026-10-06): MN4's evidence revisits only the canary rule.
- Parked members stay charged at their parked budget on their own hosts; `max_parked`
  counts a group once on each host it occupies.

### 13. Ports and peer exposure (security)

- The head's service port comes from its `endpoint_port_range` and stays on loopback,
  behind the per-launch keys and the key guard, reachable only through host ingress
  and the router (ADR 0012 unchanged).
- The rendezvous port comes from the head's `groups.rendezvous_port_range`.
- Engines also open listeners CapyCTL does not choose: the torch TCP store on the
  rendezvous port binds every interface; vLLM's broadcast queue binds an ephemeral
  port on `VLLM_HOST_IP`; TensorFold Flash Next under `--parallel` opens one extra
  ephemeral port on rank 0; SGLang with DP attention binds six ports derived from the
  rendezvous port on the head (§7); NCCL uses dynamic ports. None is authenticated.
- CapyCTL narrows what it can: it renders the direct-link addresses where the engine
  takes them (head address, `VLLM_HOST_IP`, `SGLANG_HOST_IP`), keeps every API and
  control endpoint on loopback, and exposes nothing through ingress or the router.

**Risk statement** (ADR 0012 amendment): during a group run, unauthenticated
rendezvous, broadcast and NCCL listeners are reachable on every interface of every
member host, including any wireless or overlay network such as a tailnet. Anyone who
can reach those ports can disturb or crash the group, and may be able to inject
tensors into it. CapyCTL performs no firewall check; the operator is responsible for
the network (owner decision 3). Status marks a group instance `peer transport:
unauthenticated`, and the docs say to keep group hosts on a private link.

### 14. Protocol and capability

- A new ADR 0017 capability, `engine_groups` (server to host), covers `Prepare` and
  `Launch` of a group plan with the new member fields, member-keyed `Terminate`,
  `Park`, `Restore` and residency reports, and the host's groups inventory (peer
  address, check findings). `capabilities::required` gains the `Prepare` and `Launch`
  cases. A host without it is refused `host_capability_missing:engine_groups` and
  receives nothing. Drain-only hosts may terminate a member but never prepare or
  launch one.
- Changes are additive: new fields on new numbers; `PROTOCOL_VERSION` stays `"2"`,
  `COMMAND_ENCODING_VERSION` stays `"1"`.
- Commands carry the plan's member id. The hard-coded `"head"` member ids in the
  controller (`remote_execution.rs`, `remote_readiness.rs`, `checkpoint_digests.rs`,
  `model_sources.rs`) take the member id from the plan.

### 15. Status and CLI

No new command: `deploy`, `start`, `park`, `stop` and `delete` act on a group as a
unit. Text status (JSON with `--json` carries the same fields):

```text
flash-next   ready   sglang 0.5.21   TP 2 x PP 1 on 2 hosts
  route         flash-next (head host-a)
  rendezvous    192.0.2.10:25000
  peer transport unauthenticated (keep group hosts on a private link)

  RANK  HOST    ROLE    STATE    GPU      RAM    PROCESSES
  0     host-a  head    ready    100 GiB  4 GiB  3
  1     host-b  worker  running  100 GiB  4 GiB  2

  warnings  host-b: host_tuning_warning:compaction
```

A failed member shows `failed` with its last error; an unreachable host's member
shows `uncertain` and keeps its charge. `capyctl validate config --host <host
document>`, with `--host` repeated once per named host, resolves a group deployment
against every named host's document: peer address, profile resolution and build
fingerprint, and engine environment approvals, with the deploy-time codes of §16
(decided 2026-10-06).

### 16. Error codes

| Code | Where | Exit | Meaning |
|---|---|---|---|
| `group_placement_required` | deploy | 2 | topology without exact `placement.hosts`, with `host`, `selector`, `strategy` or `max_per_host`, or on standalone |
| `group_topology_invalid` | deploy | 2 | duplicate hosts, a zero dimension, or a host count that does not divide TP × PP |
| `group_shape_unsupported` | deploy | 5 | more than one local rank per host (first version) |
| `group_shape_unsupported:<engine>` | deploy | 5 | the engine cannot run this shape (TensorFold other than TP 2 on 2 hosts) |
| `group_instances_unsupported` | deploy | 5 | `instances` > 1 with a topology |
| `group_profile_mismatch` | deploy, prepare | 2 | the profile does not resolve on a named host, or its build differs |
| `group_checkpoint_mismatch` | weights, prepare | 2 | checkpoint digests differ between hosts |
| `group_model_path_mismatch` | weights, before reservation | 2 | a SGLang group with deep park whose members' model paths differ; names the head's path and the member's path |
| `peer_address_missing` | deploy | 2 | a named host declares no peer address |
| `peer_address_not_local` | host start, prepare | 2 | the declared address is not on a local interface |
| `rendezvous_ports_exhausted` | reservation | 4 | every port in the head's range is held |
| `rendezvous_port_in_use:<port>` | prepare | 4 | the allocated rendezvous port is taken outside CapyCTL |
| `service_port_in_use:<port>` | prepare | 4 | the head's service port or a worker's loopback port is taken |
| `host_tuning_warning:<item>` | prepare, status | none | a host check found a gap |
| `host_tuning_missing:<item>` | prepare | 4 | the same, under `require_rdma: true` |
| `engine_env_reserved:<name>` | deploy, engine add, host start | 2 | a CapyCTL-owned name |
| `engine_env_not_approved:<name>` | deploy | 2 | no approval matches (on some named host, for a group) |
| `engine_env_conflict:<name>` | deploy, engine add | 2 | a name both in the document and by flag |
| `group_drift:<field>` | launch | 5 | a protected entry saw a multi-node value different from the rendered one |
| `group_member_failed` | status | none | a member exited or failed; carries host and node rank |
| `group_member_uncertain` | status | none | a member's host is unreachable; its share stays charged |
| `group_wake_mismatch` | wake | none | the post-wake canary differed |
| `group_stalled` | serving | none | a group request got no first token within the stall timeout and the head probe failed too; the group is stopped |
| `host_capability_missing:engine_groups` | placement | 5 | a named host cannot run group members |

Existing codes keep their meaning (`insufficient_space`, the insufficient-resources
family, `capability_missing:deep_park`, `multi_gpu_unsupported` for more than one
device on one host). No new exit number.

### 17. Testing

CPU and Fake-engine tests are not qualification. They pin configuration,
accounting and lifecycle; only the live rows show that a multi-node engine works.

#### 17.1 CPU: fake multi-host harness

- **Fake group** (`capyctl-testkit`): one shared fake engine group across N fake
  hosts. The head is ready only after every rank launched; a rank exit leaves the
  others alive but not serving (as NCCL hangs); faults on demand: rank exit, host
  disconnect, reconnect with kept or empty journal, sleep that leaves one rank
  resident, wrong wake output; observations: per-rank residency, liveness, collective
  call count.
- **Group world** (controller tests): the coordinator and store with N scripted
  hosts over the real command types, wired to the fake group, for activation, stop,
  failure, eviction and park/wake. N = 2 and N = 4 both run.
- **Agent tests**: Prepare, member launch, worker identities and member terminate
  through the Fake launcher, per engine, with injected host facts (local addresses,
  sysctl, memlock, `/dev/infiniband`) from a temporary root.
- **Adapter tests**: head and worker argv and env per engine; no transport variable;
  reserved flags stay reserved; single-rank rendering unchanged.
- **Runtime tests** (Python unittest): group mode in each protected entry refuses
  drift; single-rank mode unchanged.

Acceptance IDs: T03, T14 (configuration), T15, T16, T20, T27, T30, T31, T32
(lifecycle and accounting), T21, T37 (security), T22 (SGLang conformance), T34
(capability gating).

#### 17.2 Live on two GB10 hosts

Host A is the head, host B the worker, over the direct link. Prerequisites, owner as
root: compaction sysctl 0, unlimited memlock for the service user, read-write
`/dev/infiniband/uverbs*`, each host's peer address declared. Each engine registered
on both hosts under one name with equal build fingerprints.

| Row | Engine | Scenario | Pass |
|---|---|---|---|
| MN1 | vLLM 0.30.0 | bring-up model at TP 2 | READY through the router; 20/20 completions; RDMA counters rise on both hosts (else recorded as socket transport) |
| MN2 | SGLang 0.5.21 | same model at TP 2 | same as MN1 |
| MN3 | TensorFold 0.6.5 | bring-up two-rank checkpoint | same as MN1 |
| MN4 | vLLM, SGLang | deep park and wake × 5 | per-rank residency drops and returns; canary identical; one collective per cycle |
| MN5 | each engine | kill the worker's engine | group stops; each host releases on its own evidence; port released |
| MN6 | each engine | kill the head's engine | same as MN5 |
| MN7 | one engine | stop host B's agent service while READY | host B's share stays charged and uncertain; settles after the agent returns |
| MN8 | one engine | the group and a single-rank deployment on host A alternate | eviction covers both hosts whole; correct release evidence |
| MN9 | each engine that loads it | Qwen3.8-Flash-Next at TP 2, concurrency sweep 1–64 through the router | averages within about 10% of that engine's reference; peaks reported |

Bring-up models (smallest that each engine loads at two ranks): Qwen3-30B-A3B for
vLLM and SGLang (already mirrored on both hosts); Nemotron 3.5 Lightning 30B-A3B
MLX 4-bit for TensorFold (one or two ranks in the 0.6.5 family table). MN9
references (decided 2026-10-06 for vLLM): for vLLM, a same-build baseline, stock
vLLM 0.30.0 launched directly on the same two hosts with the environment CapyCTL
renders; the published two-Spark vLLM recipe (about 95 tok/s at 1 stream, 735 at 64)
is shown for reference only. For SGLang, the cookbook's two-Spark TP 2 cells; for
TensorFold, its two-rank Flash Next numbers from its recipe page.

**Catalog models on two or more machines, by engine** (from the model pins and the
installed builds' source; "class present" means the engine has the architecture,
not that it was run on GB10):

| # | Model (pinned checkpoint) | Hosts, shape | SGLang 0.5.21 | vLLM 0.30.0 | TensorFold 0.6.5 |
|---|---|---|---|---|---|
| 8 | Qwen3.8-Flash-Next NVFP4 | 2, TP 2 | pinned recipe (cookbook-verified on Spark; dev image) | published two-Spark recipe used a patched 0.30; stock unverified | two ranks with the MLX 4-bit checkpoint; the pinned NVFP4 loads one rank only |
| 9 | GLM-5.3-Flash NVFP4 | 2, TP 2 | pinned recipe (community GB10 image) | GLM classes present; verify | two ranks with EXL3 (experimental) or MLX 4-bit; NVFP4 not read |
| 10 | DeepSeek V4 Flash 0731 | 2, TP 2 | pinned recipe (preview image only) | class present; verify | no (no DeepSeek-V4 CUDA engine) |
| 11 | DeepSeek V4 Flash Vision | 2, TP 2 | pinned recipe (preview image only) | class present; verify | no |
| 12 | Hy3 NVFP4 | 2, TP 2 | class present; command unvalidated | no Hy3 class found | no |
| 13 | MiMo-V2.5 NVFP4 | 2, TP 2 | needs a custom image; unvalidated | class present; verify | no |
| 14 | MiniMax M3 NVFP4 | 3 (PP 3) or 4 (TP 4) | dev image; unvalidated | class present; verify | no (two ranks max) |
| 15 | Hy4 Preview FP8 | 8, TP 8 | dedicated image | no Hy4 class found | no |
| 16 | Nemotron 3 Ultra NVFP4 | 4, TP 4 | validated only on datacenter GPUs | class present; verify | no (Nemotron 3.5 Lightning is the two-rank family, not Ultra) |

Models whose only working build is a container image (9, 10, 11, 13 and parts of 14
and 15) wait for container launchers: this milestone runs registered virtual
environments only. Rows for 3+ hosts (14, 15, 16) need rented machines.

### 18. Documentation

- `docs/guide/several-machines.md`: a section "One model across machines": host
  prerequisites (as root, checked not changed), `peer_address`, a two-host
  deployment, the status view, the risk in plain words.
- `docs/operations/configuration.md`: the three group settings and the engine
  environment settings, each with YAML key, flag and environment variable.
- `docs/operations/network-access.md`: the group peer-exposure risk.
- `docs/guide/engines.md`: `--env` and `--approve-env`; per-engine group support
  table.
- `docs/examples/deployment-multinode.yaml` becomes a real two-host TP 2 group
  example; the current spread example moves to `deployment-spread.yaml`.
- Release notes of the release that ships it, listing exactly which catalog models
  run as groups on which engines and naming the next milestones: container launchers
  and a live row of three or more hosts on rented machines (decided 2026-10-06). The
  status runbook records each live row.

### 19. ADR and amendments

- **ADR 0028, multi-node engine groups**: the decisions above and §1–§14 condensed.
- **ADR 0012 amendment**: the §13 risk statement; every existing protection
  unchanged.
- **ADR 0013**: decision 1's multi-host refusal points at ADR 0028.
- **ADR 0023**: TensorFold two-rank groups are in scope under ADR 0028.
- **SPEC**: §11 (head first, member settlement, canary), §15 (new fields and flags),
  §16.4 (replace `placement.head` with "first host is the head"; delete "Multi-host
  group placement is not yet specified"), §20 (live rows MN1–MN9).
- **AGENTS.md**: one hard-constraint line on unauthenticated group peer transport.

## Open questions for the owner

1. **NCCL interface and HCA choice.** Decision 4 sets nothing, but every reference
   recipe sets `NCCL_IB_HCA` (TensorFold measured about 8% slower prefill on one HCA
   than on both) and `NCCL_SOCKET_IFNAME`/`GLOO_SOCKET_IFNAME`, and the pins report
   about 40% loss when NCCL falls back to TCP. *Recommendation:* build with decision 4
   as is; MN1–MN3 record which transport and HCAs carried traffic. Only if MN9 misses
   the bar because of transport, add one host-level setting (`groups.transport_env`,
   three ways, host administrator only, equal on every member) for these names. Decide
   after the evidence, not now.
2. **SGLang group deep park.** Decision 2 says deep park from day one, but SGLang's
   cross-node release and resume is inferred from source, not run.
   *Recommendation:* build it. vLLM deep; TensorFold is restart-only in any case.
   Decided 2026-10-06 (ADR 0028): SGLang groups deep-park with no restart-only
   fallback; MN4's evidence revisits only the canary rule.
3. **Bring-up and benchmark models.** *Recommendation:* Qwen3-30B-A3B for vLLM and
   SGLang bring-up (already on both hosts); Nemotron 3.5 Lightning 30B-A3B MLX 4-bit
   for TensorFold; Qwen3.8-Flash-Next for MN9 on all three, which needs two
   downloads per host (RadixArk NVFP4, about 135 GB, for SGLang and vLLM; the MLX 4-bit
   export for TensorFold two-rank). Approve the downloads and a TensorFold 0.6.5
   virtual environment on both hosts if one is not already there.
4. **Container-only catalog models.** Models 9, 10, 11 and 13 currently work only from
   a container image. *Recommendation:* ship groups with virtual environments; these
   models follow the container launcher work. GLM-5.3-Flash meanwhile runs on
   TensorFold two-rank with its EXL3 or MLX checkpoint.
5. **Three or more hosts.** The design and CPU tests cover N hosts. *Recommendation:*
   keep `instances: 1` and one device per host; run one 3- or 4-host live row on rented
   machines after MN1–MN9 pass, with the cost confirmed first.

### From 2026-10-04 review

Findings from the document review of 2026-10-04. The adopted ones are recorded in
ADR 0028; the ones the owner decided on 2026-10-06 are also applied above.

- **Group plan cannot precede host materialization** — §4 Group plan / §6 Weights (P1, whole-document (independent pass), coherence, confidence 100)

  The controller cannot construct the immutable group plan because its required
  per-host paths and checkpoint fingerprint are learned only by contacting and
  materializing on each host, while §4 requires the plan to be persisted before any
  host contact. Implementers must otherwise violate either the ordering guarantee or
  the plan's completeness and immutability.

  Adopted 2026-10-04 (ADR 0028 §4).

- **Binding deep-park decision excludes TensorFold** — Owner decision 2 / §12 Park and wake (P1, whole-document (independent pass), confidence 100)

  The release cannot simultaneously satisfy the binding day-one group-wide
  sleep/wake requirement and ship TensorFold as restart-only. This leaves acceptance
  and capability behavior dependent on which section an implementer treats as
  authoritative; explicitly limiting the binding decision to engines with collective
  sleep support makes TensorFold's documented restart behavior implementable.

  Decided 2026-10-06 (ADR 0028): decision 2 now names vLLM and SGLang groups as
  deep-parking from day one, with no restart-only fallback for SGLang, and TensorFold
  groups are restart-only from the engine's group-support data. Park and wake follow
  the deployment's effective residency (§12).

- **World-size-one topology has conflicting placement semantics** — §2 Deployment configuration (P1, whole-document (independent pass), confidence 100)

  A deployment with an explicit TP 1 × PP 1 topology cannot reliably behave like the
  promised byte-identical single-host case: the same rules refuse its normal
  placement fields and reject it in standalone mode. Users and validators will
  therefore disagree over whether that configuration is valid and how it selects its
  host.

  Adopted 2026-10-04 (ADR 0028 §2).

- **Arbitrary-N and PP support outruns qualification** — 17. Testing (P1, adversarial (independent pass), whole-document (independent pass), confidence 100)

  Users can configure advertised multi-host PP and arbitrary-N shapes that have
  never crossed the live qualification boundary, so a release may accept
  configurations whose coordination, shutdown, and recovery behavior was
  demonstrated only in simulations. Two-host TP cannot falsify assumptions specific
  to PP staging or three-plus-host failure fan-out. Gating supported shapes to
  live-qualified rows keeps the exposed capability aligned with available evidence.

  Decided 2026-10-06 (ADR 0028 §2): shapes beyond two hosts and PP > 1 stay allowed
  with no gate and no warning; support stays data-driven per engine.

- **Engine environment lacks required environment-variable inputs** — §2.1 Engine environment / §18 Documentation (P1, whole-document (independent pass), confidence 100)

  The CLI and configuration implementation cannot honor the project's declared
  three-way setting contract because no environment-variable representation or
  encoding is defined for profile variables, deployment variables, or approvals.
  Precedence and conflict behavior are consequently unspecified for one required
  input channel, and the promised operations documentation cannot be written from
  the contract.

  Adopted 2026-10-04 (ADR 0028 §2.1).

- **Environment values exposed through status** — 2.1 Engine environment (P1, security, security (independent pass) (+1 anchor), confidence 100)

  A deployment author who places a credential or token in an approved environment
  variable will have that value persisted and returned through status and
  effective-configuration surfaces. Calling the feature non-secret storage warns
  users but does not prevent accidental disclosure. Treating values as
  sensitive-by-default and redacting them from output closes the direct exposure
  path.

  Adopted 2026-10-04 (ADR 0028 §2.1).

- **Exact-token canary assumes bitwise determinism across TP collectives** — §12 Park and wake (P1, adversarial, confidence 75)

  A healthy group can fail its canary and get stopped and relaunched, which means
  reloading about 135 GB of weights per host, with no fault present. Greedy decoding
  over NCCL all-reduce is not guaranteed to give the same bits from run to run: the
  reduction order and algorithm can change between the first-readiness run and a
  post-wake run, and a near-tie logit then flips a token. The design treats any
  token difference as corruption, and no test checks how often a healthy group
  produces one. MN4 runs only 5 cycles, which is too few to measure that
  false-positive rate.

  Decided 2026-10-06 (ADR 0028 §12): the exact-token canary stays (`temperature: 0`,
  8 tokens, reference at first readiness), compared by one function so the rule can
  change in one place; it is revisited at MN4 with live evidence.

- **Stated problem (catalog models 8-16) is mostly unsolved by this milestone** — Problem; §17.2 catalog table (P1, adversarial, adversarial (independent pass), scope-guardian, confidence 75)

  The motivation is that half the pinned catalog needs two or more machines. By the
  design's own table, though, 9, 10, 11 and 13 wait for containers, 14, 15 and 16
  need 3+ hosts that are out of first-version live scope, 12 is unvalidated, and 8
  on vLLM depends on a patched build. All success criteria could pass while zero
  catalog models run through CapyCTL groups on the shipped engines. Users and the
  downstream integration would then see the 'multi-machine serving' gap as closed
  when it is not.

  Decided 2026-10-06 (ADR 0028): the release docs and release notes list exactly
  which catalog models run as groups on which engines, and name container launchers
  and a live row of three or more hosts on rented machines as the next milestones.

- **Unset GLOO interface binds loopback on default Ubuntu hosts** — §10 Engine adapters (common rules); Open question 1 (P1, feasibility, confidence 75)

  vLLM and SGLang ranks may never form a cross-host group, so MN1/MN2 fail at
  bring-up instead of just running slower. Both engines build gloo CPU groups. With
  GLOO_SOCKET_IFNAME unset, torch's ProcessGroupGloo binds whatever address the
  hostname resolves to and only falls back to loopback otherwise (the libtorch_cpu
  string: 'Using the loopback address as fallback ... set ... GLOO_SOCKET_IFNAME').
  Ubuntu-based hosts map the hostname to 127.0.1.1 by default, so gloo binds
  loopback and the remote rank cannot connect. On a host whose hostname resolves to
  a LAN or tailnet address, gloo uses that network instead of the direct link. Open
  question 1 treats GLOO_SOCKET_IFNAME as a performance knob to revisit only if MN9
  misses its bar, but this is a bring-up blocker. Picking which interface gloo binds
  is an address choice, not a transport choice. That fits the doc's own rule that
  'addresses are not transport', since CapyCTL already confirms the peer address is
  on a local interface.

  Adopted 2026-10-04 (ADR 0028 §10).

- **SGLang wake path: make `group_model_path_mismatch` firm or drop it** — §6 Weights (item 4), §12 Park and wake, §16 Error codes (P1, feasibility, scope-guardian (contradiction), confidence 75)

  SGLang group wakes will fail on the worker whenever the hosts store the model at
  different paths, so MN4 hits an error the design treats as an open live question.
  The source already answers that question. CapyCTL's SGLang wake posts
  `/update_weights_from_disk` with `model_path: self.checkpoint`, which is the
  head's path (crates/capyctl-adapters/src/sglang/http.rs:256). Rank 0 broadcasts
  that request, and each TP rank's weight updater loads `recv_req.model_path` from
  its own filesystem (sglang 0.5.21 scheduler_components/weight_updater.py). vLLM
  does not have this problem: its `reload_weights` collective sends no path. So this
  is known for SGLang today and does not need live bring-up to find out. Opposing
  view (scope-guardian): An error code, refusal path and tests are specified for a
  condition the document says may never occur. Building it ahead of a live finding
  is speculative; if it is needed, live bring-up will show it. Trade-off:
  feasibility would make the refusal unconditional for SGLang groups with deep
  residency; scope-guardian would remove the code until a live row needs it.

  Decided 2026-10-06 (ADR 0028 §6): refuse. A SGLang group with deep park whose
  members' model paths differ is refused `group_model_path_mismatch` (exit 2) before
  the reservation, naming both paths; restart-only SGLang groups are not refused.

- **No detection for a hung rank or failed interconnect after READY** — §9 Readiness and the router; §11 Stop, failure and settlement (P1, adversarial, confidence 75)

  Most real group failures leave every process alive: the direct link drops, a GPU
  throws an Xid, or one rank stalls in NCCL. The design counts only process exit,
  launch failure or failed readiness as a failure, and readiness is checked once at
  bring-up. A stalled group keeps its route open, and requests hang at the head
  until client timeouts, maybe indefinitely. The design's own fake harness models
  this case ('a rank exit leaves the others alive but not serving (as NCCL hangs)'),
  but nothing in the lifecycle acts on it.

  Decided 2026-10-06 (ADR 0028 §11): a group request with no first token within the
  stall timeout triggers one bounded probe through the head; if it fails too, the
  group fails with `group_stalled` and is stopped on every host. No periodic idle
  polling.

- **Per-member park evidence skips SGLang's saver-map contract** — §12 Park and wake (P1, feasibility, confidence 75)

  §12 settles a park from each host's `process_residency` sample. The existing
  SGLang park only counts a release when the enrolled scheduler's saver map shows
  every allocation unmapped. It refuses a park before any engine call when the
  launch enrolled no saver observation
  (crates/capyctl-agent/src/native_execution/residency.rs module doc;
  runtime/sglang_entry.py `_observation_target`). A worker's schedulers run on host
  B, and nothing in the design makes host B pass its observation directory to the
  worker launch or report saver facts. The implementer is left with two bad options:
  weaken SGLang park evidence to process sampling for groups, which breaks the
  existing rule, or find the worker has no observation and the park is refused. The
  plan's Task 19 inherits the same gap.

  Adopted 2026-10-04 (ADR 0028 §12).

- **Owned env list misses names that switch security controls** — §2.1 Engine environment, rule 1 (P1, security, confidence 75)

  A deployer can switch off launch controls through an approved environment
  variable, because the list of names nobody may set misses several variables
  CapyCTL already pins. `VLLM_PLUGINS` is pinned empty as the plugin closure,
  `VLLM_SERVER_DEV_MODE` gates the dev controls, `VLLM_API_KEY` is the engine key,
  `TORCH_EXTENSIONS_DIR` is a private build cache that ADR 0023 says loads code on
  every start, and `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` is the ADR 0025 cap.
  Interpreter variables (`PYTHONHOME`, `PYTHONUSERBASE`, `PYTHONSTARTUP`,
  `PYTHONINSPECT`) can redirect what code gets imported. A host admin who approves a
  broad glob such as `VLLM_*` (the plan's own test approves `V*`, `T*`, `S*`) lets
  anyone with deploy rights load plugins, turn on dev mode or get around the memory
  cap on that host. Rule 4 also never says whether CapyCTL's own pinned value or the
  user's value wins, so an implementer may let the user value win. Making every name
  in the adapters' fixed allowlists owned, plus the `PYTHON*` prefix apart from
  `PYTHONUNBUFFERED`, closes this. The rule's own logic already covers it ('names
  CapyCTL already renders or closes'); the enumeration just stops short.

  Adopted 2026-10-04 (ADR 0028 §2.1).

- **Risk statement understates exposure: pickle over open ports** — §13 Ports and peer exposure (security), Risk statement (P1, security, confidence 75)

  The owner accepts the trust-the-network risk on a description that is too mild.
  The risk statement says an attacker 'may be able to inject tensors', but vLLM's
  broadcast queue and the Gloo object broadcasts used by vLLM and SGLang carry
  pickled Python objects. torch.distributed's object collectives use pickle too, and
  vLLM's security guidance calls inter-node traffic insecure for this reason. Anyone
  on any interface, a tailnet included, can therefore likely run code as the
  engine's user, and from there read the per-launch engine and admin keys in that
  process's environment. The listener list also leaves out Gloo's CPU-group ports.
  This does not reopen the settled decision. It corrects what the ADR 0012
  amendment, the status line and the docs tell the operator, so the 'keep group
  hosts on a private link' advice carries its real weight.

  Adopted 2026-10-04 (ADR 0028 §13).

- **Group readiness lacks collective inference proof** — 9. Readiness and the router (P1, adversarial (independent pass), confidence 75)

  The router can expose a group whose head reports model readiness while a worker
  collective is stalled, causing the first real request to hang or fail. The claim
  that head readiness implies a working collective is warranted only if that check
  completes an actual distributed forward pass. Requiring a bounded inference probe
  before READY directly tests the property on which routing depends.

  Decided 2026-10-06 (ADR 0028 §9): before READY the controller runs one bounded
  1-token completion through the head; a failure is a launch failure and stops the
  group.

- **Port held by another process is drawn again on every retry** — §5 Rendezvous port; §7 Prepare (P1, feasibility, adversarial, adversarial (independent pass) (+1 anchor), confidence 100)

  The allocator takes the lowest port in the head's range that no unsettled group
  plan holds. If a process outside CapyCTL holds that port, Prepare refuses
  `rendezvous_port_in_use`, every member is released, and the next deploy or
  recovery relaunch draws the same port again. One stray listener therefore blocks
  group work on that head even though the rest of the range is free. Prepare also
  probes the port only on the peer address, while §13 says the torch store binds
  every interface. A listener on another address of that port passes the probe, and
  the engine then fails to bind at launch, which becomes a group member failure
  instead of a typed prepare refusal.

  Adopted 2026-10-04 (ADR 0028 §5).

- **MN9 vLLM bar compares stock 0.30.0 to a patched-build recipe** — §17.2 Live on two GB10 hosts (MN9) (P2, adversarial, feasibility (+1 anchor), confidence 100)

  MN9's 10% bar for vLLM uses the published two-Spark recipe's numbers, but the
  design notes elsewhere that this recipe ran on a patched 0.30 and that stock
  0.30.0 is unverified. On top of that, the recipe sets NCCL variables that decision
  4 forbids. A miss could therefore come from the engine patch rather than CapyCTL,
  and the row cannot tell which, so the pass/fail result would not support the
  release decision.

  Decided 2026-10-06 (ADR 0028): the vLLM bar is a same-build baseline, stock vLLM
  0.30.0 launched directly on the same two hosts with the environment CapyCTL
  renders, and CapyCTL must be within about 10% of it; the published recipe is shown
  for reference only.

- **require_rdma summary says host checks refuse; compaction never does** — Owner decisions (carried forward) vs §7 table (P2, coherence, confidence 75)

  The carried-forward summary says host checks warn by default and refuse under
  require_rdma: true, but the §7 table makes compaction warn in both modes. An
  implementer or test author following the summary would make compaction refuse
  under require_rdma. The detailed table (and the implementation plan: 'compaction
  never refuses') is authoritative.

  Adopted 2026-10-04 (ADR 0028 decision 10, §7).

- **validate config multi-host resolution is an adjacent feature** — 15. Status and CLI (P2, scope-guardian, confidence 75)

  Resolving a group deployment against every named host document via `--host` adds
  new CLI behavior that no owner decision or goal asks for, against the 'minimal
  fix, no adjacent features' rule. It adds multi-document loading, per-host profile
  resolution and its own error surface to build and test.

  Decided 2026-10-06 (ADR 0028): build it as §15 describes; `--host` is repeated once
  per named host.

## Known risks

- **Unauthenticated peer listeners** on every interface during a run (§13).
- **Transport.** With no NCCL settings, NCCL may pick TCP or one HCA; MN9 can miss the
  bar for that reason; MN1–MN3 make it visible.
- **Sleep at TP > 1 across nodes** is undocumented upstream for vLLM headless mode and
  untested for SGLang; there are upstream reports of wrong output after sleep and wake
  at TP 2. The canary and per-rank evidence catch it; they do not fix it.
- **SGLang rank > 0 ignores SIGTERM**; every SGLang worker stop escalates to SIGKILL.
- **Disk and time.** Flash-Next is about 135 GB per host per checkpoint format; the
  group occupies most of both hosts, so single-rank deployments cannot co-reside while
  it serves.
- **Kernel fit.** Most catalog recipes were validated on sm_100 datacenter GPUs; GB10
  is sm_121. A model can parse and still fail to load or run slowly.

## Out of scope

- The scheduler choosing hosts for a group; `instances > 1` of a group; more than one
  device per host.
- Rank replacement or partial group restart.
- Ray, data parallel and expert parallel across hosts (SGLang `--ep` inside TP is an
  engine flag, not a CapyCTL topology).
- Container launchers; installing engines.
- Firewall checks, CapyCTL-set NCCL tuning, sysctl or limit changes.
- Peer-to-peer weight copy between hosts.
- Shared KV caches across group members.
