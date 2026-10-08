# ADR 0028 — Multi-node engine groups

**Status:** Accepted (owner decisions 2026-09-25, 2026-10-05 and 2026-10-06).
**Amends:** SPEC §11, §15, §16.4, §20; ADR 0012; ADR 0013 decision 1; ADR 0023.
**Related:** ADR 0007, 0011, 0016, 0017; `docs/specs/2026-10-05-multi-node-groups-design.md`.

## Context

A model that does not fit one machine must run as one engine spread over several
machines: tensor parallel (TP) splits each layer across ranks, pipeline parallel
(PP) splits the layers into stages. CapyCTL runs every engine on one host today.
The domain has a two-host `GroupPlan` and wire messages, but nothing builds or sends
them, and the agent's native path refuses both. Configuration has no `topology`;
ADR 0013 decision 1 refuses a multi-host group; `multi_gpu_unsupported` refuses more
than one device on a discrete host. Every multi-node engine flag is reserved, the
single-rank rendezvous is pinned to loopback, and group reservation, fan-out launch,
per-rank settlement and recovery were never built. Half of the pinned model catalog
needs two or more machines.

## Decision

Owner decisions:

1. **Engines.** Engine virtual environments are registered with `capyctl engine add
   --name`, the same name on every host. Bring-up first uses a model the stock engine
   loads.
2. **Residency (amended 2026-10-06).** vLLM and SGLang groups deep-park from day one:
   group-wide sleep and wake with evidence from every rank. TensorFold groups are
   restart-only (no sleep or wake), as the engine's group-support data says.
3. **Peer exposure.** Trust the network. CapyCTL does no firewall check. Rendezvous,
   broadcast, gloo and NCCL ports are open on all interfaces during a run. CapyCTL binds to the
   direct link where the engine allows it. Recorded as a known risk.
4. **NCCL.** CapyCTL sets no NCCL setting; NCCL chooses the transport.
5. **Readiness.** Head health only. Other hosts record their rank's process
   identities for ownership and stop evidence.
6. **Failure.** Any rank failure stops the whole group. Each host releases only on its
   own evidence.
7. **Ownership.** One owner per rank. N-rank groups, TP x PP across N hosts.
8. **Placement.** Named hosts only, in rank order, head first. The scheduler never
   chooses hosts for a group.
9. **Rendezvous port.** Allocated by the server from a host-declared range (default
   25000-25099), recorded in the group plan, released only after every host proves
   stop.
10. **Host tuning.** The compaction sysctl, memlock and `/dev/infiniband` access are
    set by the owner as root. CapyCTL checks them and never changes them. By default
    each check warns; under `require_rdma`, memlock and infiniband refuse and
    compaction only warns.
11. **Weights.** Every named host downloads the model source in parallel. Digests are
    compared across hosts before launch. Free disk is checked first.
12. **Engines (changed 2026-10-05).** SGLang, vLLM and TensorFold together, in one
    group mechanism, on the latest installed builds: vLLM 0.30.0, SGLang 0.5.21,
    TensorFold 0.6.5. Ideally every catalog model runs on every engine that can serve
    it.
13. **Success bar.** Through the router, within about 10% of the reference recipe's
    averages. Peaks are reported, not required. For vLLM the reference is a
    same-build baseline (decided 2026-10-06, below).

Engine environment (2026-09-25): settings an engine reads only from the environment
are declared in YAML (the profile's `env`, the deployment's `engine_config.env`) or by
a flag or environment variable (`capyctl engine add --env K=V`, `capyctl deploy
--engine-env K=V`; §2.1). Names are checked against the profile's approved list.
CapyCTL-owned names (`NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP`, and the rest of
§2.1 rule 1) are never settable.

Choices carried forward from the 2026-09-25 design: addresses are not transport
(CapyCTL renders the head's peer address and each host's own peer address, plus the
interface that holds it for gloo, and nothing named `NCCL_*`); host checks warn by
default; under `require_rdma: true` memlock and infiniband refuse and compaction only
warns; group eviction is all-or-nothing across the named hosts;
`instances: 1` and one device per host in the first version; a post-wake canary for
groups. Standalone is a server plus one host, so a group needs the server and host
roles. Existing engine installations only: no engine install, no containers yet.

Decided on 2026-10-05, answering the design's open questions:

- NCCL decision 4 is built as is. Live rows record which transport and HCAs carried
  traffic. A host-level transport setting is added only if MN9 misses the bar because
  of transport, after the evidence.
- SGLang group deep park is built. (The restart-only fallback for SGLang if MN4 failed
  was withdrawn on 2026-10-06, below.)
- Bring-up models: Qwen3-30B-A3B for vLLM and SGLang; Nemotron 3.5 Lightning 30B-A3B
  MLX 4-bit for TensorFold; Qwen3.8-Flash-Next for MN9 on all three.
- Container-only catalog models wait for container launchers; groups ship with
  registered virtual environments.
- Groups of three or more hosts run live on rented machines after MN1-MN9 pass, with
  the cost confirmed first.

Review rulings 2026-10-04 (the coordinator's answers to the spec review, recorded in
the sections named; the owner decided the rest of the findings on 2026-10-06, below):

- Group plan timing: written after every host reports its path and digest, durable
  before any `Prepare` or `Launch` (§4, §5, §6).
- Explicit TP 1 x PP 1 is the single-host path, with no group rules (§2).
- Engine environment has an environment-variable channel for each setting; a
  non-empty `--approve-env` list replaces `CAPYCTL_APPROVE_ENV` (§2.1).
- Environment values are redacted in status and effective configuration (§2.1 rule 7).
- `GLOO_SOCKET_IFNAME` is rendered from the peer address; still nothing named
  `NCCL_*` (§10).
- SGLang park evidence per member follows the saver-map contract (§12).
- Owned environment names are extended, and CapyCTL values always win (§2.1 rule 1).
- Peer exposure names pickled objects and gloo's ports; decision 3 stands (§13, ADR
  0012 amendment).
- The rendezvous allocator skips externally held ports, and `Prepare` probes with a
  wildcard bind (§5, §7).
- Under `require_rdma`, memlock and infiniband refuse; compaction only warns
  (decision 10, §7).

Decided on 2026-10-06, answering the review findings held for the owner:

- **Deep park** (decision 2 amended). vLLM and SGLang groups deep-park from day one;
  SGLang has no restart-only fallback, and MN4's evidence revisits only the canary
  rule. TensorFold supports only restart-only, from the engine's group-support data.
  Park and wake follow the deployment's effective residency (§2, §12).
- **Shapes.** Groups of more than two hosts and PP > 1 are allowed with no gate and no
  warning. Support stays data-driven per engine; live qualification covers two hosts
  (§2).
- **Wake canary.** Exact tokens: `temperature: 0`, 8 tokens, compared by one function
  with the reference recorded at first readiness. A mismatch is `group_wake_mismatch`
  and stops the group. Revisited at MN4 with live evidence (§12).
- **SGLang model paths.** A SGLang group with deep park whose members' model paths
  differ is refused `group_model_path_mismatch` (exit 2) before the reservation,
  naming the head's path and the member's path, because SGLang's wake posts the head's
  path to every rank. Restart-only SGLang groups are not refused (§6, §12).
- **Request stall.** When a group request gets no first token within the stall
  timeout, the controller runs one bounded probe through the head. If the probe fails
  too, the group fails with `group_stalled` and is stopped on every host. No periodic
  idle polling (§11).
- **Readiness probe.** Before READY the controller runs one bounded 1-token
  completion through the head, which exercises every rank. A failure is a launch
  failure and stops the group. One probe function serves readiness, the stall check
  and the wake canary (§9).
- **MN9 vLLM bar.** The reference is a same-build baseline: stock vLLM 0.30.0
  launched directly on the same two hosts with the environment CapyCTL renders.
  CapyCTL must be within about 10% of it. The published two-host recipe is shown for
  reference only.
- **`capyctl validate config --host`** is built: it resolves a group deployment
  against every named host's document, with `--host <host document>` repeated once per
  host (design §15).
- **Catalog at release.** The release docs and release notes list exactly which
  catalog models run as groups on which engines. Container launchers and a live row of
  three or more hosts on rented machines are the next milestones.

Closed error codes and exit numbers are in the design's section 16 table
(`docs/specs/2026-10-05-multi-node-groups-design.md`).

The sections below mirror the design's sections 1 to 14; code cites them as
`ADR 0028 §N`.

### 1. Vocabulary

A **group** is one engine instance whose ranks run on more than one host. It is still
one ADR 0013 instance: one generation, one route endpoint, one lifecycle claim. A
**member** is the part of a group on one host. Member 0 is the **head** and serves
HTTP; every other member is a **worker** and serves no inference. The **node rank** is
the member's position in `placement.hosts` (head = 0). **Local ranks** are the devices
one member uses (first version: 1). The **group plan** is the durable, server-written
record of one group incarnation: engine, topology, every member (host, node rank,
role, devices, model path, peer address), the head's service port, each worker's
loopback port where its engine opens one, the rendezvous port and the generation. A
plan is never edited; a relaunch is a new generation with a new plan.

### 2. Deployment configuration

A deployment gains an optional `topology` (`tensor_parallel`, `pipeline_parallel`) and
exact `placement.hosts`. Checked at deploy time:

- No `topology`, or `tensor_parallel x pipeline_parallel = 1`, is a single-host
  instance with byte-identical command identity, effective configuration and digests.
  An explicit TP 1 x PP 1 topology is the single-host path: no group rules apply, and
  placement fields (`host:`, `placement.selector`, `strategy`, `max_per_host`) and a
  standalone role are allowed.
- With world size `W > 1`, `placement.hosts` is required, has at least two distinct
  entries, and its length `N` divides `W`; local ranks are `W / N`, which must be 1 in
  the first version (`group_shape_unsupported`).
- With world size `W > 1`, `host:`, `placement.selector`, `strategy` and `max_per_host`
  are refused (`group_placement_required`); `instances` must be 1
  (`group_instances_unsupported`). A standalone role refuses a topology with
  `W > 1` using `group_placement_required` (one host).
- Duplicate hosts (after trimming), a zero dimension, or a host count that does not
  divide TP x PP is `group_topology_invalid`.
- `resources` and `engine_config.memory` are per member (§5). `devices` states one
  device per host.
- Multi-node engine flags stay reserved; CapyCTL renders them from `topology` (§10).
- The profile name must resolve on every named host with the same build fingerprint
  (`group_profile_mismatch`). The effective revision records the per-host resolutions.
- Shapes beyond two hosts and PP > 1 are allowed, with no gate and no warning,
  wherever the engine's support data admits them (decided 2026-10-06). Live
  qualification covers two hosts.
- The engine must support the shape (`group_shape_unsupported:<engine>`):

| Shape and feature | vLLM 0.30.0 | SGLang 0.5.21 | TensorFold 0.6.5 |
|---|---|---|---|
| TP across hosts | yes | yes | `tp` 2 only |
| PP across hosts | yes | yes | no |
| Hosts | any N dividing TP x PP | any N dividing TP x PP | exactly 2 |
| Deep park | yes (sleep mode on every rank) | yes (memory saver on every rank) | no (restart-only) |
| Worker listens on | nothing (headless) | a loopback dummy health port | nothing |

#### 2.1 Engine environment

Implemented for every deployment, single-host or group. Levels: the engine profile
(`env:`), the deployment (`engine_config.env:`) and approval
(`security.approved_env:`). Every setting is available three ways, with precedence
flag > env > YAML > default:

| Setting | YAML | Flag | Environment |
|---|---|---|---|
| Profile env | `env:` | `capyctl engine add --env K=V` | `CAPYCTL_ENGINE_ADD_ENV` |
| Deployment env | `engine_config.env:` | `capyctl deploy --engine-env K=V` | `CAPYCTL_ENGINE_ENV` |
| Approval | `security.approved_env:` | `capyctl engine add --approve-env GLOB` | `CAPYCTL_APPROVE_ENV` |

An environment variable holds `;`-separated entries (`K=V;K2=V2` for env, `;`-separated
globs for approvals). Between the two command-line channels a flag wins per name; a
non-empty `--approve-env` list replaces `CAPYCTL_APPROVE_ENV` as a whole, which is used
only when no flag is given.

1. CapyCTL-owned names are never settable at either level, whatever `approved_env`
   says: `NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP`; the names CapyCTL already
   renders or closes (`PATH`, `LD_*`, `CUDA_HOME`, `CUDA_VISIBLE_DEVICES`,
   `CAPYCTL_*`, `VLLM_PLUGINS`, `VLLM_SERVER_DEV_MODE`, `VLLM_API_KEY`, `VLLM_PORT`,
   `VLLM_ALLOW_INSECURE_SERIALIZATION`, `TORCH_EXTENSIONS_DIR`,
   `TENSORFOLD_CUDA_MEMORY_LIMIT_GB`, every `PYTHON*` name except `PYTHONUNBUFFERED`,
   and every name the adapters pin); and the address and rendezvous names of the
   other engines (`SGLANG_HOST_IP`, `HOST_IP`, `SGLANG_LOCAL_IP_NIC`,
   `SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE`, `TF_COMM_BACKEND`, `TF_NCCL_LIB`).
   Refused `engine_env_reserved:<name>`, and so is an approval entry that matches only
   owned names. CapyCTL-rendered values always win over any configured value.
2. A deployment name must match an `approved_env` entry of the profile it resolves
   against (`engine_env_not_approved:<name>`). The profile's own `env` is
   host-authored and needs no approval, except for rule 1. The existing safe names
   (`MAX_JOBS`, `FLASHINFER_NVCC_THREADS`, `TOKENIZERS_PARALLELISM`,
   `PYTHONUNBUFFERED`, `RUST_LOG`) keep their rules at both levels and are admitted
   at the deployment level without approval.
3. An approval entry is upper-case `A`-`Z`, `0`-`9`, `_`, optionally ending in one
   `*`. A bare `*` is refused. Rule 1 is checked on the concrete name.
4. The deployment's value overrides the profile's; the effective configuration shows
   each variable with its source.
5. A name given both in the YAML document and by a flag or environment variable is
   `engine_env_conflict:<name>`.
6. In a group every named host resolves against its own profile; a name not approved
   on one host refuses the group. Rendered environments are equal across members
   apart from the per-host address variable.
7. Values are bounded (4 KiB each, 64 names per level) and single-line. They are
   stored for launch and the fingerprint, but status and the effective configuration
   show names and sources with the value as `<redacted>`. They are still not secret
   storage.
8. A deployment env change is a new revision; a profile env change changes the recipe
   fingerprint.

### 3. Host groups policy

Every setting three ways (flag > env > YAML > default):

| Setting | YAML (`host.yaml`) | Flag (`capyctl start host`) | Environment | Default |
|---|---|---|---|---|
| Peer address | `resource_policy.groups.peer_address` | `--peer-address <ip>` | `CAPYCTL_PEER_ADDRESS` | none |
| Rendezvous ports | `resource_policy.groups.rendezvous_port_range` (`start`, `end`) | `--rendezvous-ports <start-end>` | `CAPYCTL_RENDEZVOUS_PORTS` | `25000-25099` |
| Refuse on missing RDMA tuning | `resource_policy.groups.require_rdma` | `--require-rdma` | `CAPYCTL_REQUIRE_RDMA` | `false` |

A host without a peer address cannot be named in a group (`peer_address_missing`).
The address must not be loopback, unspecified or multicast; the agent verifies at
start that it is assigned to a local interface and reports it in its inventory, and a
mismatch refuses group work (`peer_address_not_local`). Port ranges need
`1024 <= start <= end`. A host document without the block keeps its policy digest.

### 4. Group plan

`GroupPlan::two_host` becomes `GroupPlan::new`, validating N members: node-rank order
0..N-1, distinct hosts, exactly one head (rank 0); member ids `head`, `worker-1`, ...;
the same engine, profile fingerprint and checkpoint fingerprint on every member;
distinct peer addresses, none loopback, unspecified or multicast; a nonzero head
service port, and a worker loopback port only when its engine opens one (SGLang); a
nonzero rendezvous port and a positive generation; and `tensor_parallel x
pipeline_parallel = N x local_ranks`. `MemberPlan` gains `role`, `model_path` and
`worker_port: Option<u16>`; `service_port` becomes `Option<u16>`. The plan carries the
engine, topology and generation, and is written durably with the reservation (§5),
after every host has materialized the source and reported its path and digest (§6;
digests agree) and before any `Prepare` or `Launch` is sent.

### 5. Reservations, memory per rank, rendezvous port

- **Owners per member**: `deployment:<id>/instance:<k>/member:<r>`, each charged on
  its own host's memory domains and device. The instance row points at the group plan.
- **Memory per rank.** Every member's footprint is the deployment's `resources` (or
  the phases derived from `engine_config.memory`), charged on its host like a
  single-host instance (ADR 0007 phases, ADR 0013 managed limit and free reserve). The
  head's API process is inside that figure, so the operator sizes for the head. On
  unified-memory hosts (GB10) the device and system domains are one pool, as today.
  TensorFold's memory cap (ADR 0025) applies
  per rank. Derived phases are sized for the member's share of the weights, not the
  whole checkpoint (amendment of 2026-10-07 below); declared `resources` are the
  member's as written.
- **All or nothing.** Every member is reserved in one store transaction under ADR 0007
  (fresh observations, epoch compare-and-swap on every named host), so two group plans
  sharing a host cannot deadlock on partial acquisition. This is atomic accounting in
  the server's store, not a cross-host atomic launch.
- **Rendezvous port** is drawn in the same transaction: the lowest port in the head's
  range not held by an unsettled group plan and not recently found held outside
  CapyCTL (`rendezvous_ports_exhausted`). A `Prepare` that finds the port held records
  it as externally held for that head, with an expiry, so the next draw skips it. SGLang
  worker loopback ports come from each worker host's `endpoint_port_range` through the
  existing endpoint lease table.
- **Eviction.** The planner computes a victim set on each named host with the existing
  per-host rules and evicts only when every host has one. A group chosen as a victim
  is parked or stopped whole.
- Each member settles on its own host's evidence only (§11).

### 6. Weights: parallel download and digest agreement

The model source is materialized on every named host at once through the existing path,
after a free-space check on each (`insufficient_space`, naming the host). When all have
finished, each reports its path and checkpoint digest; they must be equal
(`group_checkpoint_mismatch`) and nothing launches on a mismatch. Only then is the
group plan written (§4), so materialization runs before the reservation. The store's digest
record becomes one row per host. Each member's own `model_path` goes into the plan.
Before the reservation, a SGLang group with deep park whose members' paths differ is
refused `group_model_path_mismatch` (exit 2), naming the head's path and the member's
path: SGLang's wake posts the head's path to every rank (§12). Nothing is reserved, so
nothing is released. Restart-only SGLang groups, vLLM and TensorFold keep differing
paths (decided 2026-10-06).

### 7. Prepare and host checks

After the reservation commits, `Prepare(GroupPlan)` goes to every member and has no
process effect. Each host checks: the profile resolves with the recorded fingerprint;
the model path holds the recorded digest; the peer address is local; on the head, the
rendezvous port is free under a wildcard bind (`0.0.0.0` and `::`, since the torch
store binds every interface) and the service port on loopback; on the head of a
SGLang group whose deployment enables DP attention, also the six ports SGLang derives
from the rendezvous port `P` and binds there (`P+1` to `P+6`, or `P-7` to `P-2` when
`P+7` exceeds 65535, as SGLang 0.5.21 computes them; checked when the `Launch` re-runs
these checks, because only the `Launch` carries the deployment document); on a SGLang
worker, its loopback port is free; and host tuning:

| Check | How | Default | `require_rdma: true` |
|---|---|---|---|
| `compaction` | `/proc/sys/vm/compaction_proactiveness` nonzero | warn | warn |
| `memlock` | the agent's `RLIMIT_MEMLOCK` below unlimited | warn | refuse |
| `infiniband` | `/dev/infiniband/uverbs*` absent or not read-write for the service user | warn | refuse |

Warnings are `host_tuning_warning:<item>`, refusals `host_tuning_missing:<item>`.
CapyCTL reads these and never writes them. Only memlock and infiniband refuse;
compaction only warns. A refusal releases every member's
reservation and records the refusal per host. `Prepare` is idempotent and retried
under the same command id.

### 8. Fan-out launch

Once every member has prepared, `Launch(GroupPlan)` goes to every member concurrently.
Workers are not held back until the head is ready: every engine's initialization waits
for all ranks at its store. Each host journals the launch before spawning, records its
member's process identities and reports them; a host that cannot launch reports a
typed failure. A member failing to launch, or exiting before readiness, stops the
group (§11).

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

The controller, store, agent and protocol know only the group plan. Each adapter turns
one member of a plan into its engine's command through one shared input,
`GroupMemberArgs` (`tensor_parallel`, `pipeline_parallel`, `nnodes`, `node_rank`,
`head_address`, `rendezvous_port`, `own_address`, `worker_port`), and declares its
support (`GroupSupport`: shapes, deep park, worker listener) in one function the
configuration layer reads for the §2 checks.

| | vLLM 0.30.0 | SGLang 0.5.21 | TensorFold 0.6.5 |
|---|---|---|---|
| Parallel sizes | `--tensor-parallel-size T --pipeline-parallel-size P` | `--tp-size T --pp-size P` | `--tp 2` |
| Rank and world | `--nnodes N --node-rank r` | `--nnodes N --node-rank r` | `--rank r` |
| Rendezvous | `--master-addr <head> --master-port <port>` | `--dist-init-addr <head>:<port>` | `--master <head> --master-port <port>` |
| Backend | `--distributed-executor-backend mp` | default | NCCL (built in) |
| Worker | `--headless`; no host, port, key or middleware | same command, `--host 127.0.0.1 --port <worker_port>` | no host or port |
| Own address | `VLLM_HOST_IP=<own>` | `SGLANG_HOST_IP=<own>` | none |
| Deep park flag | `--enable-sleep-mode` on every rank | `--enable-memory-saver` on every rank | not applicable |
| Head serves | loopback API, keys and guard, as single-rank | same | same |

- Nothing named `NCCL_*` is rendered or inherited (decision 4). CapyCTL renders
  `GLOO_SOCKET_IFNAME` (CapyCTL-owned) as the interface that holds the member's peer
  address, because otherwise gloo binds the hostname's address, which is `127.0.1.1`
  on Ubuntu, and the remote rank cannot connect. This is an address choice, not a
  transport choice. The agent resolves the interface name from the peer address. The
  single-rank loopback pin (`GLOO_SOCKET_IFNAME=lo`, `NCCL_SOCKET_IFNAME=lo`,
  `VLLM_HOST_IP=127.0.0.1`, SGLang's file store) is not applied to a group launch.
- The protected entries (`runtime/vllm_entry.py`, `runtime/sglang_server_args.py`,
  `runtime/sglang_entry.py`) gain a group mode: CapyCTL passes the rendered multi-node
  values (`CAPYCTL_GROUP_EXPECTED`) and the entry refuses any drift
  (`group_drift:<field>`). The single-rank pins (`tensor_parallel_size: 1`;
  `tp_size: 1, nnodes: 1, node_rank: 0`) take the plan's values. TensorFold has no
  protected entry; its argv is checked in the adapter.
- The recipe's own flags stay deployment `engine_config` or approved extra args and
  must be equal on every member; TensorFold requires equal context, drafting and
  `--parallel` on both ranks.
- **vLLM:** with `nnodes > 1` it chooses `mp`; `--headless` runs a bare executor that
  joins the head's broadcast queue, which binds `VLLM_HOST_IP` (without it vLLM picks
  the default-route interface). Sleep and wake are
  collective calls from the head that reach every rank.
- **SGLang:** rank > 0 starts a dummy health server on `--host:--port`, hence the
  worker loopback port (two groups on one host would otherwise collide on the default
  30000). Rank > 0 ignores SIGTERM, so its stop normally escalates to
  SIGKILL (§11). Release and resume go to rank 0, which broadcasts to every TP rank;
  cross-node behavior is inferred from source and proven only by the live row.
- **TensorFold:** two ranks, one GPU each; rank 1 serves nothing. It has no sleep, so
  a TensorFold group is `restart_only`: park stops both ranks and wake relaunches the
  group.

### 11. Stop, failure and settlement

- **Stop** (operator, idle, switch victim): close the head's ingress and drain as
  today, then send `Terminate` to every member concurrently. Each host terminates its
  journaled process tree with SIGTERM, a bounded wait and SIGKILL, and reports
  gone-evidence. An escalation on a SGLang worker is expected and recorded.
- **Rank failure.** Any member's exit, launch failure or failed readiness (the
  readiness probe included) marks the group failed (`group_member_failed`, with host
  and node rank) and stops every other member. No rank replacement: an NCCL group
  cannot re-admit a rank.
- **Request stall** (decided 2026-10-06). When a group request gets no first token
  within the stall timeout, the controller runs one probe through the head (§9),
  bounded at 60 s. If the probe fails too, the group is failed (`group_stalled`,
  status only, no exit number) and stopped on every host; if it passes, nothing is
  stopped. An idle group is never probed: there is no periodic polling. The timeout is
  a server setting, three ways: `groups.stall_timeout` in the server document,
  `--group-stall-timeout` on `capyctl start server`, `CAPYCTL_GROUP_STALL_TIMEOUT`;
  default 120 s.
- **Settlement.** Each member's reservation is released only on its own host's
  gone-evidence. The instance leaves its lifecycle step only when every member has
  settled.
- **Unreachable host.** Its member stays charged and `uncertain`
  (`group_member_uncertain`). Lease expiry never frees it. It settles when the host
  reconnects and reconciles its journal, or when the host is revoked and recovered
  (ADR 0016). A host that returns with an empty journal settles only on recorded
  identities.
- The rendezvous port is released only after every member settles.
- **Recovery.** Under `recovery: reconcile`, a failed group relaunches as a new
  generation and plan only after every member of the old one has settled. Attempts
  count per instance.

### 12. Park and wake

The head's agent is the lead: it invokes each collective once through the head's
loopback control endpoint. Workers never receive a sleep call.

- **Park (deep), vLLM and SGLang.** Close ingress, drain, then the head calls `/sleep`
  (vLLM, admin key) or `/release_memory_occupation` (SGLang). The park settles only
  when the call succeeded and every member's host reports its resident memory at or
  below the member's parked budget (`process_residency`, per member). For SGLang the
  evidence follows the existing saver-map contract: each worker host passes its
  observation directory to its worker launch and reports saver facts for its member,
  and there is no fallback to process sampling. A member that
  reports nothing within the park deadline keeps its full charge and the group is
  uncertain; a member still resident fails the park. Either stops the group. The
  collective is never repeated.
- **Wake.** The head calls `/wake_up` or `/resume_memory_occupation`; every host
  reports its member resident; the head readiness check passes; and the canary
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
  park and wake above.
- Parked members stay charged at their parked budget on their own hosts; `max_parked`
  counts a group once on each host it occupies.

### 13. Ports and peer exposure

- The head's service port comes from its `endpoint_port_range` and stays on loopback,
  behind the per-launch keys and the key guard, reachable only through host ingress
  and the router (ADR 0012 unchanged).
- The rendezvous port comes from the head's `groups.rendezvous_port_range`.
- Engines also open listeners CapyCTL does not choose: the torch TCP store on the
  rendezvous port binds every interface; vLLM's broadcast queue binds an ephemeral
  port on `VLLM_HOST_IP`; TensorFold Flash Next under `--parallel` opens one extra
  ephemeral port on rank 0; SGLang with DP attention binds six ports derived from the
  rendezvous port on the head (§7); NCCL uses dynamic ports; gloo opens ephemeral CPU-group
  ports on each rank. None is authenticated.
- CapyCTL narrows what it can: it renders the direct-link addresses where the engine
  takes them, keeps every API and control endpoint on loopback, and exposes nothing
  through ingress or the router.

Risk: during a group run, unauthenticated rendezvous, broadcast, gloo and NCCL
listeners are reachable on every interface of every member host, including any
wireless or overlay network. The rendezvous store, vLLM's broadcast queue and gloo's
object collectives carry pickled Python objects, so anyone who can reach those ports
can likely run code as the engine's user and read its per-launch keys. They can also
disturb or crash the group. CapyCTL performs no firewall check; the operator is
responsible for the network (decision 3). Status marks a group instance `peer
transport: unauthenticated`, and the docs say to keep group hosts on a private link.
The ADR 0012 amendment records this.

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
  controller take the member id from the plan.

## Honest scope

CPU and Fake-engine tests are not qualification. They pin configuration, accounting
and lifecycle; only the live rows MN1-MN9 on two hosts show that a multi-node engine
works. Known risks: unauthenticated peer listeners (§13); NCCL may pick TCP or one HCA
with no NCCL settings; sleep at TP > 1 across nodes is undocumented upstream for vLLM
headless mode and untested for SGLang; SGLang rank > 0 ignores SIGTERM; the group
occupies most of both hosts; most catalog recipes were validated on sm_100 while GB10
is sm_121.

Out of scope: the scheduler choosing hosts for a group; `instances > 1` of a group;
more than one device per host; rank replacement or partial restart; Ray, data parallel
and expert parallel across hosts; container launchers and engine install; firewall
checks, CapyCTL-set NCCL tuning, sysctl or limit changes; peer-to-peer weight copy;
shared KV caches across members.

## Consequences

- A model larger than one machine can serve through one route, on any of the three
  engines, with per-host accounting and evidence.
- Every group run exposes unauthenticated peer listeners on all interfaces of its
  hosts, and some carry pickled objects. This is a recorded risk (ADR 0012 amendment), not a protection.
- Group placement is manual: the operator names the hosts and declares each host's
  peer address.
- Engine environment settings become declarable for every deployment, single-host or
  group, behind per-profile approval.
- A group whose residency is `restart_only`, and so every TensorFold group, parks by
  stopping and wakes by relaunching.
- The protocol gains one capability; the wire version does not change.

## Amendment of 2026-10-07: derived memory is per rank (owner decision)

Found in review: a member whose phases derive from `engine_config.memory` was sized
from the whole checkpoint, as on one host (each member's grant is its own host's
resolution, and every launch named the checkpoint's weights). A two-host TP2 group
reserved its weights twice, and a checkpoint larger than one host could never be
admitted. Decision: each member is charged its share of the weights. Declared
`resources` stay the member's as written, and a single-host deployment (world size 1)
is unchanged: its effective revision, recipe fingerprint and digests are byte-identical.

**Formula.** For a checkpoint of `W` bytes of weights at `tensor_parallel` T and
`pipeline_parallel` P:

```text
replicated = W - sharded
stage      = sharded                                          when P = 1
           = min(sharded, ceil(layers / P) x largest_layer)   otherwise
share      = ceil(stage / T) + replicated
```

that is `W / (T x P)` plus an allowance for the tensors every rank keeps whole, and,
with P above 1, for a stage holding more layers than another. `sharded`, `layers` and
`largest_layer` are the checkpoint's layout, read by the host from the safetensors
headers beside the weights (tensor names, shapes and byte ranges; no tensor data):
inside a numbered decoder layer (`layers.N.`, `h.N.`, `blocks.N.`) every tensor of two
or more dimensions is sharded except MoE routers (`mlp.gate.weight`,
`shared_expert_gate`) and latent-attention low-rank and indexer projections
(`q_a_proj`, `kv_a_proj_with_mqa`, `indexer`, `f_a_proj`, `g_a_proj`); everything
else (tensors of at most one dimension, every tensor outside the layers, multimodal
towers, other weight files, a draft model, the headers) counts as replicated. A
checkpoint without readable safetensors headers, or one measured by an older host,
keeps 10 % of `W` whole and splits the rest evenly across stages.

**What the engines replicate** (read from the installed sources: vLLM 0.30.0,
SGLang 0.5.21, TensorFold 0.6.5):

- vLLM and SGLang shard the embeddings and the output head by vocabulary across
  tensor-parallel ranks (vLLM `model_executor/layers/vocab_parallel_embedding.py:326`
  and `ParallelLMHead` at `:569`; SGLang `srt/layers/vocab_parallel_embedding.py:324`
  and `:602`). They keep whole on every rank: norms (full-size `RMSNorm`), the bias of
  a row-parallel projection (vLLM `model_executor/layers/linear.py:1714`; SGLang
  `srt/layers/linear.py:1507`), MoE routers (`ReplicatedLinear`, vLLM
  `model_executor/models/qwen2_moe.py:139`, SGLang `srt/models/qwen2_moe.py:345`) and
  latent attention's low-rank projections (vLLM `model_executor/models/deepseek_v2.py`,
  `q_a_proj` and `kv_a_proj_with_mqa` as `ReplicatedLinear`). With fewer key-value
  heads than tensor-parallel ranks they replicate the key and value projections
  (vLLM `linear.py:1050`, SGLang `linear.py:1017`); the layout does not model that
  case. Mamba and gated-delta convolutions, `A_log` and `dt_bias` are sharded by head.
- Pipeline stages hold contiguous layers (`get_pp_indices`, vLLM
  `distributed/utils.py:127`, remainder on all but the last stage; SGLang
  `srt/distributed/utils.py:105`, remainder on the last stages; both overridable,
  `VLLM_PP_LAYER_PARTITION` and `SGLANG_PP_LAYER_PARTITION`). The first stage holds
  the embeddings and the last the final norm and head (vLLM
  `model_executor/models/llama.py:388`, `:394`; with tied embeddings the last stage
  holds the embeddings too, `:379`).
- TensorFold runs `--tp 1` or `2` and has no pipeline parallelism
  (`src/tensorfold/cli_args.py:129`). Its families keep the embeddings whole on both
  ranks (`families/qwen3_5/cuda/distributed.py:165`,
  `families/nemotron_h/cuda/tp.py:107`) and, for GLM-5.3-Flash, the output head too
  (`families/glm5_next/cuda/split.py:32`, the `REP` rules); Qwen3.5-family and
  Nemotron-H split the head by vocabulary rows (`families/qwen3_5/__init__.py:324`,
  `families/nemotron_h/cuda/tp.py:108`).

Counting the vocabulary tensors and every other tensor outside the layers whole is
therefore exact for TensorFold's embeddings and conservative for vLLM and SGLang.

**KV cache and state.** `engine_config.memory.kv_cache` (declared or defaulted) is per
member, as `resources` are: each rank holds the cache of its own heads (the key-value
heads divided by T, at least one) for its own stage's layers, and the engines take one
token capacity for every rank, the smallest (vLLM `v1/core/kv_cache_utils.py:2760`;
SGLang `srt/utils/common.py:551` and `srt/mem_cache/kv_cache_configurator.py:2285`;
TensorFold `src/tensorfold/cuda/capacity.py:270`). vLLM's `--kv-cache-memory-bytes`
and SGLang's static fraction are per rank (vLLM `v1/worker/gpu_worker.py:542`; SGLang
takes the fraction of each rank's free memory, `kv_cache_configurator.py:2190`). The
member's request is its share, its KV cache and its margin; the margin, the startup
placeholder (`share x 2.25 + margin`), the graph allowance, an SGLang hybrid's state
reserve and a `host_backed` copy are derived from the share as on one host. The
context CapyCTL fits to the KV cache and the hybrid state slot still count the whole
model per token, which is conservative for a member; the timeouts keep the whole
checkpoint.

**Records.** A member's `engine_config.memory` records `member`: the topology, the
whole checkpoint's weights and the layout its share (`weights_bytes`) was taken with,
so a stored snapshot (which no longer states the topology) re-derives the same share,
and a provisional revision is re-resolved with its share once the checkpoint is
measured. The digest evidence carries the layout (`CheckpointDigestEvidence.layout`)
and a member's launch plan names the whole weights, which the host verifies, and the
layout (`SingleLaunchPlan.checkpoint_layout`, under the `engine_groups` capability),
so the host resolves the same share and SGLang's static pool holds the member's share.
A group revision accepted before this amendment keeps the whole-checkpoint charge;
deploy it again for the share.

**Limits.** With the placeholder startup peak, a 126 GiB checkpoint at TP 2 on two
121.7 GiB GB10 hosts (a share of about 64 GiB) fits once Ready but not while starting:
`64 GiB x 2.25 + margin` exceeds the 97.4 GiB automatic managed limit, so such a
deployment declares `memory.startup` (the group activation charges each member the
cold phase its revision resolved; it has no first-start whole-host fallback).
