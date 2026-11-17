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
   Reaffirmed by the owner on 2026-10-08: decision 3 stands, now documented per engine
   listener (address, port, authentication) with firewall guidance in the user guide
   ([Ports a group opens](../../guide/several-machines.md#ports-a-group-opens)).
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
SGLang group whose deployment enables DP attention, also the seven fixed ports SGLang
derives from the rendezvous port `P` and binds there (`P+1` to `P+6` from
`PortArgs.init_new` and the worker-port handshake at `P+13` from
`data_parallel_controller.py`, as SGLang 0.5.21 computes them; SGLang moves the six to
`P-7` to `P-2` when `P+6` exceeds 65535 but never moves `P+13`, so any `P` above 65522
is refused `rendezvous_port_in_use:<P>`; checked when the `Launch` re-runs these checks,
because only the `Launch` carries the deployment document; the head's one ephemeral
PUSH socket per DP rank is chosen by the kernel at launch and is not checked); on a
SGLang worker, its loopback port is free; and host tuning:

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
Amended 2026-10-15 (owner decision, found live in MN3, MN5 and MN6): TensorFold answers
the ids in its response's own `tensorfold` block, where they are read. A TensorFold
head whose answer carries no ids passes readiness and the stall check on its non-empty
generated text, and the controller logs that it did; a vLLM or SGLang head always
needs the ids, and the wake canary compares ids only.
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
| Backend | `--distributed-executor-backend mp` | default | NCCL, loaded at run time: `TF_NCCL_LIB=<installation libnccl.so.2>` on every rank, else refused `nccl_library_missing` (amended 2026-10-15, found live) |
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
  ephemeral port on rank 0; SGLang with DP attention binds seven fixed ports derived from
  the rendezvous port and one ephemeral port per DP rank on the head (§7); NCCL uses
  dynamic ports; gloo opens ephemeral CPU-group
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

## Amendment of 2026-10-15: a member's leader is reaped only once its group ends

Found live (MN5 vLLM): SIGKILL of a TP2 worker member's vLLM process left the
`VLLM::Worker_TP1` it had forked (38 GiB of device memory) and a resource tracker
running. A worker's tree is recorded when the tree first settles after the spawn,
before vLLM forks its workers, so only the leader was recorded. The leader was reaped
at once, and a leaderless process group never authorizes a signal (its id can name an
unrelated group once the leader's pid is reused), so every `Terminate` ended
uncertain, the member stayed charged and the stop expired unsettled.

The launcher that spawned a launch's leader now waits for its exit without reaping it
(`WNOWAIT`) and reaps it only once no other process of its group runs. While the
leader is held, its pid, and so the group id, cannot be reused, so §11's "terminate
its journaled process tree" signals the whole group through the held leader, live or
dead, the processes never recorded included. A held leader that has exited reads
gone and its exit is reported as before, with the status the wait observed. Release
still needs gone evidence for every recorded identity and an empty group. A leader
this process did not spawn (a launch from before a restart of the host role) is not
held, and its leaderless group is still never signalled: that case stays uncertain and
charged, as before.

## Amendment of 2026-10-15: park evidence and the parked charge per member (owner decision)

Found live at MN4 (vLLM 0.30.0 and SGLang 0.5.21, TP2 over two GB10 hosts): the
head's collective succeeded (`POST /sleep?level=2` and `POST /release_memory_occupation`
both 200), yet the group stayed `parking` for the whole 15-minute park step and then
stopped. Three causes, each fixed here:

- **A worker recorded its leader alone.** A worker's Launch reported its process tree
  one second after the spawn, before the engine forked its rank process (vLLM's
  `VLLM::Worker_TP1`, SGLang's rank scheduler) once its imports were done, so the
  member's recorded processes were not the ones holding its device: its host's
  `process_residency` named none of them and its saver enrollment was refused
  (`owner_not_a_launch_member`). A worker's Launch now reports its tree only once it
  holds a process per local rank besides the spawned one (none for a TensorFold
  follower), bounded by the step's deadline (§8: "worker: its process tree").
- **A vLLM member was judged on its anonymous pages.** Its host's `process_residency`
  adds each process's anonymous resident pages to its device memory on a unified
  host; a parked vLLM keeps more than its 2 GiB placeholder there (4.7 GiB measured on
  one GB10 host), so no report ever settled. The park (and the wake) is now judged on
  the device memory the driver attributes to the member's processes, which is what
  the collective releases and restores; a SGLang member stays judged on its saver map.
- **The wait ran to the step's deadline.** The collective returns once every rank
  acted, so each member's own evidence is awaited for a bounded window after the head's
  reply (60 s). A member still resident (or not resident after a wake) then fails the
  group (`group_member_failed`); one that reported nothing of its own is
  `group_member_uncertain` and keeps its full charge. Either stops the group, each
  member released only on its own host's evidence (§11).

**The parked charge.** §12's "parked members stay charged at their parked budget" now
reads: a member's parked budget is its measured residue once one is recorded (ADR 0014
amendment A13). A park of a revision whose phases CapyCTL derived needs, from each
member's own host, the residue its processes hold in a sample taken after the head's
reply (device memory on a discrete host's `device` domain, device memory plus
anonymous pages on a unified pool). The residue is recorded for the revision on that
member's host and for the launch, and the member is charged it (never below its
placeholder, never above its Ready charge); a member reported without one leaves the
park unsettled. A revision with declared `resources` keeps its declared parked phase.
ADR 0014 amendment A19's growth bound applies per member, each against its own host's
`parked_growth_limit`; a member past it turns the group's next park into a group stop.

CPU and Fake-engine tests only; the MN4 rows qualify it.

## Amendment of 2026-10-15: a member with no recorded identities settles on its host's journal

Found live (MN2 SGLang): a worker's Launch reply named two processes under one role,
so its identities could not be recorded and the member, dispatched, could settle on
nothing (§11, R23: a dispatched member settles only on recorded identities). It stayed
`uncertain` and charged after its host had stopped every process.

A dispatched member with no recorded identities (its Launch reply lost, refused or
not recordable) now settles on its own host's gone-evidence for the Launch it was
dispatched with: the host's answer to the Terminate of that Launch's handle names every
process its journal recorded for it, each gone, with the claim released. The store
accepts it only for that handle and that host, with at least one well-formed process
(roles may repeat, a process may not), and records those processes with the
settlement. A host with an empty journal names nothing and claims nothing, which is
no evidence: the member stays charged, as §11 requires. No protocol field is added.

## Note of 2026-10-15: SGLang groups do not park in 0.1.3

Found live (MN4, SGLang 0.5.21, TP2 across two GB10 hosts): with the amendment
above, a group deep park completes, but the first wake fails. The head reloaded
its weights from disk for 222 s, the wake canary ran, and 3 s later the wake
went `restore_uncertain` and the group was stopped. It fails safe; group wake
is not usable. vLLM groups pass 5/5 deep park cycles and are unchanged.

Owner decision 2026-10-15, narrowing §2's support table for 0.1.3: an SGLang
group (world size above 1) does not park. `group_support("sglang").deep_park`
is false, so a group that states no residency resolves `restart_only`, and one
that states `deep` or `host_backed` is refused at deploy with
`capability_missing:deep_park` and the hint to declare `restart_only` (both
parking tiers, for every engine whose group cannot park). A single-host SGLang
deployment, its residency and its pinned identities are unchanged. Lift the
rule once a live group wake passes on the installed SGLang.

## Note of 2026-10-16: the SGLang group wake failure was stale evidence; SGLang groups park again

The cause of the MN4 failure above was in the controller. A wake read every
member's own host report (each host's observation time), then ran the head's
readiness probe and the canary, then committed those reports; the store accepts
a report only while it is within its host's observation TTL (2 s by default and
on the lab hosts). SGLang's probes right after a wake that reloaded the weights
took longer than that, so the reports were stale at the commit, the store
refused them (`evidence freshness`) and the group stopped `group_member_uncertain`.
vLLM's probes are fast, so its wakes passed. The live failure text was not
kept; the group world reproduces it with probes that outlast the TTL
(`a_wake_whose_probes_outlast_the_observation_ttl_completes`).

A park can hit the same rule (found live 2026-10-16, the first MN4 SGLang park
on this fix: `park_uncertain`, `evidence freshness`, 9 s after acceptance). The
members settle at different times, each host pushing its reports on its own
period (an SGLang member's saver map every 2 s; a park also waits for a residue
sampled after the head's reply), and the controller kept each member's first
settling report while it waited for the others, so the earliest one could be
older than the TTL when the last settled. The 0.1.3 SGLang parks that passed did
so by timing.

Every report a park or wake commits is now fresh by the store's own rule: the
controller reads the TTL the step's commit applies (the observation TTL of its
host policy), asks every member's host again on every poll, settled members
included, and hands the commit the reports only when each member's latest one
settles and is younger than that TTL less a margin (300 ms, at most half the
TTL). A member with no report that young by the end of the evidence window is
silent (`group_member_uncertain`, its charge kept), as one with no report at all.

§12's wake also ends with one more read: once the canary passed, every member's
own host report is read again, observed after the canary, under the same rules
(each member on its own host's report, a rank not resident fails the group at
that rank, the bounded evidence window, no second collective), and those reports
are the ones the wake commits. A single launch's wake had the same order (the
restore's evidence, then the canary, then the commit); it now commits that
evidence with the canary's answer as its observation, the launch's engine
processes checked alive with it.

The note of 2026-10-15 is lifted: `group_support("sglang").deep_park` is true
again, an SGLang group that states no residency resolves `deep`, and `deep` is
accepted at deploy. The refusal of both parking tiers stays for an engine whose
group cannot park (TensorFold). The branch merges once the MN4 SGLang row (deep
park and wake cycles on both lab hosts) passes live.

## Amendment of 2026-10-25: a world size above one without named hosts is one host (ADR 0035)

Owner decisions 2026-10-25 ([ADR 0035](0035-multi-gpu-one-host.md)).

- **§2.** `topology` with fewer than two `placement.hosts` is no longer refused
  `group_placement_required`: it is one instance on N GPUs of one host, not a group (no group
  plan, peer address, rendezvous port or `engine_groups` capability; the single-host loopback
  pin applies). `host`, `placement.selector`, `strategy`, `max_per_host` and `instances` stay
  allowed there, and standalone accepts it. `pipeline_parallel` above 1 on one host is refused
  `group_shape_unsupported:<engine>`. `topology: auto` beside two or more hosts is refused
  `group_topology_invalid`: a group's shape is the operator's. Groups keep `local_ranks == 1`.
- **§11.** The stall rule covers every instance with N > 1 on one host as well, with the same
  `groups.stall_timeout`; its failure is `tp_stalled`.
- **Amendment of 2026-10-07.** The per-rank share (`ceil(sharded / T) + replicated`) sizes each
  GPU of a one-host instance with `T = N`, `P = 1`.

## Proposed amendment 2026-11-09 — private-link ingress and group socket interfaces

**Status: Proposed, for owner review.** The owner authorized starting this design
on 2026-11-09, not its implementation or the decisions below.
**Related:** [issue #178](https://github.com/edurdias/capyctl/issues/178).
**Would amend on acceptance and implementation:** decisions 3 and 4, §§3, 7, 10,
13 and 14 above; SPEC §15 and T03, T21, T30, T34 and T37; the exposure wording in
[ADR 0012's amendment](0012-deep-park-default-on.md#amendment-2026-10-05--peer-exposure-of-engine-groups-adr-0028).
The accepted text above remains the current contract until then. This amendment
does not change [ADR 0019](0019-discrete-gpu-and-network-endpoint.md)'s public
inference listener or [ADR 0035](0035-multi-gpu-one-host.md)'s single-host loopback
collectives.

### A1. Problem and evidence

Two GB10 hosts joined by a cable should not need a tailnet merely to serve one
group. A private cable address such as `10.42.0.2` is currently refused for host
ingress, even with `transport: trusted_private_link`. Meanwhile NCCL can select a
different interface from the one carrying the member's `peer_address`.

The current implementation has three relevant boundaries:

- `crates/capyctl-config/src/remote_roles.rs`, `private_ingress_endpoint` and
  `HostConfig::parse`, accept numeric HTTP endpoints only on loopback or
  `100.64.0.0/10`, require a nonzero port and require `ingress.bind` to equal the
  advertised endpoint. `crates/capyctl-store/src/host_publication.rs` repeats the
  endpoint check both when freezing a binding and when reading its destination.
  Changing only the host parser would therefore leave the feature unusable.
- `crates/capyctl-agent/src/host_checks.rs` resolves a peer IP through
  `getifaddrs`. `native_execution.rs::group_args` uses it for vLLM and SGLang,
  but skips TensorFold. The resolver currently takes the first matching interface.
  Adapters render the head's peer IP for rendezvous and the member's own IP for
  vLLM/SGLang broadcast traffic. An advertised rendezvous IP is not a bind
  restriction on torch's wildcard TCP store.
- `crates/capyctl-adapters/src/group.rs`, the vLLM/SGLang initializers,
  TensorFold's argument renderer and `runtime/loopback_rendezvous.py` implement
  decision 4: groups strip NCCL settings; vLLM and SGLang retain only the rendered
  `GLOO_SOCKET_IFNAME`. The Python group guard also rejects NCCL settings, so
  changing the Rust environment alone would not work.

The runbook entries “Group listeners documented” (2026-10-08), “Multi-node live
rows MN1-MN8” (2026-10-15), and “Group parks and wakes commit only fresh member
reports” (2026-10-16) establish existing two-host bring-up, RDMA use, failure
cleanup and deep park/wake evidence. They do not establish acceptance of RFC 1918
ingress or enforcement of an NCCL interface pin. Their timings are not a baseline
for this proposal.

### A2. Options considered

| Option | What it buys | Why propose or reject it |
|---|---|---|
| Fixed private-address allowlist, plus the existing explicit trust declaration | Accepts ordinary cable addresses without another copy of the network configuration | **Proposed.** Small, auditable rule; the declaration acknowledges trust, not proof of isolation. |
| Require an operator-declared link CIDR, such as `ingress.link_cidr` | Detects an endpoint outside the operator's stated subnet | Not proposed. A CIDR neither authenticates peers nor proves that a private subnet is a cable; it duplicates interface configuration. If chosen instead, constrain the entire CIDR to the private allowlist and reject public endpoints even inside a declared CIDR. |
| Require an operator-declared interface; bind sockets to the device | Expresses a device rather than an address and can narrow some traffic | Not proposed as the ingress contract. Interface names differ by host, address discovery still matters, and device binding requires privileges or engine support CapyCTL does not currently have. An exact NCCL interface selector is useful, but it is not `SO_BINDTODEVICE` or a firewall. |
| Re-address the cable inside `100.64.0.0/10`, or keep a tailnet for ingress | Avoids the current ingress rejection | Reject as the required workflow: can conflict with an existing overlay and does not fix NCCL selection. |

### A3. Proposed ingress rule and host document

Keep the existing host fields; add no CIDR, interface or general-purpose NCCL
environment escape hatch. This is a partial host document, not a runnable file:

```yaml
name: host-b
ingress:
  transport: trusted_private_link
  address: "http://10.42.0.2:8444"
  bind: "10.42.0.2:8444"
resource_policy:
  groups:
    peer_address: "10.42.0.2"
    rendezvous_port_range: {start: 25000, end: 25099}
    require_rdma: false
```

With `transport: trusted_private_link`, accept a numeric unicast endpoint inside
exactly these ranges: RFC 1918 (`10/8`, `172.16/12`, `192.168/16`), IPv4 link-local
(`169.254/16`), IPv6 ULA (`fc00::/7`), existing shared address space
(`100.64/10`), and loopback (`127/8`, `::1`). Retain exact address/bind equality
and a nonzero port. IPv6 HTTP URLs use brackets, for example
`http://[fd42::2]:8444`. Canonicalize IPv4-mapped IPv6 before classification so
it cannot bypass the IPv4 rule.

Refuse public/global addresses, wildcard and multicast binds, IPv4 limited
broadcast, DNS names, URL credentials, paths, queries and fragments. Reject
subnet broadcast/network addresses when the host's actual interface prefix makes
them non-unicast; `/31` point-to-point endpoints are usable unicast. IPv6
link-local with scope IDs is not added: it needs a separate scoped-address
contract across the server and hosts. The broad private ranges do not mean every
reserved address within them is a usable interface address.

The declaration is required even for a private address; no address is
automatically “protected.” No generated document is widened and no running
binding's destination changes. Ordinary document overrides keep their existing
precedence and undergo the same validation.

### A4. Proposed group rendering

For every newly planned group, resolve each member's peer address to exactly one
local interface, for TensorFold as well as vLLM and SGLang. Refuse a missing or
ambiguous match rather than guessing from the default route or hostname. Recheck
at Prepare and immediately before Launch; a changed interface requires a new
plan, not silent rerouting.

Render on every rank:

- `NCCL_SOCKET_IFNAME` with the value `=<interface>`: the leading `=` requests an
  exact match, so `link0` cannot also select `link01`.
- `NCCL_SOCKET_FAMILY=AF_INET` or `AF_INET6`, matching the plan's peer-address
  family, to avoid selecting the other family on a dual-stack interface.
- `GLOO_SOCKET_IFNAME=<interface>` for vLLM and SGLang, as today. TensorFold
  drives NCCL directly and needs no new Gloo setting.

Keep the rendezvous destination at the head's peer address and all existing own-IP
renderings. Validate one address family across the group, and render bracketed
IPv6 host/port syntax wherever an engine expects a combined endpoint. Do not
apply these rules to ADR 0035's one-host multi-GPU instances: their loopback pin
is unchanged.

The [NCCL environment contract](https://docs.nvidia.com/deeplearning/nccl/user-guide/docs/env.html#nccl-socket-ifname)
documents exact interface selection and `NCCL_SOCKET_FAMILY`. It does not promise
selection of one particular IP when several same-family IPs share that interface.
Live evidence must therefore record the selected IP, not infer it from the env.

CapyCTL continues owning all `NCCL_*` and `GLOO_*` names. Only the two rendered
NCCL names survive group environment filtering and the Python entry guards;
configured or inherited overrides stay refused or stripped as appropriate.
Expected values participate in the protected launch contract and its fingerprint.
Host-local interface names may differ between ranks.

**RDMA is a separate decision.** `NCCL_SOCKET_IFNAME` selects bootstrap/socket
interfaces, not RDMA HCAs, ports or GIDs. This proposal leaves NCCL's RDMA choice
unchanged, including `require_rdma`'s existing tuning checks. It does not render
`NCCL_IB_HCA`, force TCP, or claim that all collective payload traffic is confined
to the cable. The owner must accept that bounded scope or require a larger
transport policy before implementation: exact, validated HCA/port/GID selection
for the link, or an explicit socket-only mode. Neither is silently substituted.

### A5. Trust model and status

`trusted_private_link` means the operator trusts the network carrying cleartext
ingress. It does not add encryption, authentication of the cable, source-CIDR
filtering or proof that the address is unreachable from another network.
Routing, bridging or a VPN can make even a private destination reachable elsewhere.
Binding an IP restricts the local destination, not necessarily the incoming device.

Any machine able to route to host ingress can connect to its TCP listener.
Existing per-binding ingress credentials, inference route/method allowlists and
gate lifecycle still decide whether requests are accepted; no engine admin key or
control route is exposed. A party observing the trusted cleartext path may observe
requests and credentials. Server bootstrap/control TLS is unchanged; this is not
ADR 0019's public inference listener and grants no permission for public ingress.

Any party able to reach unauthenticated peer listeners is still inside the engine
trust boundary. Compromising the other cable endpoint, a bridge or a routed peer
can permit code execution as the engine user through pickled objects, theft of
per-launch keys, or denial of service. A private range or NCCL pin is not peer
authentication. Torch's wildcard rendezvous store and any other engine listener
not constrained by its own address setting remain exposed on other interfaces.
The operator's isolation/firewall responsibility in §13 stands. CapyCTL does not
change or certify firewall rules, routes, interfaces or RDMA isolation.

Status must retain `peer transport: unauthenticated`. Show the planned peer IP,
resolved interface, NCCL socket family and that RDMA selection is automatic;
do not replace the peer warning with “secure” or “all ports bound to the link.”
Update the guide's per-listener table precisely: NCCL socket selection narrows,
the torch store still binds wildcard, and RDMA remains separate.

`validate config --host` can compare declared ingress and peer IPs, but cannot
discover a remote host's interfaces. It warns `ingress_peer_address_differs` when
both non-loopback addresses differ, without claiming they are on different NICs.
The host reports `ingress_peer_interface_mismatch` only after a local comparison
proves different interfaces. This is a warning, not a refusal: separate protected
ingress and collective networks remain legitimate. Offline validation explicitly
reports that interface locality and reachability are unverified.

### A6. Validation, refusal codes and accounting

The following table distinguishes existing codes from proposed additions:

| Check | Result and stage |
|---|---|
| Endpoint outside the allowlist, malformed URL, missing transport declaration, unequal bind, zero port | Existing `invalid_config`, exit 2, at the precise `ingress` field; schema validation before side effects. Unknown fields retain the existing unknown-field refusal. |
| Ingress IP absent locally or not usable unicast under the local prefix | New `ingress_address_not_local`, exit 2, at host start before opening ingress. Diagnostic distinguishes absent from non-unicast. |
| Peer absent locally or differs from the host's declared peer | Existing `peer_address_not_local`, Prepare/Launch refusal. |
| Peer IP assigned to more than one distinct interface, or interface changed since Prepare | New `group_peer_interface_ambiguous` or `group_peer_interface_changed`, Prepare/Launch refusal naming the host and peer IP. |
| Mixed group peer-address families | New `group_peer_address_family_mismatch`, exit 2 at group validation, before reservation. |
| Required new networking behavior unsupported by server/host | `host_capability_missing:private_link_networking`, before sending Prepare/Launch; normal ADR 0017 version-skew refusal takes precedence. |
| Rendered address, interface or socket family differs at protected entry | Existing `group_drift:<field>` launch refusal. No fallback to an unpinned launch. |

Apply the endpoint rule consistently in configuration, host publication, binding
freeze and destination readback; do not weaken destination allowlists or let a
mutable discovery record redirect a frozen binding. Missing interface data is a
refusal, not “pick the first interface.” A failed preflight starts no member.
A failure after fan-out uses §11: stop every launched member, retain each charge
until its own gone-evidence, and never release merely because networking failed.

### A7. Version skew, wire contract and rollout

Add one ADR 0017 capability, `private_link_networking`, covering expanded private
ingress validation and the new group socket contract. A new server requires it
on every member for a newly created pinned group; an older host is never sent a
plan it could execute without the pin. Ordinary version compatibility and
drain-only rules remain in force.

Use additive wire fields on new numbers for the group's networking-contract
version and per-member resolved socket-interface evidence. The agent publishes
the interface with its peer inventory; the server freezes it with the plan and
the host revalidates it at Prepare and Launch. The negotiated launch contract,
not merely the host's current binary, determines rendering. Publish expanded
ingress only after the server negotiated the capability; an older server must
not discover an endpoint it cannot validate. No protocol or command-encoding
version bump is proposed.

Existing frozen revisions, binding destinations and running processes retain
their recorded contract. Recover/stop can handle a legacy unpinned plan without
claiming the pin; never reinterpret or silently relaunch it as pinned. Moving to
the new group contract requires an explicit revision update and a new generation.
Omitted new fields retain legacy fingerprint/encoding bytes. The implementation
must cover both store ingress checks, config publication, capability requirements,
durable plans, environment fingerprints and runtime guards in one cutover.

### A8. Tests and live qualification required

Add tests under existing acceptance IDs rather than allocating a new T-ID:

| ID | Required evidence |
|---|---|
| T03, T37 | Table-test every allowed range at its boundaries and adjacent disallowed addresses; loopback/CGNAT regression; ULA and mapped IPv6; public, wildcard, multicast, broadcast, scoped IPv6, DNS, URL components, port zero and unequal bind refusals. Cover host parsing and both store ingress call sites. |
| T03, T30 | Inject no match, one match, duplicate interface entries, two distinct matching interfaces, non-unicast endpoints, an interface change between Prepare and Launch, and mixed address families. Refuse before spawn; repeated entries for one device are not ambiguity. Cover both warnings and honest offline validation. |
| T21, T22, T30, T41 | Head and worker rendering for all three engines; exact `=link0` rather than prefix selection; address-family rendering; IPv6 rendezvous formatting; inherited/configured NCCL variables cannot override; Python guards retain only expected values and refuse drift; TensorFold now resolves an interface. |
| T21, T37, T46 | Ingress still denies controls and unapproved destinations; engine API/controls remain loopback-only and keyed; single-host and local multi-GPU loopback contracts remain unchanged. |
| T30, T31, T32 | A member's failed networking preflight or post-spawn failure opens no early route; partial launches stop; unreachable members stay charged until evidence permits settlement. |
| T34, T39 | New server/old host, old server/new host, missing capability, drain-only host, frozen legacy plan read/stop/recovery and explicit contract update; no silent pin drop, destination rewrite or fingerprint reinterpretation. |

After implementation and owner approval, add a direct-link variant of MN1–MN4
on **two GB10 hosts with a cable**, using approved, recorded engine builds:

1. Use a privately addressed cable, initially RFC 1918, with other host networks
   still present. Put host ingress and peer IPs on the cable; the server must
   have a route to ingress over that protected link. Do not require a tailnet.
2. For vLLM, SGLang and TensorFold, reach READY and complete 20/20 requests
   through the router. On both hosts record only the selected network variables,
   owned TCP sockets and actual connection addresses; never dump an entire process
   environment or keys. Check exact interface selection with similarly prefixed
   interface names using fixtures, not by changing a lab interface.
3. Record NCCL's actual bootstrap/socket interface and chosen transport. If RDMA
   is active, identify its HCAs/ports from bounded read-only evidence; do not
   label the payload “cable-confined” without that evidence. Exercise ULA
   end-to-end before claiming native IPv6 qualification; parser tests alone
   establish only config acceptance.
4. Preserve the existing deep park/wake and stop behavior (vLLM/SGLang), and
   restart-only behavior (TensorFold). Verify no owned listeners/processes remain
   after cleanup. Failure injection, if needed, signals only recorded
   CapyCTL-owned processes; missing-interface cases use deterministic fixtures,
   never interface/firewall changes on the hosts.

Acquire the normal per-host live-session marker, private state/prefix and
non-default ports; stop every deployment and role started and remove the marker
after cleanup. Do not touch unrelated training. While training is running, this
is functional evidence only: record no timing or memory measurements. A later
performance comparison requires an uncontended session. CPU and Fake-engine
tests are not qualification. No live check was run for this proposed amendment.

### A9. Decisions requested from the owner

1. Accept the fixed private-unicast allowlist with the existing
   `trusted_private_link` declaration, rather than require a new link CIDR?
2. Accept exact, derived NCCL socket-interface and address-family pins for every
   new group, including TensorFold, with no operator-settable NCCL variables?
3. Accept socket/bootstrap pinning with RDMA selection unchanged, or require
   validated HCA/port/GID selection or explicit socket-only operation before
   implementation? The narrower proposal is not a claim that all traffic stays
   on the cable.
4. Retain §13's operator-managed isolation and known wildcard-listener risk, with
   ingress/peer mismatch warnings rather than forced same-interface ingress?
5. Accept capability-gated, explicit-revision rollout and the two-GB10 functional
   qualification above before any new native-engine support claim?
