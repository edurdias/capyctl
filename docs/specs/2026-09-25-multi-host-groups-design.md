# Multi-host engine groups — design

Date: 2026-09-25. Status: owner-decided. The owner's decisions of 2026-09-25 are
recorded in "Decisions" below. The six design choices those decisions left open were
settled by the owner on PR #42 the same day and are recorded under "Owner decisions
on this design".

This is a follow-up milestone after 0.1.0. It must not block 0.1.0: no task changes a
0.1.0 surface, every protocol addition is a new capability, and a deployment without
`topology` behaves exactly as it does in 0.1.0.

It will be recorded as ADR 0020, amending SPEC §11, §16.4 and §20, ADR 0013
(decision 1, the multi-host refusal) and the discrete-GPU design's
`multi_gpu_unsupported` rule, plus an amendment to ADR 0012 for peer exposure.

Implementation status: not started.

## Owner decisions on this design (owner-decided on PR #42, 2026-09-25)

1. **Engine environment variables.** Engine settings that exist only as environment
   variables are declared in YAML or by a CLI flag, at two levels: the engine
   profile (`env` in `engines.yaml` or `host.yaml`, or `mllm engine add ... --env
   K=V`, saved in the profile) and the deployment (`engine_config.env`, or `mllm
   deploy model ... --engine-env K=V`, saved with the deployment). Names are checked
   against the profile's host-approved list, `approved_env`, which admits globs.
   mllm-owned names (`NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP`) can never be
   set. Design in §2.1.
2. **Addresses are not transport.** mllm renders `--master-addr` (the head's
   declared peer address) and `VLLM_HOST_IP` (each host's declared peer address),
   and nothing named `NCCL_*` or `GLOO_*`. The single-rank loopback pin
   (`GLOO_SOCKET_IFNAME=lo`, `NCCL_SOCKET_IFNAME=lo`) is not applied to a group
   launch. If live bring-up shows gloo or NCCL bootstrap choosing a wireless or
   overlay interface, decision 4 is revisited then, with evidence.
3. **Host-check severity.** Every check warns by default; a host declares
   `groups.require_rdma: true` to make a missing `/dev/infiniband` or a small
   memlock limit refuse the launch.
4. **Group eviction.** The planner computes a victim set on each named host with the
   existing per-host rules (ADR 0013 decision 8) and evicts only when every host has
   a valid set; if any host cannot make room, nothing is evicted anywhere (SPEC §11:
   validate all hosts before evicting).
5. **First-version shape limits.** Groups are `instances: 1` and use one device per
   host (ranks per host = 1). The design and types carry N hosts × k local ranks,
   but k > 1 and `instances > 1` are refused until a later milestone.
6. **Post-wake canary.** A group wake is complete only after the head's readiness
   check *and* a short deterministic completion whose tokens match the reference
   recorded at first readiness. A mismatch fails the wake and restarts the group.
   It costs one short request per wake, for groups only. Upstream vLLM has reports
   of wrong output after sleep/wake at TP=2.

## Problem

A model that does not fit one host must run as one engine spread over several hosts:
tensor parallel (TP) splits each layer across ranks, pipeline parallel (PP) splits
layers across stages. mllm today runs every engine on one host:

- The domain has a two-host `GroupPlan` and a wire message, but nothing sends them,
  and the native launch path refuses every group launch
  (`crates/mllm-agent/src/native_execution.rs`, `LaunchSingle` only).
- Group reservation, fan-out launch, per-rank settlement and recovery (units U6, U7
  and U9 of the 2026-09-21 two-host plan) were never built.
- Configuration refuses a multi-host topology (ADR 0013 decision 1) and more than
  one device (`multi_gpu_unsupported`).
- Every multi-node engine flag is reserved, and the single-rank rendezvous is pinned
  to loopback (`runtime/loopback_rendezvous.py`).

The first target is to reproduce a published two-host run of Qwen3.8-Flash-Next
(hibrid48 weights, patched vLLM 0.30, TP=2 over the hosts' direct RDMA link, MTP K=5)
through mllm, at about the published average throughput.

## Decisions (owner, 2026-09-25, binding)

1. **Engine.** Patched vLLM 0.30 is rebuilt as a virtual environment on each host
   and registered with `mllm engine add --name`. Bring-up first uses a model that
   stock vLLM loads, at TP=2.
2. **Residency.** Deep park is supported from day one: group-wide sleep and wake
   with evidence from every rank. Upstream TP>1 sleep bugs are a recorded risk with a
   test plan.
3. **Peer exposure.** Trust the network. mllm does no firewall check. Rendezvous and
   NCCL ports are open on all interfaces during a run. mllm binds to the direct link
   where the engine allows it. This is a known risk, recorded in an ADR 0012
   amendment.
4. **NCCL.** mllm sets nothing; the engine and NCCL choose the transport.
5. **Readiness.** Head health only. Worker hosts still record their rank's process
   identities for ownership and stop evidence.
6. **Failure.** Any rank failure stops the whole group. Each host releases only on
   its own evidence. An unreachable host's share stays charged and uncertain.
7. **Ownership.** One owner record per rank, tied together by the instance. The
   design covers N-rank groups (TP × PP across N hosts), not only TP=2.
8. **Placement.** Named hosts only. The deployment lists exact hosts in rank order,
   head first. The scheduler never picks hosts for a group.
9. **Rendezvous port.** The server allocates it from a host-declared range (default
   25000–25099), records it in the group plan, and releases it only after every host
   proves stop.
10. **Host tuning.** The owner sets the compaction sysctl, memlock and
    `/dev/infiniband` access as root. mllm checks them before launch, warns or
    refuses, and never changes them.
11. **Weights.** Each named host downloads the model source in parallel through
    mllm. Checkpoint digests are compared across hosts before launch. Free disk is
    checked first.
12. **Engine order.** vLLM native multi-node first (multiprocessing backend:
    `--nnodes`, `--node-rank`, `--master-addr`, `--master-port`; workers
    `--headless`). SGLang next, with a quick-restart model.
13. **Success bar.** Through the router, within about 10% of the recipe averages
    (about 95 tok/s at 1 stream and about 735 tok/s at 64), with a concurrency sweep
    from 1 to 64. Peaks are reported too.

## Design

### 1. Vocabulary

- **Group**: one engine instance whose ranks run on more than one host. It is still
  one ADR 0013 instance: one generation, one route endpoint, one lifecycle claim.
- **Member**: the part of a group on one host. Member 0 is the **head**; it runs the
  API server. Every other member is a **worker** and runs headless.
- **Node rank**: the member's position in `placement.hosts` (head = 0). This is the
  engine's `--node-rank`.
- **Local ranks**: devices one member uses. First version: 1 (owner design decision 5).
- **Group plan**: the durable, server-written record of one group incarnation: every
  member, its host, node rank, devices, model path, peer address, the head's service
  port and the rendezvous port.

### 2. Deployment configuration

```yaml
schema_version: 1
kind: deployment
name: flash-next
runtime_profile: vllm-030-patched        # registered with `mllm engine add --name`
topology:
  tensor_parallel: 2
  pipeline_parallel: 1
placement:
  hosts: ["host-a", "host-b"]           # exact hosts, rank order, head first
residency: deep
model:
  source: {type: huggingface, repo: "org/model", revision: "<commit>"}
engine_config:
  context_length: 262144
  memory: {request: "83GiB", kv_cache: "41GB"}
```

Rules, checked at deploy time (errors in §15):

- `topology` is optional. Absent, or `tensor_parallel × pipeline_parallel = 1`,
  means a single-host instance, exactly as in 0.1.0.
- With a world size `W = tensor_parallel × pipeline_parallel > 1`, `placement.hosts`
  is required, has no duplicates, and its length `N` divides `W`; the local rank count
  is `W / N`. First version requires `W / N = 1` (`group_shape_unsupported`).
- `placement.selector`, `strategy` and `max_per_host` are refused with a topology;
  so is the `host:` shorthand (`group_placement_required`).
- `instances` must be 1 (`group_instances_unsupported`).
- `devices` may state a count of 1 per host and a sharing mode; naming a device id
  applies to every host (a one-GPU host names it `gpu0`).
- `memory.request` and `kv_cache` are **per member**. Each member's footprint is
  charged on its own host (§4).
- The multi-node engine flags stay reserved. Users never write `--nnodes`,
  `--tensor-parallel-size` and the like; mllm renders them from `topology` (§8).
- The runtime profile name must resolve on every named host. Resolution runs per
  host (ADR 0013 decision 3); a group deploys only if **every** named host resolves,
  unlike single-host instances where one is enough.
- The build fingerprint of the named profile must match on every host
  (`group_profile_mismatch`), because ranks of different builds cannot form a group.

The effective revision of a group is keyed by (deployment, revision) and records the
per-host resolutions it was built from.

Host policy gains a `groups` block:

```yaml
resource_policy:
  groups:
    peer_address: "192.0.2.10"           # this host's address on the direct link
    rendezvous_port_range: {start: 25000, end: 25099}   # default
    require_rdma: false                  # design decision 3
```

A host without `groups.peer_address` cannot be named in a group
(`peer_address_missing`). The agent verifies at start that the address is assigned
to one of its interfaces and reports it with its inventory; a mismatch refuses group
launches on that host (`peer_address_not_local`).

### 2.1 Engine environment

Some engine settings exist only as environment variables (the target recipe needs
`MBX_FUSED_DRAFT`, `MBX_PLE_REPLICATE`, `VLLM_MARLIN_USE_ATOMIC_ADD`,
`TORCH_NCCL_ASYNC_ERROR_HANDLING` and `TORCH_NCCL_HEARTBEAT_TIMEOUT_SEC`). Design
decision 1 gives them two levels, each declared in YAML or by a CLI flag. This
applies to every deployment, single-host or group.

| Level | YAML | CLI flag | Saved in |
|---|---|---|---|
| Engine profile (host administrator) | `env:` of the profile in `engines.yaml` or `host.yaml` | `mllm engine add ... --env K=V` (repeatable) | the profile |
| Deployment (deployment operator) | `engine_config.env:` | `mllm deploy model ... --engine-env K=V` (repeatable) | the deployment revision |
| Approval (host administrator) | `security.approved_env:` of the profile, a list of names or globs | `mllm engine add ... --approve-env GLOB` (repeatable) | the profile |

Rules:

1. **mllm-owned names are never settable**, at either level, whatever
   `approved_env` says: `NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP`, and the
   names mllm already renders or closes (`PATH`, `LD_*`, `PYTHONPATH`, `CUDA_HOME`,
   `CUDA_VISIBLE_DEVICES`, `MLLM_*`). A profile or deployment naming one is refused
   `engine_env_reserved:<name>`, and so is an `approved_env` entry that names or
   matches only such names.
2. **Approval.** A deployment's name must match an `approved_env` entry of the
   profile it resolves against (`engine_env_not_approved:<name>`). A profile's own
   `env` is written by the host administrator and needs no separate approval, except
   for rule 1. The existing safe names (`MAX_JOBS`, `FLASHINFER_NVCC_THREADS`,
   `TOKENIZERS_PARALLELISM`, `PYTHONUNBUFFERED`, `RUST_LOG`) stay allowed at both
   levels with their existing value rules.
3. **Globs.** An entry is an upper-case name made of `A`–`Z`, `0`–`9` and `_`,
   optionally ending in one `*` (for example `MBX_*`). A bare `*` is refused.
   Matching is exact or prefix; rule 1 is checked on the concrete name, so a glob
   never admits an owned name.
4. **Precedence.** The deployment's value overrides the profile's for the same name;
   the effective configuration shows each variable with its source (`profile` or
   `deployment`, SPEC §7 provenance).
5. **One way to say each thing.** `--engine-env K=V` merges into the deployment
   document before submission; a name given both in the file and by a flag is
   refused `engine_env_conflict:<name>`. Likewise `engine add --env` with a name the
   role document's profile already sets.
6. **Groups.** Every named host resolves the deployment against its own profile; a
   name not approved on any one host refuses the group deploy. The rendered
   environment of every member must be equal apart from `VLLM_HOST_IP`.
7. **Values** are bounded (4 KiB each, 64 names per level), contain no NUL or
   newline, and are recorded in the effective configuration and the recipe
   fingerprint. They are not secret storage: status and effective configuration
   show them.
8. **Changes.** A deployment env change is a revision (ADR 0013 decision 7, not
   count-only). A profile env or approval change changes the recipe fingerprint, as
   any profile change does.

### 3. Group plan

`GroupPlan::two_host` becomes `GroupPlan::new`, validating N members:

- members in node-rank order 0..N-1, distinct hosts, one head (rank 0);
- same profile fingerprint and checkpoint fingerprint on every member;
- distinct, non-loopback, non-unspecified peer addresses;
- the head has a nonzero service port; workers have none;
- a nonzero rendezvous port from the head's declared range;
- `tensor_parallel`, `pipeline_parallel` and local-rank count consistent with N.

`MemberPlan` gains `role` (head or worker), `model_path` (the member host's
materialized path), and loses the requirement that every member has a service port.
The plan also carries the instance's generation, so every member command is fenced
to one incarnation (ADR 0013 §5). The plan is written durably with the reservation
(§4) before any host is contacted, and it is never edited: a relaunch is a new
generation and a new plan.

### 4. Reservations per rank

- Resource owners become per member: `deployment:<id>/instance:<k>/member:<r>`,
  each charged on its own host's domains. The instance row points at the group plan;
  the group plan names the owners (decision 7).
- All members are reserved in **one** server-store transaction under ADR 0007 (fresh
  observations and epoch compare-and-swap on every named host). Either every member
  is reserved or none is, so concurrent group plans cannot deadlock on partial
  acquisition (SPEC §11, T27). This is atomic accounting in the server's store; it
  is not a cross-host atomic launch, and the design never claims one.
- The rendezvous port is allocated in the same transaction from the head's range,
  skipping ports held by any unsettled group plan on that host. Exhausted:
  `rendezvous_ports_exhausted`.
- If a member does not fit, the planner applies design decision 4: per-host victim sets
  on every named host, evicting only when every host can make room.
- Each member's reservation settles on its own host's evidence only (§7).

### 5. Preparation and weights

Before reserving, the coordinator makes sure each named host holds the checkpoint:

1. Each host's model source is materialized through the existing path
   (`MaterializeSource`, ADR 0008), started on every host at once. Each host
   checks free space against the source's size first and refuses with the existing
   `insufficient_space` before downloading.
2. When every host has materialized, each reports its checkpoint digest
   (ADR 0014 §7). The digests must be equal (`group_checkpoint_mismatch`); a
   mismatch names the hosts and digests and launches nothing.
3. A `Prepare(GroupPlan)` goes to every member after the reservation commits. It has
   no process effect. Each host checks: the profile resolves with the recorded
   fingerprint; the model path holds the recorded digest; the peer address is local;
   on the head, the rendezvous and service ports are free; and the host tuning checks
   (§11). Every host answers with a verdict and its warnings.
4. Any refusal releases every member's reservation (nothing was launched, so no
   process can exist) and records the refusal per host. A lost `Prepare` reply is
   retried under the same command id; `Prepare` is idempotent.

### 6. Fan-out launch

Once every member has prepared, the coordinator sends `Launch(GroupPlan)` to every
member **concurrently**. Workers are not held back until the head is ready: vLLM's
initialization waits on every rank (SPEC §11: "do not wait for the head to become
fully ready before starting workers it needs").

- Each host journals the launch durably before spawning (existing journal path),
  records the exact process identities of its member (the head's API server and
  engine core; the worker's headless process tree), and reports them.
- The native path renders the member's command from the plan (§8), under the same
  closed environment, key-guard and loopback API rules as a single-rank launch.
- A host that cannot launch reports a typed failure. Any member failing to launch,
  or exiting before group readiness, triggers group stop (§9).

### 7. Readiness

Decision 5: the group is READY when the head passes the existing native readiness
check (SPEC §6.1: model readiness, not HTTP liveness). A TP or PP forward cannot
complete without every rank, so a passing head check implies the collective works.

Worker hosts do not probe readiness. They report their member's process identities
at launch and report exits; those identities are the ownership and stop evidence.
The router routes only to the head's ingress. Until READY, no route is opened
(T30).

The initialization timeout defaults to the deployment's `timeouts.initialize`
(ADR 0014 A1); the target recipe needs about 4 minutes and allows up to 30, so the
live rows set it explicitly.

### 8. Engine rendering

**vLLM (first).** Every member runs the registered installation's `vllm serve` with:

| Flag | Head | Worker |
|---|---|---|
| model path | its host's materialized path | its host's materialized path |
| `--tensor-parallel-size` | `topology.tensor_parallel` | same |
| `--pipeline-parallel-size` | `topology.pipeline_parallel` | same |
| `--distributed-executor-backend` | `mp` | `mp` |
| `--nnodes` | N | N |
| `--node-rank` | 0 | r |
| `--master-addr` | head peer address | head peer address |
| `--master-port` | plan rendezvous port | same |
| `--headless` | absent | present |
| `--host`, `--port`, keys, guard | loopback service port, as single-rank | absent |

Environment: the single-rank closed environment, plus `VLLM_HOST_IP` set to the
member's own peer address (design decision 2). No `NCCL_*` or `GLOO_*` variable is set
or inherited, and the loopback rendezvous pin is not applied. The protected entry
(`runtime/vllm_entry.py`) gains a group mode: it compares every reserved
destination it already checks (`nnodes`, `node_rank`, `master_addr`, `master_port`,
`headless`, `distributed_executor_backend`, TP and PP sizes) against the values mllm
rendered and refuses any drift. The frozen plan's `tensor_parallel_size: 1` pin
(`crates/mllm-adapters/src/vllm/frozen.rs`) becomes the plan's value.

The recipe's own flags (`--speculative-config`, `--block-size`, `--kv-cache-memory`
and so on) are deployment `engine_config`/`extra_args` and host approvals, as for any
single-rank launch. Recipe variables other than transport are engine environment
(§2.1): the profile's `env` and the deployment's `engine_config.env`, both checked
against `approved_env`.

vLLM requires every node to see the model at a path; the plan carries each host's
own path. If live bring-up shows vLLM requires identical paths across nodes, the
hosts' model-store roots must match, and preparation refuses a mismatch
(`group_model_path_mismatch`). That is a live-found condition, not assumed.

**SGLang (later).** `--tp`, `--nnodes`, `--node-rank`, `--dist-init-addr
<head>:<port>`; the head serves HTTP. SGLang starts a dummy health server on nonzero
ranks whose health always passes, so worker health is never read, consistent with
decision 5. Decision 12's quick-restart model: a SGLang group is `restart_only`
(deep park refused `capability_missing:deep_park` for groups) until release/resume
across ranks is proven live. Flash-Next is not a SGLang target: SGLang rewrites its
47.7 GiB n-gram table on every restart.

### 9. Stop, failure and compensation

- **Stop** (operator, idle, switch victim): close the head's ingress and drain as
  today, then send `Terminate` to every member concurrently. Each host terminates its
  own journaled process tree and reports gone-evidence for its member.
- **Rank failure.** Any member's exit, launch failure or failed readiness marks the
  group failed and triggers a stop of every other member (decision 6). There is no
  rank replacement: an NCCL group cannot re-admit a rank.
- **Settlement.** Each member's reservation is released only on that host's own
  gone-evidence (ADR 0011, T31). The instance leaves its lifecycle step only when
  every member has settled.
- **Unreachable host.** Its member stays charged and marked uncertain (T32). Lease
  expiry never frees it. It settles when the host reconnects and reconciles its
  journal, or when the host is revoked and recovered (ADR 0016). The other members
  settle on their own evidence meanwhile.
- **Rendezvous port** is released only after every member has settled (decision 9),
  because a surviving rank may still hold it.
- **Recovery.** Under `recovery: reconcile`, a failed group relaunches as a new
  generation with a new plan only after every member of the old one has settled.
  Attempts count per instance (ADR 0011 decision 5).
- **Head crash with surviving workers (T31).** The worker hosts report the survivors
  and terminate them on the group stop; no competing activation starts while any
  member is unsettled.

### 10. Park and wake across ranks

The head's agent is the lead: it invokes each collective once through the head's
loopback control endpoint (SPEC §11). Workers never receive a sleep call.

- **Park (deep).** Close ingress, drain, then the head calls `/sleep` with the admin
  key (ADR 0012). The park settles only when the head call succeeded **and** every
  member's host reports its member's resident memory at or below the member's
  parked budget (per-rank evidence, `process_residency`). A member that reports
  nothing within the park deadline keeps the full charge and the group is uncertain;
  the planner then stops the group (T20: no blind repeated collective).
- **Wake.** The head calls `/wake_up`; each host reports resident memory back above
  the parked bound; the head readiness check passes; and the post-wake canary
  matches (design decision 6). Any failure stops the group and relaunches
  under recovery.
- Parked members stay charged at their parked budget on their own host; the parked
  set bound (`max_parked`) counts a group once per host it occupies.
- **Risk.** Sleep with multi-node `--headless` is not documented upstream; there are
  reports of wrong output after sleep/wake at TP=2 and a VMM race across TP GPUs; the
  recipe sets `NCCL_CUMEM_ENABLE=0`, whose interaction with sleep mode is unknown.
  The test plan (§16) covers these: repeated park/wake cycles with output comparison,
  and park under a concurrent single-rank engine on one of the hosts.

### 11. Host checks

Before launch (in `Prepare`) each host reports, and never changes (decision 10):

| Check | How | Default | With `require_rdma: true` |
|---|---|---|---|
| `vm.compaction_proactiveness` | read `/proc/sys/vm/compaction_proactiveness`; nonzero is a finding | warn | warn |
| memlock | the agent's `RLIMIT_MEMLOCK`, inherited by the engine; below unlimited is a finding | warn | refuse |
| `/dev/infiniband` | present, and `uverbs*` read-write for the service user | warn | refuse |

Warnings are shown in `status` and in the deploy result as `host_tuning_warning:<item>`;
refusals are `host_tuning_missing:<item>`. The engine's transport is never inferred
from these checks; the live rows read the RDMA port counters to show which transport
carried traffic.

### 12. Ports and peer exposure

- The head's service port comes from its existing `endpoint_port_range`, as today.
- The rendezvous port comes from the head's `groups.rendezvous_port_range` (§4).
- vLLM also opens listeners mllm does not choose: the torch TCP store on the
  rendezvous port binds every interface; the head's broadcast queue binds every
  interface on a random port; NCCL uses dynamic ports. None is authenticated.
- mllm narrows what it can: `--master-addr` and `VLLM_HOST_IP` name the direct-link
  addresses, the API server and every control endpoint stay on loopback with the
  per-launch keys and the guard (ADR 0012 unchanged), and nothing is exposed through
  ingress or the router.
- It does not narrow the rest (decision 3). The ADR 0012 amendment records: during a
  group run, unauthenticated rendezvous, broadcast and NCCL listeners are reachable on
  every interface of every member host, including any wireless or overlay network;
  mllm performs no firewall check; the operator is responsible for the network. The
  status of a group instance marks it `peer_transport: unauthenticated`.

### 13. Protocol and capability

- A new ADR 0017 capability, `engine_groups` (server to host), covers
  `Prepare(GroupPlan)`, `Launch(GroupPlan)` with the new member fields, member-level
  `Terminate` and residency reports for a group member, and the host's `groups`
  inventory (peer address, check findings). The server refuses to name a host
  without it: `host_capability_missing:engine_groups`.
- Changes are additive: new fields on new numbers, `PROTOCOL_VERSION` stays `"2"`,
  command encoding version stays `"1"`.
- Commands to a worker carry `member_id: "worker-<r>"`; the hard-coded `"head"`
  member ids in the controller (`remote_execution.rs`, `remote_readiness.rs`) take the
  plan's member id.

### 14. Status and CLI

`mllm status` shows a group instance with its topology, the rendezvous port, the
`peer_transport: unauthenticated` marker, and one row per member: host, node rank,
role, state, process identities, reservation, residency, last error and host-check
warnings. No new command is added; `deploy`, `start`, `park`, `stop` and `delete`
act on the group as a unit.

### 15. Error codes

| Code | Where | Meaning |
|---|---|---|
| `group_placement_required` | deploy | topology with no exact `placement.hosts`, or with `selector`, `strategy`, `max_per_host` or `host:` |
| `group_topology_invalid` | deploy | duplicate hosts, or host count does not divide TP × PP |
| `group_shape_unsupported` | deploy | more than one local rank per host (first version) |
| `group_instances_unsupported` | deploy | `instances` > 1 with a topology |
| `group_engine_unsupported:<engine>` | deploy | the profile's engine has no group support yet (SGLang until its phase) |
| `group_profile_mismatch` | deploy, prepare | the profile's build fingerprint differs between hosts, or does not resolve on one |
| `group_checkpoint_mismatch` | prepare | checkpoint digests differ between hosts |
| `group_model_path_mismatch` | prepare | only if live bring-up shows vLLM needs identical paths |
| `engine_env_reserved:<name>` | deploy, engine add, host start | the name is mllm-owned (§2.1 rule 1) |
| `engine_env_not_approved:<name>` | deploy | no `approved_env` entry of the profile matches (on some named host, for a group) |
| `engine_env_conflict:<name>` | deploy, engine add | the name is given both in the document and by a flag |
| `peer_address_missing` | deploy | a named host declares no `groups.peer_address` |
| `peer_address_not_local` | host start, prepare | the declared address is not on any local interface |
| `rendezvous_ports_exhausted` | reservation | every port in the head's range is held |
| `rendezvous_port_in_use:<port>` | prepare | the head's allocated rendezvous port is taken by something outside mllm |
| `service_port_in_use:<port>` | prepare | the head's loopback service port is taken |
| `host_tuning_warning:<item>` | prepare, status | a host check found a gap (warning, not an exit) |
| `host_tuning_missing:<item>` | prepare | the same, on a host with `require_rdma: true` |
| `group_member_failed` | status | a member exited or failed; carries host and node rank |
| `group_member_uncertain` | status | a member's host is unreachable; its share stays charged |
| `group_wake_mismatch` | wake | the post-wake canary differed (design decision 6) |
| `host_capability_missing:engine_groups` | placement | a named host cannot run group members |

Existing codes keep their meaning: `insufficient_space` (download), the
insufficient-resources family, and `multi_gpu_unsupported` for more than one device
on one host. CLI exits: deploy-time shape errors and the `engine_env_*` codes use the
invalid-configuration exit (2); `group_shape_unsupported`, `group_instances_unsupported` and
`group_engine_unsupported` use the unsupported exit (5); `rendezvous_ports_exhausted`
uses the insufficient-resources exit (4). No new exit number.

### 16. Testing

CPU and Fake-engine tests are not qualification. They pin configuration, accounting
and the lifecycle; only the live rows show that a native multi-node recipe works.

**Deterministic** (tagged with SPEC §20 IDs):

- **T03, T14, T37**: engine environment at both levels, by YAML and by flag;
  approval globs; owned names refused at every level and through globs; deployment
  overrides profile with provenance; conflicts between file and flag; group
  resolution refuses a name unapproved on one host.
- **T03, T14**: topology parsing and every deploy-time refusal; reserved flags stay
  reserved; effective configuration shows the rendered group flags with provenance.
- **T27**: two group plans sharing one host reserve all-or-nothing under concurrency,
  with no partial acquisition and no deadlock; rendezvous port allocation and reuse.
- **T15**: concurrent activation requests produce one group plan and one launch per
  member.
- **T30**: a member refusing `Prepare` releases every reservation; a member failing
  `Launch` stops the launched members and settles each on its own evidence; no route
  opens before head readiness.
- **T31**: head exit with a surviving worker; worker exit with a surviving head;
  both stop the group and settle per member.
- **T32**: a worker host disconnects; its member stays charged and uncertain; the
  port stays allocated; reconnection and reconciliation settle it.
- **T20**: group park and wake with per-member residency evidence; a member that does
  not report keeps its charge; no repeated collective; canary mismatch stops the
  group.
- **T34**: a host without `engine_groups` is refused typed and receives nothing.
- **T21, T37**: a group launch keeps the API and control endpoints on loopback with
  the per-launch keys; no transport variable is rendered or inherited.
- Runtime entry (Python unittest): group mode compares every rendered multi-node
  destination and refuses drift; single-rank mode is unchanged.
- The Fake engine gains a multi-member mode: members on several fake hosts, a
  headless worker, exits and residency on demand.

**Live rows** on the two hosts (host A head, host B worker) over the direct link:

| Row | Scenario | Pass |
|---|---|---|
| MH1 | Stock vLLM 0.30 build, a model stock vLLM loads, TP=2 | READY through the router; RDMA port counters rise on both hosts during load |
| MH2 | MH1, deep park and wake × 5 | per-rank residency drops and returns; canary tokens identical each cycle |
| MH3 | MH1, kill the worker's engine | group stops; each host releases on its own evidence; port released |
| MH4 | MH1, kill the head's engine | same as MH3 (T31) |
| MH5 | MH1, stop host B's agent service while READY | host B's share stays charged and uncertain; settles after the agent returns |
| MH6 | A → B → A: the group and a single-rank deployment on host A under pressure | eviction plan covers host A only when it must; correct release evidence (T16) |
| MH7 | Patched vLLM 0.30, Flash-Next hibrid48, TP=2, MTP K=5 | concurrency sweep 1–64 through the router; average ≥ ~95 tok/s at 1 and ≥ ~735 at 64; peaks reported |
| MH8 | MH7, one deep park and wake | per-rank evidence and canary as MH2 |
| MH9 | SGLang group, small model, TP=2, `restart_only` (later phase) | READY through the router; worker kill stops the group as MH3 |

Live work is authorized on the two hosts; the patched build is a new virtual
environment on each host (decision 1) and changes no existing engine environment.
Host tuning is the owner's root step (decision 10), done before MH1.

## Known risks

- **Patched build.** Rebuilding the patched vLLM 0.30 as an aarch64 venv (patches,
  fastsafetensors, sm_121a) may not reproduce the published image.
- **Unauthenticated peer listeners** on every interface during a run (decision 3,
  ADR 0012 amendment).
- **Transport choice.** With no NCCL variables, NCCL may choose TCP or a single HCA;
  the published numbers used two merged HCAs. MH7 may miss the bar for that reason;
  the counters in MH1 make it visible.
- **Sleep at TP>1** across nodes is undocumented upstream, with open corruption
  reports (§10).
- **Resources and time.** About 105 GB per host at about 9 MB/s is about 3.25 hours
  per host; the model occupies most of both hosts, so single-rank deployments cannot
  co-reside while it serves.

## Out of scope

- The scheduler choosing hosts for a group; `instances > 1` of a group; more than one
  device per host (first version).
- Rank replacement or partial group restart.
- Ray and SGLang's Ray mode; data parallel and expert parallel across hosts.
- Container launchers (SPEC F5).
- Firewall checks, mllm-set NCCL tuning, sysctl or limit changes.
- Peer-to-peer weight copy between hosts.
- A SGLang deep park for groups (quick-restart only until proven).
- Shared KV caches across group members.
