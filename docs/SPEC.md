# CapyCTL — Architecture and Implementation Handoff

**Revision:** 0.2  
**Date:** September 10, 2026  
**Owner:** Eduardo Rodrigues Dias  
**Status:** Consolidated design for implementation planning. No CapyCTL runtime has been implemented or qualified by this document.  
**Supersedes:** `capyctl-initial-design.md`, revision 0.1, and conflicting configuration/CLI sketches in the preceding discussion.  
**Engine delivery order:** vLLM first; SGLang immediately next.

> One endpoint. Bring your own inference engines. Explicit deployment ownership, safe model residency transitions, and aggregate resource control.

## How to use this specification

Read sections 1–5 for the product boundary, sections 6–13 for behavior and safety, and sections 14–19 for interfaces, configuration, and delivery. Section 20 defines acceptance tests. `AGENTS.md` is the companion working agreement for contributors; this document is authoritative when summaries differ.

**MUST / MUST NOT** identify required behavior. **SHOULD** identifies a recommended default that may be changed with a documented architectural decision. Implementation language, internal libraries, numeric default tuning, and the illustrative YAML field names are baseline proposals, not claims of separately approved implementation details. The requirements and ownership boundaries are the established direction.

All CapyCTL commands, protocol names, and YAML below describe a proposed product. They are not available commands or tested engine recipes. Sources establish specific upstream behavior only. A source-backed engine capability does not establish that a particular patched build, checkpoint, cache layout, or multi-node combination works.

## Navigation

| Area | Sections |
|---|---|
| Product and deployment model | [Scope](#1-purpose-and-agreed-scope), [objects](#2-vocabulary-and-durable-objects), [roles](#3-roles-process-boundaries-and-packaging), [enrollment](#4-enrollment-and-host-preparation), [ownership](#5-managed-deployments-attachment-and-adoption) |
| Runtime behavior | [Lifecycle](#6-deployment-lifecycle-and-durable-intent), [resources](#7-resource-contracts-and-aggregate-boundaries), [profiles](#8-runtime-profiles-and-the-enginelauncher-contract), [adapters](#9-engine-specific-integration-boundaries), [routing](#10-routing-waiting-draining-and-fairness) |
| Distribution and reliability | [Multi-node](#11-multi-node-groups-and-failure-coordination), [KV caches](#12-kv-cache-integration-and-data-lifetime), [protocol/recovery](#13-control-protocol-supervision-and-recovery) |
| Handoff | [CLI](#14-action-first-cli-and-interfaces), [defaults](#15-configuration-model-and-generated-defaults), [YAML](#16-illustrative-configuration-examples), [metrics](#17-observability-and-benchmark-evidence), [delivery](#18-delivery-sequence-and-module-boundaries), [decisions](#19-decision-register-and-review-gates), [tests](#20-acceptance-test-matrix), [sources](#21-revision-history-and-source-boundary) |

## 1. Purpose and agreed scope

CapyCTL makes a catalog of model deployments available on hardware that cannot keep all model weights resident simultaneously. A client chooses a public model ID. CapyCTL admits the request, activates the corresponding deployment when necessary, and routes inference to its engine group. Activation may restore a parked group or start a stopped one.

The project is a fresh, standalone open-source controller, not a fork of an existing proxy or a modification embedded inside an inference engine. Its value is correct lifecycle and residency management, not an assertion that no other project supports routing, unloading, or sleep helpers.

### 1.1 Established requirements

| ID | Requirement |
|---|---|
| R01 | Multi-engine architecture from the start: vLLM first, SGLang the next engine deliverable. |
| R02 | One platform-appropriate executable supplies the client CLI, server, host-agent, and standalone roles. |
| R03 | External stock or patched engines remain independent: native installations, virtual environments, approved scripts, and later supported container/service launchers. |
| R04 | Support colocated operation and a server on a different machine from the engines. |
| R05 | Use a lifecycle agent on every directly managed host; enable inference ingress only on API-facing members of an engine group. |
| R06 | Deploy a model as a deployment of one or more instances, placed by the scheduler from live capacity on an explicit allowed host set or selector, or pinned to one host, without manually launching any engine (ADR 0013). |
| R07 | Distinguish attachment to an existing service from ownership of its lifecycle. |
| R08 | Keep initialized engines parked where the declared residency tier can be delivered (ADR 0010, ADR 0011); otherwise use stop/start. |
| R09 | Keep model parking and KV-cache offloading separate but coordinate their resource ownership and compatibility. |
| R10 | Deployments request resources and select cache integrations; hosts enforce aggregate boundaries across all managed owners. |
| R11 | Use action-first commands: `capyctl <action> <resource>`. |
| R12 | `deploy` without `--wait` returns a durable deployment ID; status remains queryable after the CLI disconnects. |
| R13 | Generate safe default configuration when no implicit configuration exists; never replace an explicitly supplied missing or invalid file with defaults. |
| R14 | Preserve streaming, cancellation, fairness, whole-group ownership, and recovery correctness through every lifecycle transition. |

### 1.2 Boundaries and non-goals

CapyCTL owns routing, admission, deployment intent, reservations, lifecycle coordination, local supervision, and operational visibility. Engines own tokenization, kernels, batching, attention, tensor/pipeline distribution, and inference. Cache backends own KV serialization and block management.

The initial product does not install drivers, compile kernels, quantize checkpoints, download checkpoints implicitly ([ADR 0008](design/adr/0008-engine-installations-and-runtime-types.md) permits materializing an explicitly declared model source), implement tensor transport, build a new KV storage format, or provide cloud placement, billing, training, a desktop marketplace, or high-availability consensus. Other tools may call CapyCTL, but none is required to operate it. Ray and container runtimes are not mandatory CapyCTL dependencies; an explicitly selected engine recipe or launcher may have its own requirements.

llama-swap and NVIDIA PAIR are reference projects, not dependencies or the implementation base. Do not position CapyCTL merely as the first router with unloading or sleep. Any later integration must establish one lifecycle owner rather than letting two controllers manage the same engine.

Do not turn one initialized base-model engine into an arbitrary different architecture by swapping a model name. Separate runtime configurations are separate deployments. Adapter-specific LoRA support or compatible weight-update use cases are later features, not a substitute for this ownership model.

## 2. Vocabulary and durable objects

> **Amended by [ADR 0008](design/adr/0008-engine-installations-and-runtime-types.md).**
> Engine family, engine installation, runtime type and model source are the terms of
> record. "Runtime profile" below is the same object as an engine installation.
> "Model recipe" narrows to an internal frozen artifact derived from a deployment.

| Object | Meaning and identity |
|---|---|
| **Server** | Integrated controller, scheduler, management API, and inference router. One active controller in the initial scope. |
| **Host / agent** | User-facing resource `host`; agent is the local process implementing its control contract. Host identity survives reconnects and is not its display name or IP. |
| **Runtime profile** | Host-approved adapter, executable/launcher, environment, build fingerprint, and control constraints. Multiple profiles of the same engine may coexist. |
| **Model recipe** | Checkpoint identity, engine tuning, topology, resource estimates, and lifecycle/cache requirements reusable across deployments. |
| **Deployment** | Durable named declaration of model, engine installation and configuration, instance count, placement constraints, route and policy. Its deployment ID survives start, park, stop, count changes and controller restarts. |
| **Instance** | One engine group realizing a deployment, with its own generation, runtime binding, reservation, lifecycle and placement. A deployment declares its instance count; instances share its model, engine configuration, route and policy and differ only in placement and runtime identity (ADR 0013). |
| **Engine group** | The complete runtime realization of one instance, potentially multiple processes and hosts. |
| **Member / worker** | Host-local processes participating in the group; head and worker are assignment roles, not permanent host types. |
| **Resource domain** | A physically meaningful accounting unit: unified/system RAM, discrete device memory, filesystem capacity, or remote storage capacity. |
| **Exclusive pool** | The set of devices a group reserves exclusively while ready or activating, with shared host budgets checked separately. Overlapping sets conflict. |
| **Resource owner** | Unique account for a deployment allocation, retained private cache, or shared service. Physical allocations are charged once. |
| **Cache profile/service** | An engine integration plus private allocations, or an independently owned shared service and client quotas. |
| **Operation** | A durable lifecycle attempt for a deployment. Internal operation IDs support history and deduplication; normal users can work with the deployment ID. |
| **Generation** | Monotonic per-deployment identity; each instance activation draws a new value, so deployment and generation identify one instance incarnation. Used to reject stale commands and inference dispatch. Not evidence that an old process stopped. |

Separate administrative intent from observed state. A stopped deployment may be enabled for on-demand activation or explicitly suspended. Those are not the same condition.

## 3. Roles, process boundaries, and packaging

### 3.1 Responsibilities

| Component | Authority |
|---|---|
| Controller | Desired deployment state, activation decisions, group transitions, retries, ownership reconciliation. |
| Scheduler | Exclusive device ownership, aggregate budget reservations, queues, fairness, and reclaim decisions. |
| Router | Public model resolution, admission, waiting requests, backend dispatch, streaming and cancellation accounting. |
| Host agent | Approved launches, local worker identity, memory/storage observations, local safety checks, and adapter execution. |
| Engine adapter | Engine-specific inspection, readiness, quiescence, park/restore, and optional cancellation/cache barriers. |
| Launcher | Start and stop semantics for an owned process tree, container, or service. Independent of the engine adapter. |
| Host inference ingress | Narrow authenticated forwarding to the assigned engine; no independent model selection or global scheduling. |

The controller decides what should run. The agent may reject an unsafe or unauthorized transition. It MUST NOT independently evict another model or restart a distributed rank contrary to group policy.

### 3.2 Deployment modes

**Standalone:** `capyctl start standalone` runs the server and embedded host-agent components together. Use the same contracts and domain model, with in-process calls rather than enrollment and a network management stream. A separate local proxy hop is unnecessary if equivalent admission and endpoint isolation are preserved.

**Remote:** `capyctl start server` runs the server; `capyctl start host` runs one agent per managed host. The server may be GPU-less. An agent establishes an authenticated connection to the server and controls engines locally.

**Mixed:** the server may manage an embedded local host plus remote hosts. Client-only machines need the executable and credentials, not a running agent.

```text
Management: CLI -> server -> host-agent control contract -> engine/launcher
Inference:  client -> server router -> API-head ingress -> engine
Storage:    engine/cache connector <-> configured checkpoint or KV storage
Compute:    engine ranks <-> engine ranks, using the engine's own transport
```

Management RPCs MUST NOT become an implicit tunnel for prompts, token streams, weights, or KV payloads. Outbound agent connectivity does not establish router-to-ingress or engine-peer reachability; preflight those paths separately.

### 3.3 Implementation baseline

Recommend Rust throughout the long-lived CapyCTL components, with one executable per supported OS/architecture. This favors distribution and separation from engine environments; no unmeasured speedup over Python is asserted. Keep Python available for external launch helpers and integration tests, not as a required server runtime.

Recommended baseline: asynchronous I/O, a transactional embedded server store and local agent journal, and versioned gRPC with mutual TLS for remote control. SQLite is a reasonable initial store candidate. Record the language, storage, and transport decisions before scaffolding; do not introduce a second language/runtime or external broker without a specific justification.

Linux ARM64 and x86-64 are first validation targets. macOS roles and compatible Metal-backed engines require their own tests. A single binary is not one universal binary or a promise that no system libraries are used. Engine dependencies remain external. GPU telemetry support MUST be optional on server-only machines.

## 4. Enrollment and host preparation

### 4.1 Workflow

1. Initialize/start the server with an empty inventory.
2. An authenticated administrator creates a short-lived, single-use host invitation.
3. On the compute host, `join host` verifies server identity and enrolls using that invitation.
4. The host generates and retains its private key locally; issued credentials are scoped to its host identity.
5. `start host` connects using the saved identity, reports inventory and approved runtime profiles, and begins reconciliation.
6. The operator validates local preparations, then deploys models through the control-plane API.

The invitation carries the server address, trust pin or CA material, and bootstrap credential. Enrollment has a narrowly scoped server-authenticated bootstrap endpoint because a new host does not yet possess a client certificate. It MUST NOT bypass trust verification, use insecure TLS as a convenience, or require giving the host a general administrator token.

A valid invitation can preauthorize enrollment. Optional pending approval is a policy, not an additional mandatory manual host-inventory entry. Reusing or replaying an invitation is rejected, with idempotent recovery for the same enrollment transaction where securely identifiable. Secrets are not printed in normal logs or kept in role YAML.

Host-name collisions do not authorize replacing an existing identity. Certificates must be renewable and revocable; a revoked host must not continue accepting new commands through an old connection. Losing identity files requires explicit recovery/re-enrollment, not automatic adoption of a similarly named host.

> **Amended by [ADR 0016](design/adr/0016-revoked-host-recovery.md)** (owner decision 2026-09-24).

A revoked host, or one that lost its identity files, recovers by re-enrolling under its **same** host identity, never under a new one and never by adopting a name. An authenticated administrator issues an explicit recovery invitation for the revoked host (`invite host <name|id> --recover`); it is single-use, short-lived, bound to that host id and journaled, and it is refused for a host that is not revoked. The host redeems it explicitly (`join host --recover`), keeping its state and journal, or starting from fresh identity files if they were lost, and always with a new key. It receives a new certificate bound to the same host id. Revocation is per certificate: the old certificate stays revoked for ever. Name-collision rules for new hosts are unchanged. On reconnect the host is reconciled as after any session loss (§13.2): a Ready engine it still owns is re-proven by a fresh probe against its recorded process identities before dispatch reopens; anything unproven stays closed and charged; pending stops and drains complete through the ordinary path. A host whose journal was lost cannot re-prove its engines: they stay uncertain and charged until an operator stop settles them on gone evidence, which the host reports by observing the server's recorded process identities without signalling anything it does not own. Accounting is never released without evidence.

### 4.2 Preparation is not deployment

A host may enroll before engines or checkpoints are installed. Track independently: enrolled identity, current connectivity, profile eligibility, and deployment readiness. Online does not mean eligible for every recipe.

Host administrators register trusted runtime profiles, permitted devices and directories. The initial release does not silently install engines, execute discovered scripts, or download weights. `doctor host` performs approved non-destructive checks; destructive park/restore verification is an explicit operation.

> **Amended by [ADR 0018](design/adr/0018-engine-registration.md)** (owner decision 2026-09-25).

Host administrators register runtime profiles with `capyctl engine detect`, `add`, `list` and `remove`, the same on a host and in standalone. Registered profiles live in `engines.yaml` beside the role's configuration file and are merged with it at load; CapyCTL never rewrites the role document. Detection reads package metadata only and executes nothing; an installation is executed (bounded version check, installation fingerprint, deep-park probe) only after the operator names or picks it. A registered profile is published on the live control session without restarting the role (capability `live_profile_update`); the server validates it like a startup publication and keeps the previous approved snapshot when it refuses one. A published profile is removed only after the server confirms, in two phases, that no deployment on that host uses it, stopping them through the ordinary stop path when asked and never confirming without stop evidence. A deploy naming a profile no allowed host publishes is refused at once (`profile_not_published`). CapyCTL still installs no engine.

The agent initiates its management connection; the server sends commands over that session. gRPC supports bidirectional streaming and TLS client authentication as protocol building blocks [S8, S9]. Retry with backoff; reconnect after reboot without creating another host record. First-time setup is server-first, but steady-state boot order is not constrained.

### 4.3 Foreground roles and service operation

Role-start commands stay in the foreground. Provide normal OS service definitions rather than custom daemonization. Server and agent shutdown modes must distinguish ordinary service restart from explicit draining/termination of deployments. A role restart must not silently delete deployments. Reconciliation after restart is required even if an OS service manager also supervises the agent.

## 5. Managed deployments, attachment, and adoption

### 5.1 Managed

The agent launches an approved runtime profile and obtains a verifiable ownership handle. CapyCTL may drain, park, restore, stop, and recover that group under policy. Engine workers and explicitly managed cache services have separate ownership records when their lifetimes differ.

### 5.2 Attached

Attachment registers and validates an already-running inference service for routing and observation. It grants no implicit permission to sleep, kill, replace weights, restart, or evict resources. An upstream disappearing is not proof that its allocations disappeared.

If the service shares a managed host, its resource usage must be represented conservatively. Uncertain attached usage cannot be treated as reclaimable capacity. Direct external clients may bypass CapyCTL's in-flight counts; no drain-based lifecycle operation is authorized without exclusive admission control or a validated external quiescence contract.

An externally managed endpoint may use no CapyCTL agent, but restart guarantees are unavailable unless an equivalent supervisor integration is explicitly configured. Mark that limitation in status.

### 5.3 Adoption

Adoption is a separate authorized transfer of ownership, not an attachment flag that guesses a PID. Require complete worker/service identity, launch recipe, checkpoint/build identity, resource allocation, existing-supervisor handoff, and admission authority. The initial safe path is controlled drain and recreation under an agent. Do not steal ownership from another supervisor.

## 6. Deployment lifecycle and durable intent

### 6.1 Runtime states

| State | Meaning | Admission |
|---|---|---|
| STOPPED | No owned engine workers remain; cache/storage allocations may still exist. | Queue only if enabled/on-demand. |
| STARTING | Cold initialization is in progress and resources are reserved. | Queue. |
| READY | Whole-group model readiness and allocation checks pass. | Admit within limits. |
| DRAINING | New admission is closed; accepted work is completing. | Queue. |
| PARKING | Selected release protocol is running. | Queue. |
| PARKED | Initialized runtime retained; release conditions verified; residual budgets retained. | Queue/restore if enabled. |
| WAKING | Allocations and model contents are being restored. | Queue. |
| STOPPING | Owned engine workers are being terminated and accounted for. | Queue or reject according to desired state. |
| RECONCILING | Ownership, generation, runtime state, or resources are being established. | Closed. |
| FAILED | Recovery required; resources may still be occupied. | Closed except bounded recovery wait. |

`FAILED`, loss of connectivity, or expiration of a command lease MUST NOT release a resource reservation without evidence. Liveness of an HTTP server is not model readiness.

Normal transitions:

```text
STOPPED -> STARTING -> READY
READY -> DRAINING -> PARKING -> PARKED
PARKED -> WAKING -> READY
READY -> DRAINING -> STOPPING -> STOPPED
PARKED -> STOPPING -> STOPPED
Any uncertain state -> RECONCILING -> verified state or FAILED
```

### 6.2 Residency policies

**Deep park:** preserve initialized engine state while releasing inactive weight contents; restore from the checkpoint. This is not process hibernation and does not create a model snapshot on disk.

**Host-backed park:** retain a weight backup in host RAM. Optional later policy with explicit accounting; never silently substituted for deep parking.

**Restart-only:** stop and initialize again. This is first-class supported behavior, including backends without a verified memory-release API.

A deployment declares exactly one residency — `restart_only`, `host_backed`, or `deep` — and there is no runtime ladder between them: the deployment states the tier it wants, and resolution either confirms the profile and host can deliver it or fails closed. `auto`, which formerly selected a tier at run time, is withdrawn (ADR 0010): a running SGLang engine cannot switch tiers, since its park flags are startup-only, and on the hardware in hand the choice is forced by the host's declared memory topology before launch anyway, so a runtime ladder would have nothing to choose between. `deep_required` is likewise withdrawn, because a declared `deep` residency already fails validation when deep parking is unavailable, by construction. `restart_only` prohibits sleep calls. Live verification of a tier does not override an operator's security restrictions. `host_backed` is refused at configuration time, not at first park, on a host domain declared to have device and host memory as one physical pool, since retaining a weight backup there would free nothing.

> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).

`host_backed` is supported on hosts whose device memory is distinct from host RAM; it is the default there. A deployment that states no residency gets `host_backed` on a discrete-GPU host when its weights copy plus the engine's host overhead fits the system domain's parked room, `deep` when it does not or while the weights are unknown, `deep` on a unified host, and `restart_only` when its profile opted out of deep parking. The copy is charged on the system domain and bounded by its `parked_limit`; a switch victim whose copy would not fit is stopped, never parked `deep` in its place. A `host_backed` launch on a build without parking support is refused `capability_missing:deep_park`.

### 6.3 Command semantics

| Action | Required semantics |
|---|---|
| deploy model | Create a durable deployment. Default activation follows policy; `--activate` requests an initial activation. Updating an existing deployment requires an explicit revision-aware operation. |
| start deployment | Enable it and request one transition to READY. Restore if parked, initialize if stopped. Not a permanent pin against future swaps. |
| preinitialize deployment | Sequentially start, validate, and park; park at the declared tier (ADR 0010). Never evict live user work just to preinitialize. |
| park deployment | Drain and park at the declared tier. Leave eligible for later on-demand activation; fail clearly if explicit parking is unsupported. |
| stop deployment | Suspend automatic activation, drain, and terminate engine workers. Preserve deployment identity and configured persistent storage. |
| delete deployment | Remove route and deployment after authorized cleanup. Refuse while any instance still holds a runtime, reservation, lease, or open operation. `--stop` first stops every instance, waits for verified cleanup, then deletes; when cleanup cannot be proven yet it reports the operation as pending, leaves the deployment intact, and resumes on a retry with the same request identity. Do not delete user-owned checkpoints or cache files implicitly. The name becomes free for a new deployment with a new ID. (Owner decision 2026-09-23 replaced `undeploy model`.) |
| attach model | Register an existing service under attached ownership semantics. |

Automatic idle stop differs from administrative stop: idle eviction leaves the deployment eligible for on-demand activation. Required explicit stop behavior MUST NOT be undone by the next inference request.

### 6.4 Durable acceptance and status

Persist the deployment record and accepted operation transactionally before returning its ID. Default CLI stdout contains the deployment ID; diagnostics use stderr; structured output may include acceptance state. No `--wait` means submission returns after durable acceptance, not after readiness.

`--wait` observes the same operation. CLI exit or watch timeout does not cancel it. Status exposes desired state, runtime state, latest operation and error, participants, reservation state, and conditions such as blocked or degraded. An on-demand STOPPED deployment can be a successful accepted deployment, not a failed startup.

Use idempotency keys for retries and revision/generation preconditions for mutations. A response lost after persistence must not produce another deployment on retry. Native engine calls are not assumed idempotent: reconcile ambiguous effects instead of blindly repeating a sleep/reload collective. A deployment ID is stable; operations and generations are distinct internal identities.

### 6.5 Idle policy, pre-initialization, and bounded parked sets

The controller owns idle policy; agents must not independently race incoming dispatch with a local sleep timer. After a ready-idle timeout, attempt the selected automatic parking policy or stop under restart-only fallback. After a parked-idle timeout, stop the engine to reclaim its residual state. These idle transitions leave automatic activation enabled.

F2 adds an explicit warm-residency commitment: retain initialized runtimes while
parked, allowing weight rereads during wake. While that commitment is selected,
neither idle expiry nor capacity reclamation may silently stop a retained runtime.
An infeasible warm arrangement queues within its deadline or fails with a capacity
diagnostic; stopping requires explicit policy that relinquishes warm residency or
an authorized administrative/recovery action. Budget every cold, park, and wake
transition, including already parked owners, before increasing resource use.

Pre-initialize only an explicit selected set, sequentially under normal reservations: initialize, verify, park, then continue. Defer this work while it would displace active requests. A ready-idle timer does not reset a fairness window already opened for a waiting model.

Enforce both the maximum parked count and aggregate residual budgets. Reclaim least-recently-used eligible parked groups first, coordinating any retained-cache owners separately. Keeping a deployment parked does not exempt it from host limits. An explicit pre-initialize command must fail rather than claim a restart-only deployment is prewarmed.

## 7. Resource contracts and aggregate boundaries

### 7.1 Ownership split

Host policy defines approved inventory, total managed ceilings, category sub-limits, system headroom, storage pools, and local enforcement capabilities. Deployment manifests define requested devices, phase-specific allocations, cache integration and quotas. Shared cache services have their own capacity records and client quotas. The control plane plans reservations; agents enforce local admission.

Deployments must not claim all host resources merely because a runtime's defaults do. Host YAML must not assign one cache amount to every vLLM/SGLang process.

### 7.2 Physical domains, categories, and bounds

Map allocations to physical domains before totaling them. On a unified-memory host, CPU and accelerator allocations may share one domain. On a discrete-GPU host, system RAM and each device's VRAM are distinct. NVIDIA describes DGX Spark's 128 GB as unified system memory [S4]; this is why a CPU weight copy is not extra capacity on the Sparks.

A host-wide managed ceiling includes weights, active KV, private offload buffers, workspaces, parked residue, engine processes, and separately managed service allocations. Host-KV and parked limits are sub-limits, not additional capacity. Category sets may overlap; the physical total uses the union of uniquely owned allocations, never a naive sum of every label or metric.

Track system/agent/router overhead and external workloads through the configured safety reserve and live pressure measurements. Available-memory counters and RSS can overlap or include reclaimable page cache; do not invent precise free capacity from incompatible measurements. Unknown topology or unreconciled usage closes unsafe admission.

> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).

On a discrete-GPU host each GPU's memory is a `device` domain observed from the device; host RAM is a `distinct` system domain. A deployment's derived budget charges both. A device domain without a fresh observation closes admission on that domain (`device_unobserved`) and keeps every reservation charged. One GPU serves one model; CapyCTL picks the GPU unless the deployment pins one.

### 7.3 Reservation lifecycle

| Owner/state | What stays charged |
|---|---|
| Catalog-only or stopped engine | No engine RAM reservation after verified cleanup; surviving cache services and persisted storage remain charged. |
| Activating group | Full per-host activation peak, including staging and temporary allocations. |
| Ready group | Serving reservation, including its provisioned private cache budgets. |
| Parked group | Verified retained footprint plus conservative residual reservation; retained host-cache allocations remain charged. |
| Independent cache service | Its own allocation once, regardless of how many deployments use it. |
| Persistent namespace | Allocated quota/reservation or actual usage under a defined storage policy until safely reclaimed. |

For each domain, require all retained owner reservations plus the candidate's activation reservation to fit the effective managed ceiling. Do not double count the candidate's own existing parked reservation when replacing it with an activation reservation; validate the transition's true peak. Other parked engines and shared services remain included.

Use peak reservations conservatively. Sampled usage below the reservation does not authorize unsafe overcommit. Observed usage above a reservation triggers a blocked/degraded condition and recovery; it is not hidden by the ledger. Device exclusivity and aggregate budgets are separate constraints: disjoint GPU pools on one host still compete for host RAM and disk.

F2 also supports explicitly shared GPU assignments: independently managed vLLM and
SGLang deployments may serve concurrently on the same device when all assignments
permit sharing and their transition/serving budgets fit. Exclusive assignments still
conflict with any other owner's overlapping assignment. Capacity checks alone do not
override exclusivity, and shared execution does not promise performance isolation.

### 7.4 Cache example and shared services

If host-KV capacity is 16 GiB, A retains 9 GiB, and B requests 8 GiB, B is blocked: 17 GiB exceeds the boundary. First reclaim A through a supported cache operation, stop an eligible owner, or wait. Do not assume parking released A's private host cache. Do not shrink B's requested allocation silently.

If A and B use the same 16 GiB shared cache service, charge the service's physical allocation once. Client quotas partition its usable capacity; they are not a second physical charge. Account for the service's own metadata/overhead separately from usable cache bytes where necessary. LMCache documents an isolated eviction policy with per-namespace quotas [S7]; CapyCTL must verify the selected backend's actual quota behavior rather than infer it from a connector name.

Remote stores require one shared resource identity and quota authority. Each host must not independently assume ownership of the store's full capacity. Separate logical pools on the same filesystem also share a real free-space boundary.

### 7.5 Enforcement and reclamation

Distinguish admission enforcement, backend limits, supported OS/filesystem controls, and observation/recovery. A YAML value is not automatically a hard GPU-memory cap. Expose the enforcement level and verification status of each resource contract.

Adapters translate granted budgets to engine/cache settings with explicit units and scope. vLLM documents per-instance GPU utilization, per-GPU explicit KV bytes, and TP-group-wide offloading size [S5]. Normalize these before rendering commands. A group budget must not be multiplied accidentally by assigning it in full to every rank.

Prefer fixed, verified allocations initially. Runtime resizing is supported only where a backend exposes safe semantics. Reclaim through backend eviction, release, or owned-service shutdown; never delete live store internals blindly. Reclamation completes only after verified release. Required caches block activation on failure; optional caches may use a separately validated cache-disabled path or recompute safely.

## 8. Runtime profiles and the engine/launcher contract

A host may approve several profiles: stock vLLM, one or more patched vLLM builds, and SGLang. The deployment names a profile; the host resolves its local command. Global profile names do not prove matching builds across hosts.

### 8.1 Launch contract

A managed profile supplies an executable and argument array, controlled environment, working directory, permissions, ownership mechanism, endpoint discovery, and stop semantics. No implicit shell interpolation. Administrator-authored scripts are trusted execution, not a sandbox.

**Native-compatible:** append the engine-native argument tail to the configured launch prefix. A wrapper replaces itself with the engine or remains foregrounded, forwards signals, waits, and propagates exit status.

**Custom/fixed recipe:** an explicit mapping supplies model/rank/port parameters through approved arguments or a file. Hidden fixed settings are declared and validated. Such a script is not advertised as arbitrarily configurable.

A process that backgrounds children and exits without a durable ownership handle is invalid. Container/service launchers must track actual container/service identity; killing a CLI wrapper is not complete cleanup. A host script must not secretly launch remote ranks beyond the participating agents' ownership.

Engine initialization may allocate substantial memory before readiness. Reservations and private endpoint settings must be in place before launch. Runtime executables, material scripts/configuration, environment identities, and checkpoints are fingerprinted; updates do not silently mutate active deployments or reuse a superseded binding identity. CapyCTL validates a recipe's shape and the host's capacity to hold it; whether the recipe works is the user's responsibility (ADR 0011).

Custom and patched engine builds are first-class (ADR 0008, owner decision 2026-09-23). An engine installation is fingerprinted when the host registers it: the engine package's version and a `sha256:` digest over its files. A launch that measures a different fingerprint flags the drift in the host's status and the event journal, and is refused (`installation_drift`) only when the installation's host policy says `installation_drift: refuse`; the default is `warn`. CapyCTL MUST NOT compare installation files to hard-coded hashes and applies no permission rule to them. The engine internals CapyCTL hooks are probed by shape at launch (import, attribute or signature presence, record fields, served routes); a missing capability refuses only the feature that depends on it with a closed `capability_missing:<name>` reason, and serving without that feature stays available.

### 8.2 Parameter ownership

Deployment recipes contain engine tuning; host profiles contain launch context and fixed local constraints. The adapter constructs one effective command, not a blind concatenation of conflicting flags.

CapyCTL controls or validates device assignment, process ownership, bind addresses, private ports, public/upstream model mapping, distributed ranks, rendezvous data, granted memory/cache settings, and required lifecycle prerequisites. Reject conflicting overrides, duplicate reserved flags, or hidden configuration-file values. Unknown ordinary engine arguments may be passed through subject to operator policy; security-sensitive code-loading or path options are not unrestricted inference-client inputs.

Preserve the original engine's supported arguments where possible. Do not place every new kernel flag into the generic control-plane schema. `inspect deployment --effective-config` exposes the resolved command and provenance with secrets redacted.

A deployment's `engine_config` carries typed common parameters per engine family and, only when the deployment sets `accept_extra_args: true` and host policy allows it, ordinary engine arguments passed through unchanged. Reserved settings are refused at deployment and verified again after the engine's own parser resolves them. Code-loading, path, listener and egress options require the host installation to approve them by name. *Amended by ADR 0014 Amendment A3 (owner decision 2026-09-25):* vLLM `--speculative-config` is approved by name and then admitted key by key: a closed key list, scalar values, and a draft model inside the approved paths. A checkpoint is identified by a content digest recorded when the deployment is accepted and re-verified before every launch and wake (ADR 0014).

### 8.3 Adapter operations

| Operation | Contract |
|---|---|
| Inspect | Report identity, observed engine state, advertised control capabilities, and available observations. |
| Render/validate plan | Resolve native parameters, topology, budgets, endpoints, and profile prerequisites without starting work. |
| Check readiness | Establish model-specific usability, not only liveness. |
| Prepare to park | Establish quiescence and supported cache-write coordination. |
| Park | Execute the selected release strategy and report observed outcomes. |
| Restore | Restore all required components and allocation classes before readiness. |
| Observe/cancel work | Provide explicit acknowledgement semantics where supported; otherwise report uncertainty. |

Launch, terminate, owned-handle inspection, and exit reporting belong to the launcher. Reservation policy, retries, fallback, timeouts, and fairness belong to the controller. An adapter MUST NOT secretly start a competing process or acquire its own conflicting pool reservation.

### 8.4 Withdrawn

Capability qualification was withdrawn by ADR 0011. The section number is kept so
that references in §20 remain stable. Recipe ownership is stated in §8.1.

## 9. Engine-specific integration boundaries

### 9.1 vLLM first

Current vLLM documentation distinguishes level 1, which keeps a CPU weight backup, from level 2, which discards weights and KV while retaining some buffers. The documented online deep-sleep path is `POST /sleep?level=2`; restoration wakes weight allocations, invokes `reload_weights` through the collective RPC interface, then wakes KV allocations. Online controls require startup configuration including sleep support and development mode [S1].

The vLLM adapter wraps this in CapyCTL admission and resource checks. Waking allocations alone is not successful restoration. A functioning plain restart-only deployment is the baseline before enabling this optimization.

**Security gate:** vLLM's security documentation warns against enabling development mode in production and identifies the collective RPC surface as dangerous [S2]. The initial deep-parking path is an explicitly authorized, isolated experimental integration, not a production-safe claim. It is enabled unless host policy forbids it. Private binding and a narrow ingress are necessary controls but do not erase that upstream warning. Production readiness requires a separately reviewed supported control path or appropriate engine changes. Owner decision 2026-09-17, reaffirmed 2026-09-22 (ADR 0012): deep parking is enabled by default and a host opts out with `security.deep_park: disabled` on the runtime profile; standalone opts out with `CAPYCTL_DEEP_PARK=off`. Default enablement is not a production-safety claim: development controls stay on loopback behind the per-launch key guard, are never reachable through host ingress or the router, and status marks every profile that uses them.

### 9.2 SGLang immediately next

SGLang documents memory-saver startup support, release/resume APIs, no ongoing requests before release, and disk-based weight updates. Releasing KV invalidates live cache contents [S3]. Verify its actual release, retained-copy, and restoration behavior on authorized hardware rather than assigning vLLM level numbers to it.

Use the same controller, launcher, resource contracts, queues, and conformance tests. SGLang is not postponed behind a dashboard, plugin marketplace, or a general cluster scheduler. Restart-only support is valid when the declared tier is `restart_only`. A build whose launch-time probe lacks the memory saver hooks or the release, resume and reload routes still serves `restart_only`; a `deep` launch or a Park on it is refused with `capability_missing:deep_park` and the launch left unchanged (§8.1).

Restart-only is not sufficient to close F2: the owner requires parking and
restoration verified live on authorized hardware for a selected recipe of each engine, sequential preinitialization,
concurrent serving when capacity permits, and repeated pressure-driven warm switching.
Per-deployment ports, credentials, resource accounting, and management API/CLI wiring
are F2 prerequisites. Unknown work is not proof of safe quiescence for parking; an
uncertain drain must reconcile or fail within its bound. The approved
[F2 design](design/milestones/f2-sglang-design.md) defines these requirements and the
snapshot/event contracts for a future UI; UI implementation remains later work.

### 9.3 Later backends

A compatible Metal or other server can begin with an approved restart-only profile. Hardware branding does not permanently determine its policy. The agent need not import the engine's runtime libraries simply to control an external process.

## 10. Routing, waiting, draining, and fairness

The first inference surface is `GET /v1/models` and streaming/non-streaming `POST /v1/chat/completions`. List configured enabled public IDs without waking them. Live conditions belong in management status. Do not claim full API equivalence for embeddings, Responses, native endpoints, or other surfaces without contracts and tests.

Resolve model aliases to explicit deployments. A deployment with several instances is served by all of its READY instances; the router selects among instances with open admission using its own in-flight counts and fresh host-reported engine load, and fails over to another instance only before upstream acceptance (ADR 0013). Preserve supported payloads, tool calls, structured-output parameters, reasoning fields, multimodal content, and stream events. Do not tokenize, rewrite prompts, silently substitute models, or execute client tools. Any model-name remapping in responses must be documented and limited.

Note (2026-09-24): CapyCTL relays tool calls but does not parse them; the engine does. A deployment serves `tool_choice: auto` (and SGLang any tool call) only when its engine is launched with its tool parser through `engine_config.extra_args` with `accept_extra_args: true` (vLLM `--enable-auto-tool-choice --tool-call-parser <name>`, SGLang `--tool-call-parser <name>`). Without one, the engine rejects the request or answers in plain text, and CapyCTL relays that answer. An engine's complete invalid-request answer (HTTP 400, 413 or 422 with a JSON body) is completion evidence: the client receives the engine's status and message as `engine_rejected` and the request's lease closes. Every other engine error status stays uncertain (owner decision, 2026-09-24).

Note (owner decision 2026-09-25): a request for a deployment an operator stopped (`stop deployment`, or `stop instance` on every instance) is refused at once with HTTP 409 and code `deployment_stopped`; the message says an operator stopped it and names `capyctl start deployment <id>`. It is not queued and is not a capacity refusal: `insufficient_resources` remains for admission blocked by capacity. The error body keeps the router's shape.

Eviction is planned per host and per instance: B is served by an existing READY instance when one exists; otherwise the planner releases capacity only on the one host that will run B's instance, preferring instances whose deployment keeps serving elsewhere. Steps 3–5 below apply in full when the victim is its deployment's last READY instance (ADR 0013).

A request for B while A owns the pool follows:

1. Authenticate, validate the route and bounds, then enqueue B with a deadline.
2. Join one activation operation for B; simultaneous requests do not create duplicate wakes.
3. Close new admission to A according to fairness policy.
4. Drain accepted work at the router and ingress, then confirm engine quiescence through the adapter.
5. Park or stop A and verify release on all participating hosts.
6. Reserve the complete activation plan for B and launch/restore it.
7. Verify whole-group readiness and open the new generation's admission gate.
8. Dispatch and stream the queued requests with in-flight accounting until completion or confirmed cancellation.

The host ingress authenticates the router, permits only inference paths, validates the assignment generation, and refuses late/stale dispatch after its gate closes. It does not choose the model or maintain a second independent global scheduler. Engine management and arbitrary proxy paths are never exposed through it.

Keep the current model ready across short tool-call gaps. Once another group waits, use a bounded, non-resetting admission window, then drain; select the oldest waiting group and preserve per-group ordering. Effective waiting bounds also depend on explicit inference/drain deadlines. No permanent session pinning or prompt-based guessing of conversation boundaries.

Bound queues by requests and buffered bytes. Limit body size and memory retained by multimodal payloads. Do not buffer entire streaming responses. A waiting request gets no fake inference tokens or invented successful response. Client deadlines must include activation when appropriate.

Normal switches do not kill live requests. A drain timeout fails the switch by default; force termination is separately authorized. Client disconnect is not proof the engine stopped working. Retain conservative accounting until cancellation acknowledgement, completion observation, or controlled cleanup. Never replay partially streamed inference or silently retry after uncertain backend acceptance.

Management operations are durable; queued inference bodies and open streams are not promised to survive a server crash. Requests may fail during a router/server outage. Do not imply that a durable deployment ID makes inference exactly-once or resumable.

## 11. Multi-node groups and failure coordination

Use an agent on every host directly supervised by CapyCTL. Only the API-facing group member needs ingress. The group may expose one inference endpoint while owning workers on both Sparks. vLLM's documented native multi-node topology includes an API-facing node and headless workers [S10]. CapyCTL manages the deployment; the engine implements distributed inference.

Worker agents exist to start missing processes, observe exits, account for local resources, reconcile orphans, and clean up after head failure. They are not inference routers or tensor relays. A head-only integration is allowed only when an explicitly supported external supervisor provides equivalent remote ownership guarantees; it is not the default bare-metal topology.

A group launch plan specifies member identities, exact runtime fingerprints, devices, model paths, local ports, private peer addresses, rank roles, and rendezvous data. Validate all hosts before evicting current work where possible. Coordinate required rank startup; do not wait for the head to become fully ready before starting workers it needs.

Reserve all members before launching. Concurrent group plans must not deadlock through partial device acquisition. Failed launches use compensating cleanup: do not claim a cross-host atomic transaction, and do not report release until all relevant owners are accounted for.

A designated lead agent invokes each engine-level collective operation through the appropriate control endpoint. Other agents perform local supervision and report evidence; do not issue the same collective independently per rank. Readiness, parking, and restoration cover all required workers and drafters.

If one member fails, coordinate recovery according to the engine recipe; otherwise restart the full group. Autonomous rank replacement is not assumed safe. A disconnected worker can still be consuming memory; lease expiry never makes it free. Keep reservations quarantined until reconnection/reconciliation or explicit fencing establishes cleanup.

## 12. KV-cache integration and data lifetime

Model parking reduces inactive weight residency. Persistent KV caching can preserve reusable computation. Neither promises that every live block, active attention state, or entire conversation survives a switch.

vLLM documents hierarchical offloading connectors and filesystem storage [S6]. SGLang HiCache documents private per-instance device/host tiers and an external sharing tier [S11]. Cache selection is per deployment and exact build. A path called a cache pool does not enable or convert an engine integration.

CapyCTL retains cache configuration and namespaces, accounts for all owners, starts/stops explicitly owned cache services, and coordinates supported persistence barriers. Engines/connectors serialize, retrieve, index, and evict blocks. Cross-engine KV conversion is outside scope.

Namespace isolation includes model/checkpoint revision, engine/layout, quantization, tokenizer/template effects, parallelism/rank, and security domain. Conservatively isolate by deployment until reuse is verified. Public model aliases must not collapse incompatible caches. Secret-bearing prompts and KV are sensitive even when text is not logged.

Persistent local caches need per-writer quotas and a filesystem-wide free-space reserve. Retention after stop/delete is explicit and visible; no silent data deletion. Shared services need reference counts or equivalent ownership protections so parking one client does not stop another client's cache.

A `reclaimable` flag authorizes a policy, not arbitrary deletion. Invoke validated eviction or stop an owned cache process, then verify its releases. If a backend exposes no persistence barrier, report best-effort retention. Cache misses/corruption cause safe misses or failure according to policy, never reuse of invalid state.

## 13. Control protocol, supervision, and recovery

### 13.1 Remote management contract

Suggested operation families: enroll, connect/report inventory, prepare local plan, launch local member, close/open ingress gate, inspect, execute designated group control, terminate owned member, cancel supported work, and report/replay operation results. These are conceptual contracts, not frozen protobuf messages.

Commands carry host/deployment identity, assignment generation, operation ID, profile fingerprint, expected state, deadline, and permitted resource plan. Agents persist accepted operation state and relevant ownership before acknowledging effects. At-least-once delivery must not lead to duplicate launches; replay known results and reconcile ambiguous in-progress actions. Results include evidence and errors, not just a boolean success.

The agent enforces monotonic assignments, authorized server identity, and one local resource registry lock. It rejects stale commands, incompatible protocol/schema versions, and unapproved profiles. Reconnection carries inventory and resumable operation history with bounded retention; do not place inference bodies in that journal.

> **Amended by [ADR 0017](design/adr/0017-version-skew-and-capability-gating.md)** (owner decision 2026-09-24).

Release compatibility within the session protocol is a SemVer skew policy. The host reports its release version on connect (strict SemVer; a missing or unparseable version counts as incompatible). Patch, pre-release and build-metadata differences on the server's `major.minor` line are fully compatible. A host one minor release behind the server, on the same major, is supported, with an upgrade recommended. A host older than that, or on another major, is connected **drain-only**: the server may still stop, drain, revoke, terminate, probe and inspect what it owns there, and MUST NOT place, start, wake, park, digest or materialize anything on it; status and host listings show `upgrade_required` with the reason. A host newer than the server (any minor or major ahead) MUST be refused with a clear "upgrade the server first"; the host logs it and reconnects with backoff. Upgrade order is the server first, then hosts one at a time. For 0.x as for later releases, a change affecting the protocol or durable state ships only in a minor or major release; a patch release never changes the protocol.

Every protocol feature added after the protocol version 2 baseline is a named capability the host declares on connect. The server MUST NOT send a field or action a host did not declare; it refuses that operation for that host, before anything is sent, with the typed reason `host_capability_missing:<name>` (or `host_upgrade_required` for a drain-only host), and placement excludes a host lacking a capability every launch needs. Absent additive fields encode exactly as before, so journaled command digests stay verifiable.

### 13.2 Process and controller failures

Track PID plus start identity, process-tree/service/container handles, deployment generation, and ownership record. Never kill by executable name or adopt whatever occupies a port. Detect PID reuse. Foreground wrappers and process groups are a baseline; complete isolation/cleanup capabilities must be explicit for the chosen platform.

On server restart, reconcile desired state with agents before dispatch. On agent restart, inspect owned handles, verify current generations, and reconcile resources before accepting new transitions. On control-channel loss, preserve ownership, freeze new unsupervised transitions, and let existing work finish where the serving path remains available. Server/router failure may still break streams.

On wake failure, keep admission closed. A bounded clean-restart fallback is allowed after verified cleanup and before inference dispatch. Repeated failures exhaust the deployment's attempt budget and leave it terminal `Failed` with its own admission closed (ADR 0011 decision 5). On park failure, safely stop the owned group after drain if policy allows. Unexpected memory growth blocks new admission; no blind activation of the next model.

Controller command generations are not a substitute for physical fencing. Initial scope is one active controller with persistent state and local locking, not active-active failover. An agent cannot switch to another controller identity just because it has a similar hostname.

### 13.3 Local authority and security

Run agents with the least privilege needed for their approved processes. Arbitrary privileged shell execution, automatic container-socket exposure, or unrestricted path access is not part of the management API. Host profiles authorize execution; deployment operators may only select them and use permitted overrides. Inference clients have neither permission.

Allowlist normalized methods and paths, strip/replace internal routing headers, verify trusted upstream destinations, bound resource-amplifying requests, and prevent public access to administrative RPCs. Remote control and ingress use distinct authenticated identities/roles; do not pass end-user API secrets to unrelated upstream services. Local-only does not mean unauthenticated by default.

Protect credentials, journals, checkpoint permissions, and sensitive cache directories. CapyCTL's own runtime helper files (the runtime directory, its modules and the protected entry) and the directories on the way to them MUST be owned by root or the service user, never writable by other, and writable by group only through the owning user's private group; a group whose membership cannot be established is refused (owner decisions 2026-09-22 and 2026-09-23). CapyCTL's private state (identity, credentials, lock files, observation sockets) admits no group write at all. Engine installation files are governed by §8.1, not by this rule. *Amended 2026-09-25 (owner decision):* an engine's environment is closed. Its PATH is the installation's own `bin`, then fixed system directories, never the caller's shell PATH. A runtime profile may name a host-approved CUDA toolkit root, `cuda_home`, approved like `executable`. The host administrator writes it, or `capyctl engine add` detects it from `CUDA_HOME`, else from `/usr/local/cuda` when it holds `bin/nvcc`; standalone environment installations take `CAPYCTL_CUDA_HOME`. When `cuda_home` is set, CapyCTL puts `<cuda_home>/bin` right after the installation's `bin` and sets `CUDA_HOME`; without it the PATH stays minimal. CapyCTL also bounds JIT compile parallelism in the engine environment. It sets `MAX_JOBS` to `clamp(floor(MemAvailable at launch / 8 GiB), 1, CPU count)` and `FLASHINFER_NVCC_THREADS` to 1. A profile's `env` may override either one with a positive integer, and the host log records the chosen value at every launch. Do not log prompts by default. Record lifecycle commands and failures with secrets redacted. An explicit local development flag, `start standalone --debug-engine-logs`, may retain full native engine output in private owner-only log files. It defaults off, is not persisted, and does not relax launch or plugin checks. Raw development logs may contain secrets and MUST NOT be included in management responses or lifecycle journals. Revocation closes control sessions and prevents new work; terminating existing workloads on revocation follows explicit administrative policy.

> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).

The inference listener may be reachable from the network. It requires the API key by default; turning the key off (`authentication: none`, `--no-inference-auth` or `CAPYCTL_INFERENCE_AUTH=none`) prints a warning at start when the bind is not loopback, and there is no constant or fallback key. The router's allowlists, header stripping and amplification bounds apply on every bind. The management listener, engine listeners and the key-guard protections keep their loopback rules.

## 14. Action-first CLI and interfaces

Canonical grammar: `capyctl <action> <resource> [identifier] [options]`.

```bash
# Role startup is local and foregrounded.
capyctl start server --config server.yaml
capyctl start host --config host.yaml
capyctl start standalone --config standalone.yaml

# Initialize / enroll.
capyctl init server --output server.yaml
capyctl init host --output host.yaml
capyctl invite host --name host-a --output host-a.join
capyctl join host --join-file host-a.join --config host.yaml

# Inventory and preparation.
capyctl list hosts
capyctl inspect host host-a
capyctl doctor host host-a

# Submit and observe deployment intent.
capyctl deploy model --file deployment.yaml
capyctl deploy model --file deployment.yaml --activate --wait
capyctl status deployment dep_example
capyctl status deployment dep_example --watch
capyctl inspect deployment dep_example --effective-config

# Lifecycle actions go through the same control-plane rules.
capyctl start deployment dep_example
capyctl park deployment dep_example
capyctl stop deployment dep_example
capyctl preinitialize deployment dep_example
capyctl delete deployment dep_example
capyctl delete deployment dep_example --stop

# Configuration operations.
capyctl validate config --file host.yaml
capyctl inspect config --role host --effective

# Reclaim materialized model sources no deployment references (ADR 0008).
capyctl prune sources --host-config host.yaml --apply

# Register runtime profiles from an engine already installed (ADR 0018).
capyctl engine detect [--path DIR]
capyctl engine add [PATH] [--name NAME] [--deep-park enabled|disabled] [--drift warn|refuse]
capyctl engine list
capyctl engine remove NAME [--drain]
capyctl list engines --config server.yaml
```

`start host` starts the local agent, not a remote machine or power-on action. A client-only installation selects a server context and credential reference. Do not mix action-first commands with the previous `capyctl server run` grammar in user documentation.

`deploy model` without `--wait` returns a deployment ID after durable acceptance. `--wait` waits for the specific accepted target operation, not forever for the deployment to remain ready. Machine-readable JSON and stable error codes are required; exact output layout can be finalized with the CLI tests. Owner decision 2026-09-25: commands that read records (`list`, `status`, `engine list`, `engine detect`) print an aligned, human-readable table by default, terminal or not; `--format json` (or `--json`, or the older `--output json`) prints the JSON result unchanged and reports errors as JSON. List/status commands do not activate models as a side effect.

The management API is the source of semantics for CLI, future UI, and integrations. Required operations cover deployment creation/revision, inspection, lifecycle actions, inventory, invitation/enrollment, events, and effective configuration. Return structured errors such as invalid configuration, unauthorized profile, host unavailable, insufficient resources, queue full, unsupported capability, activation timeout, and unreconciled ownership. Owner decision 2026-09-25: a start that places nothing because no allowed host is eligible for placement (drain-only after version skew, draining, revoked, offline or not reconciled) is refused `host_ineligible` (CLI exit 15), naming each host and why, with the host's and the server's versions for a drain-only host; `capacity_blocked` is reserved for capacity. `start deployment --evict` covers every instance the start activates and plans them together before releasing anyone, never evicting beyond what placement needs; when one instance cannot be placed even with eviction, nothing is released and the refusal (`capacity_blocked`, CLI exit 4) names the instance and each host's need, free and evictable memory. `start deployment --wait` exits 0 only once every instance is ready. Exact HTTP paths and protobuf field numbers are to be frozen in the first implementation plan, not inferred from these command sketches.

> **Amended by [ADR 0018](design/adr/0018-engine-registration.md)** (owner decision 2026-09-25).

`capyctl engine detect|add|list|remove` and `capyctl list engines` (§4.2) add nine closed codes to the error vocabulary, each with a stable CLI exit: `engine_not_found` (16, the named or picked path has no `vllm-*`/`sglang-*` `dist-info`), `engine_unsupported` (17, its engine family is not one CapyCTL integrates), `engine_version_failed` (18, the bounded version check failed or timed out), `profile_exists` (19, the name is already registered, declared in the role document, or reserved for a standalone environment profile), `profile_in_use` (20, `engine remove` without `--drain` while a deployment on this machine uses the profile, naming it), `publish_rejected` (21, the running role validated the profile like a startup publication and refused it; the previous approved snapshot is kept), `agent_unreachable` (22, no role is listening on `<state_dir>/control.sock`; `add` still writes `engines.yaml` and the role picks it up at its next start, `remove` writes nothing), `not_interactive` (23, `engine add` needs an operator choice — a name or a `detect` pick — and stdin is not a terminal). A deploy naming a `runtime_profile` that no allowed host publishes is refused at once, nothing stored: `profile_not_published` (HTTP 409, CLI exit 24), naming the profile, each allowed host with the profiles it publishes, and the fix (`capyctl engine add <path> --name <profile>` on a host, then deploy again). Exit code 9 stays unused.

## 15. Configuration model and generated defaults

### 15.1 Separation of authority

| File/object | Owns | Excludes |
|---|---|---|
| Server YAML | Listeners, authentication, enrollment, state location, scheduler and lifecycle defaults. | Host executable paths, static copies of all enrolled hosts, individual deployment records. |
| Host YAML | Server identity reference, approved inventory, aggregate boundaries, storage pools, ingress, runtime profiles, supervision. | Model-specific allocations, global routing, private independent swap scheduling. |
| Engines file (`engines.yaml`) | Runtime profiles registered with `capyctl engine add`, beside the role's configuration file; written only by `capyctl engine add` and `remove`, merged with the role document at load. | Anything else; a profile name the role document also declares. |
| Deployment YAML | Model identity, runtime profile, placement/topology, per-host budgets, cache choice, route, lifecycle overrides. | Agent secrets, executable installation, controller credentials. |
| Cache-service record | Unique physical allocation, backend identity, client quotas, lifetime and storage policy. | Duplicate per-client charging of the full service. |
| CLI context | Selected management endpoint and credential reference. | A running service role. |

Operator configuration is not mutable runtime state. Server/agent processes write identities, journals, reservations and operational evidence separately; no continuous rewriting of administrator YAML. Creating a missing configuration during initialization/enrollment is an explicit documented exception.

> **Amended by [ADR 0018](design/adr/0018-engine-registration.md)** (owner decision 2026-09-25).

The engines file is CapyCTL-owned operational state, not administrator YAML: CapyCTL writes it only when the operator runs `capyctl engine add` or `remove`, under a lock and atomically, with its revision in the first-line comment `# capyctl-document-revision: N`. The role's own document is never rewritten.

> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).

CapyCTL never rewrites an administrator document. ADR 0019 had sanctioned a one-time migration of the old loopback inference default; it was removed before 0.1.0 (owner decision 2026-09-29), so a stated inference bind, loopback included, is always honoured as written.

### 15.2 No-config behavior

| Situation | Behavior |
|---|---|
| No `--config`; implicit role config exists | Load and validate it. Invalid existing content is an error, not a reset trigger. |
| No `--config`; no implicit server config | Atomically generate safe per-user config/state and protected credentials; start locally authenticated listeners. Print file locations, not secret contents. |
| No implicit standalone config | Generate local server + embedded host defaults; no remote enrollment or engine execution occurs automatically. |
| Host already enrolled | Load its saved configuration and identity. Do not enroll again. |
| Host not enrolled, no config | Generate an offline host template, explain the required join step, and exit without guessing a server. |
| Explicit config path missing or invalid | Fail. Generate only through an explicit `init` or enrollment workflow, never silent startup fallback. |
| Existing identity missing or mismatched | Fail for recovery; do not overwrite trust or silently create a new host identity. |

Creation must be atomic and owner-protected, with no clobber on concurrent starts. Use platform-appropriate per-user config/state paths; system-wide paths require an explicit service installation choice. Run-time role flags can override ordinary settings under documented precedence, not host safety limits or identity checks.

Defaults: local-only listeners with authentication; no public binding, no arbitrary script execution, no automatic engine installation/download, no large host-cache reservation or persistent storage writes until a deployment enables them. Discover inventory but do not execute detected engines merely because they are on PATH. Online enrollment is possible before any profile is eligible.

> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).

Defaults: the inference listener binds all interfaces (`0.0.0.0:8443`) and requires the API key; `authentication: none` is an explicit opt-out that warns at start when the bind is not loopback. Management listeners stay loopback-only. (Model downloads from Hugging Face and HTTP sources are allowed by default within a bounded store, per the ADR 0008 amendment of 2026-09-25; engines are still never installed automatically.)

`auto` budgets must resolve to finite, versioned, inspectable allocations before launch. Establish a deterministic conservative headroom policy in the implementation ADR; its exact tuned numbers are not inferred from example YAML. Missing recipe estimates require an explicit estimate or controlled verification under a safe reservation, not unbounded startup. If a safe estimate cannot be established, block with a useful diagnostic. This specification does not authorize forced model loading merely to make a generated default appear turnkey.

Material default changes affect new deployments only unless an explicit update is requested. Persist effective values, source/provenance, schema version, and profile revisions with each deployment. Hard constraints compose by intersection; exceeding a host ceiling is an error or queue condition, not silent shrinking.

### 15.3 Validation

Reject unknown CapyCTL fields, duplicate YAML mapping keys, invalid units, unsatisfied required fields, unsupported role/adapter combinations, conflicting reserved arguments, invalid cache references, and contradictory standalone/remote connections. Secrets use references or protected files. No arbitrary YAML object construction or shell evaluation.

Validate syntax/schema before side effects. Resolve paths and fingerprints locally, then perform distributed preflight. Editing a profile cannot mutate an active process in place; changes require controlled revision/reconciliation. A decrease in host limits below current reservations blocks new admission and reports the condition rather than killing workloads immediately.

## 16. Illustrative configuration examples

The matching files are included under `examples/`. They are parseable schema sketches, not runnable installations or measured resource recipes. Host names refer to the planned lab; network addresses and all byte/time values are examples. No certificate files, secrets, engine binaries, or checkpoints are included.

A deployment document needs only `name`, `engine` (its runtime profile) and `model`; every other deployment field below is optional and defaulted from the document, the host and the checkpoint when absent (ADR 0014 amendment A4). The examples state every field to show the schema.

The canonical examples use `memory.system` for the host's system physical domain: unified CPU/GPU capacity on a Spark, host RAM only on a discrete-GPU system. A discrete-GPU schema also needs per-device `device_memory` phase budgets; omission must not be interpreted as unlimited VRAM. The first schema ADR should preserve this distinction.

### 16.1 Server: policy and durable state

```yaml
schema_version: 1
kind: server
name: lab
state_dir: /var/lib/capyctl/server
listeners:
  management:
    bind: "10.10.0.10:7443"
    advertise_url: "https://capyctl.lab:7443"
    authentication: admin_token
  agents:
    bind: "10.10.0.10:7444"
    advertise_url: "https://capyctl.lab:7444"
    authentication: mtls
  inference:
    bind: "10.10.0.10:8443"
    advertise_url: "https://capyctl.lab:8443"
    authentication: api_key
tls:
  mode: managed
  identity_dir: /var/lib/capyctl/server/identity
enrollment:
  method: invitation
  default_invitation_ttl: "15m"
scheduler:
  residency: exclusive_per_pool
  queue:
    max_requests_per_deployment: 32
    max_buffered_bytes_total: "64MiB"
    wait_timeout: "10m"
lifecycle_defaults:
  activation: on_demand
  parking: auto
  ready_idle_timeout: "5m"
  parked_idle_timeout: "30m"
logging:
  level: info
  log_prompts: false
```

This is an explicitly networked example, not the auto-generated local-only default. Server trust and management authentication must be established before remote use. The bootstrap endpoint uses server TLS plus invitation authorization, distinct from enrolled-agent mutual TLS.

### 16.2 Host: aggregate boundaries and approved runtimes

```yaml
schema_version: 1
kind: host
name: host-a
state_dir: /var/lib/capyctl/host
server:
  url: "https://capyctl.lab:7444"
  identity_dir: /var/lib/capyctl/host/identity
resource_policy:
  allowed_devices: ["gpu:0"]
  memory:
    accounting: auto
    system:
      managed_limit: "96GiB"
      free_reserve: "12GiB"
      host_kv_limit: "16GiB"
      parked_limit: "16GiB"
  max_parked_groups: 3
storage_pools:
  checkpoints:
    path: /srv/models
    access: read_only
  kv-local:
    path: /srv/capyctl/kv-cache
    aggregate_limit: "200GiB"
    filesystem_free_reserve: "50GiB"
network:
  ingress:
    mode: when_api_facing
    bind: "10.10.0.21:8444"
    advertise_url: "https://host-a.lab:8444"
    authentication: router_mtls
    tls: managed
  engine_api:
    bind: "127.0.0.1"
    port_range: [8100, 8199]
runtime_profiles:
  vllm-stock:
    adapter: vllm
    launch:
      type: exec
      command: ["/opt/vllm/bin/vllm", "serve"]
      argument_contract: native
  vllm-patched:
    adapter: vllm
    launch:
      type: exec
      command: ["/opt/inference/start-vllm-patched"]
      argument_contract: native
      working_directory: /opt/inference
      environment_file: /etc/capyctl/runtime/vllm-patched.env
  sglang-patched:
    adapter: sglang
    launch:
      type: exec
      command: ["/opt/sglang/bin/python", "-m", "sglang.launch_server"]
      argument_contract: native
security:
  allow_development_engine_controls: false
supervision:
  process_mode: foreground
  stop_grace_period: "30s"
logging:
  level: info
  log_prompts: false
```

`free_reserve` and `managed_limit` are simultaneous constraints, not additive allowances. The host-KV and parked limits are sub-limits. Device aliases resolve to stable local identities. A worker host uses its own identity and ingress address; ingress remains inactive while all its assignments are headless. Engine HTTP loopback binding does not restrict the separate engine-peer transport to loopback.

This example opts out of development engine controls explicitly, so a profile needing vLLM development endpoints cannot deep-park under it: a `deep` residency fails resolution and `restart_only` is required. Omitting the setting enables deep parking (ADR 0012).

> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).

On a discrete-GPU host the resource policy declares host RAM as a `distinct` system domain and each GPU as a `device` domain that names its device (the full document is `examples/host-discrete.yaml`):

```yaml
resource_policy:
  domains:
    system:
      memory: distinct          # host RAM only
      managed_limit: "24GiB"
      free_reserve: "8GiB"
      parked_limit: "12GiB"      # parked residue in host RAM, host_backed copies included
      host_kv_limit: "4GiB"      # host-KV offload budget
    gpu0:
      memory: device
      device: gpu0               # the device whose memory this is
      managed_limit: "14848MiB"
      free_reserve: "1536MiB"
      parked_limit: "2GiB"       # CUDA context left by parked engines
  devices:
    gpu0: {domain: gpu0, sharing: shared}
```

`host_kv_limit` is refused on a device domain, a device domain maps exactly its own device, and a unified domain is never combined with a device domain (`unsupported_gpu_topology`). The agent refuses to start when a device domain does not match the observed GPU (`device_policy_mismatch`).

### 16.3 Single-host deployment: allocations and cache choice

```yaml
schema_version: 1
kind: deployment
name: coding-small
ownership: managed
model:
  path: /srv/models/coding-small-r1
runtime_profile: vllm-patched
placement:
  hosts: ["host-a"]
resources:
  per_host:
    devices: ["gpu:0"]
    memory:
      system:
        activation_peak: "48GiB"
        ready_budget: "40GiB"
        parked_budget: "2GiB"
route:
  model_id: coding-small
lifecycle:
  activation: on_demand
  parking: auto
kv_cache:
  integration: none
engine_config:
  context_length: 65536
  memory:
    kv_cache: "8GiB"
```

`integration: none` disables a CapyCTL-managed external offload integration; it does not remove the engine's active attention KV or forbid native in-memory prefix caching. All private active allocations must fit the memory contract. These budgets do not assert that an unspecified checkpoint fits; the pinned recipe must be verified. `engine_config` is validated for shape; whether the engine supports the combination on this checkpoint is the user's responsibility (ADR 0011).

### 16.4 Two-host engine group (one TP2 instance) with private host-cache and persistent storage

```yaml
schema_version: 1
kind: deployment
name: coding-large
ownership: managed
model:
  path: /srv/models/coding-large-r1
runtime_profile: sglang-patched
placement:
  hosts: ["host-a", "host-b"]
  head: host-a
topology:
  tensor_parallel: 2
  pipeline_parallel: 1
resources:
  per_host:
    devices: ["gpu:0"]
    memory:
      system:
        activation_peak: "80GiB"
        ready_budget: "72GiB"
        parked_budget: "8GiB"
route:
  model_id: coding-large
lifecycle:
  activation: on_demand
  parking: auto
kv_cache:
  integration: hicache
  ownership: private
  availability: required
  per_host:
    host_memory_budget: "6GiB"
    storage:
      pool: kv-local
      max_bytes: "80GiB"
      reclaimable: true
```

The 6 GiB private host cache is included in each applicable phase total, not added again. Its actual retention must fit the parked budget or parking fails/requires reclamation. Disk namespaces survive according to retention policy and remain charged. `required` means an unsupported cache integration blocks this deployment, rather than silently changing semantics. This is a topology/resource example, not proof that any named model supports the chosen combination.

This example is one instance whose engine group spans two hosts. Load-balanced instances are a different shape: `instances: 2` with `placement: {hosts: [host-a, host-b], strategy: spread, max_per_host: 1}` and a single-host topology runs two independent engine groups behind one route (ADR 0013). Multi-host group placement is not yet specified.

A shared-cache variant must reference a registered service identity instead of declaring independent full service allocations per deployment. Until a service/quota schema and backend tests exist, reject that variant clearly; do not implement fake shared quotas with per-client files. The unique-owner accounting contract is required from the foundation milestone even while additional backends are added later.

### 16.5 Generated standalone shape

```yaml
schema_version: 1
kind: standalone
server:
  name: local
  state_dir: ./state/server
  listeners:
    management:
      bind: "127.0.0.1:7443"
      authentication: admin_token
    inference:
      bind: "127.0.0.1:8443"
      authentication: api_key
host:
  name: local
  state_dir: ./state/host
  connection: embedded
  resource_policy:
    allowed_devices: auto
    memory:
      accounting: auto
      system:
        managed_limit: auto
        free_reserve: auto
  runtime_profiles: {}
```

Relative paths in this example resolve against the configuration file, not an arbitrary current working directory. An actual auto-generated file uses appropriate per-user absolute paths. No model is started and no inference runtime is executed by this shape. The standalone listeners serve plain HTTP on loopback, so the shape has no `tls` block and a standalone document that states one is refused (§15.3). Older generators wrote `server.tls: {mode: managed, identity_dir: <state root>/identity}`; a block equal to exactly that value is accepted so existing installations keep starting, reported at boot as ignored, and never rewritten (R13). Any other `server.tls` value is refused with its path. An embedded host cannot also specify a remote `server.url`. Numeric `auto` values require resolution before a deployment is admitted.

> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).

The generated shape states `inference.bind: "0.0.0.0:8443"` (with `authentication: api_key`). The sentence above that standalone listeners "serve plain HTTP on loopback" now reads: standalone listeners serve plain HTTP; use a private network or a TLS reverse proxy. The management listener stays on loopback.

## 17. Observability and benchmark evidence

Record operation/generation IDs, state transitions, participant states, reservation decisions, cleanup evidence, queue depth/bytes, activation progress, and failures. Status must distinguish configured capacity, granted reservation, observed use, and unknown observations. Protect high-cardinality metrics from unbounded user input.

Measure queue wait, drain time, park/stop duration, allocation/weight restoration, readiness time, request-to-first-token, total response latency, peak physical memory, retained cache/storage use, failed switches, and cache hit/reload behavior where observable. Report distributions and workload context, not a single best reload RPC.

Compare cold initialization, restart with existing compiler artifacts, immediate park/wake, return after another model creates memory pressure, and persistent-cache hit/miss cases. Record page-cache conditions: a checkpoint reload from an OS cache is not necessarily an NVMe cold read. Include exact engine, hardware, checkpoint, dtype, topology, drafter, storage, and request profile.

No specific Spark wake time, throughput, or percentage speedup is promised. Do not carry earlier conversational benchmark numbers into a product guarantee without independent reproducible evidence.

## 18. Delivery sequence and module boundaries

These are implementation slices, not dates or time estimates. Each slice must leave a coherent tested product. Preserve the remote-host and resource-owner contracts from the beginning even when first tests use embedded or simulated participants.

| Slice | Deliverable | Exit gate |
|---|---|---|
| F0 — Contracts and foundation | Action-first CLI skeleton; strict config/default behavior; durable objects; resource ledger; abstract host/adapter/launcher contracts; fake engines and transport. | State, allocation, idempotency, and bootstrap/default tests pass without GPUs. |
| F1 — First vLLM path | Standalone/local managed launch and attachment; streaming router; durable deployment submission; restart-only switching; deep parking gated by host policy (default on, host opt-out; ADR 0012). | Two profiles alternate safely; experimental park/reload validated where allowed; failures reconcile. |
| F2 — Shared single-host foundation and SGLang | Close relevant vLLM/shared gaps; add SGLang, declared-tier parking, retained-runtime preinitialization, fit-based coexistence, and API/CLI contracts usable by a future UI. | Concurrent vLLM/SGLang serving and pressure-driven vLLM -> SGLang -> vLLM warm switching pass shared contracts and live verification of the selected recipes on authorized hardware. No parallel controller implementation or silent cold-stop fallback. |
| F3 — Remote host operation | Real invitation enrollment, persisted identity, outbound control stream, private ingress, multi-host ownership and recovery. | CLI-to-server deployment on remote hosts; reconnect, revocation, stale-command and orphan tests. Transport scaffolding may be exercised earlier in F0. |
| F4 — Distributed and cache verification | Named two-Spark group recipes; per-host release evidence; private and shared-cache combinations as supported. | A -> B -> A under worker failure, retained caches, and aggregate pressure; quota and persistence tests. |
| F5 — Broader packaging/backends | Supported service/container launchers, platform packages, compatible restart-only backends, selected API expansion. | Each advertised backend/platform has explicit supported tests and dependency documentation. |

F2 must follow the first working vLLM slice rather than wait behind UI, marketplaces, broad scheduling, or every remote topology. Full remote and multi-node management remain product requirements with named gates, not accidental capabilities of a shell script.

Recommended module boundaries: domain/state, durable store, scheduler/accounting, protocol, controller, router/ingress, host supervision, engine adapters, launchers, CLI/config, and test harness. Exact crate/file layout belongs in the agent's first implementation plan. Prefer small coherent modules, not a monolithic command handler or premature public plugin ABI.

## 19. Decision register and review gates

### Fixed direction

Fresh standalone product; multi-engine with vLLM then SGLang; one CLI for all roles; action-first grammar; external runtimes; explicit placement; durable deployment IDs; distinct attachment/ownership; agent on each directly managed host; ingress on API-facing members; aggregate host boundaries with deployment requests; independent cache ownership; safe generated defaults; declared-tier parking and restart fallback.

### Baseline proposals to record before implementation

| Topic | Proposed baseline | Required action |
|---|---|---|
| Language/package | Rust; one executable per platform. | Record ADR; avoid an unapproved Python/Rust split. |
| Persistence | Embedded transactional server store, local agent journal; SQLite candidate. | Specify atomic acceptance, recovery, migrations, and backups. |
| Remote control | Versioned gRPC + mutual TLS; agent-initiated session. | Freeze schemas, bootstrap authorization, revocation, and reconnection semantics. |
| YAML/API surface | Versioned strict configuration with the boundaries in this spec. | Turn illustrative fields into tested schema and fixtures; document any renaming. |
| Numerical defaults | Conservative, finite, versioned, visible; examples are not calibration. | Select initial policy with tests; do not infer limits from screenshots or marketing capacity. |
| License | Open source; exact license not selected here. | Obtain explicit project-owner selection before publication; do not invent a license grant. |
| Deployment updates | Revision-aware update/restart, no hot mutation. | Specify conflict handling and interruption behavior before adding update commands. |

There are no approved engine-build versions or production-verified Spark recipes in this specification. Live verification on authorized hardware and the development-endpoint security issue are release gates, not missing details an agent can fill with confident assumptions.

## 20. Acceptance test matrix

Every requirement below needs an automated test where feasible; real-engine and hardware gates supplement, not replace, the deterministic suite. Test names are suggested identifiers, not existing tests.

| ID | Scenario | Required evidence |
|---|---|---|
| T01 | Action-first parsing and role startup | Correct role selection; no accidental model start or remote host power-on semantics. |
| T02 | Missing implicit configuration | Safe files/identity created once; local authenticated listeners; no engine execution. |
| T03 | Missing/invalid explicit config or duplicate YAML keys | Clear failure without fallback, file overwrite, or side effects. |
| T04 | Concurrent initialization | Atomic creation; one state owner; no credential overwrite. |
| T05 | Invitation enrollment | Server authenticated before secret exchange; one-use/expiry enforced; local key retained. |
| T06 | Reconnect, name collision, revocation, and recovery | Stable identity; no duplicate host; unauthorized replacement/revoked sessions rejected; a revoked host recovers only through an explicit, single-use recovery invitation under its same host id, its old certificate stays refused, and its engines reopen only on fresh proof (ADR 0016). |
| T07 | Online host without prepared runtimes | Inventory visible; deployment preflight fails specifically; no implicit installation. |
| T08 | Non-wait deployment and CLI crash | ID returned after persistence; operation continues; status works from a new client. |
| T09 | Lost response and idempotent retry | One deployment and one launch despite repeated submission. |
| T10 | Administrative stop versus idle stop | Explicit stop blocks autoactivation; idle eviction remains on-demand eligible. |
| T11 | Attached service | Routing works; no implicit sleep/kill/restart/adoption. |
| T12 | Patched foreground wrapper | Native arguments preserved; signals/exit status and owned descendants handled. |
| T13 | Detached or hidden remote launch | Invalid ownership rejected or managed by an explicit supported launcher. |
| T14 | Reserved flags/profile change | Conflicts fail; effective config shows provenance; a superseded binding identity is not reused. |
| T15 | Simultaneous activation requests | Single activation operation and one group; no duplicate processes. |
| T16 | A -> B -> A | Correct generations, model readiness, release evidence, and resource retention. |
| T17 | Active streaming during swap | Drain honors completion/cancellation; no premature park or response replay. |
| T18 | Late ingress request | Stale generation/admission token rejected after closure. |
| T19 | Fairness and queue bounds | Busy A cannot reset the window forever; byte/count limits and deadlines enforced. |
| T20 | Park/reload timeout or partial failure | No blind repeated collective; reconcile, quarantine, or verified restart. |
| T21 | vLLM experimental-controls policy | Development controls enabled by default for deep parking and refused when the host opts out (`security.deep_park: disabled`; standalone `CAPYCTL_DEEP_PARK=off`); omitted policy enables; controls reachable only on loopback with the per-launch key; no public admin passthrough; status shows the experimental-controls surface (ADR 0012). |
| T22 | SGLang conformance | Same domain/controller tests pass; no assumption of vLLM sleep semantics. A custom build that keeps the probed shapes launches `restart_only` and `deep`; one missing the saver hooks refuses `deep` and Park with `capability_missing:deep_park` and serves `restart_only`; installation drift is flagged, refused only under `installation_drift: refuse` (§8.1). |
| T23 | Peak activation versus steady state | Candidate blocked when transient demand exceeds the available budget. |
| T24 | Retained private host caches: 9 + 8 > 16 GiB | Admission blocked until supported reclamation is verified. |
| T25 | Shared cache ownership | Physical service counted once; clients subject to their quotas; one client cannot stop all users. |
| T26 | Unified versus discrete memory | No double-counted unified RAM and no missing per-device VRAM limits, and device-domain observation, derived device and system budgets, and a switch on a small card (ADR 0019). |
| T27 | Overlapping GPU sets / shared host RAM | Exclusive assignment and aggregate limits both enforced without deadlock. |
| T28 | Disk retention and shared filesystem | Stopped engines retain charged cache data; quotas/free reserve cover all writers. |
| T29 | Unknown/external memory pressure | No invented capacity; local agent can reject a stale server plan. |
| T30 | Multi-node start failure | No early route; all partial owners cleaned or quarantined; reservations not prematurely freed. |
| T31 | Head crash with surviving worker | Worker agent supplies ownership evidence and cleanup; no competing activation. |
| T32 | Lost worker connection or expired lease | Reservation remains unavailable until evidence/fencing resolves state. |
| T33 | Controller/agent restart and PID reuse | Reconciliation from durable records; no arbitrary process adoption or killing. |
| T34 | Old command/session replay | Stale generations rejected; ambiguous effects reconciled. A newer host is refused ("upgrade the server first"), an older-than-N-1 or unversioned host is drain-only, and a command needing a capability the host did not declare is refused typed and never sent (ADR 0017). |
| T35 | KV persistence across park and restart | Observed hit/miss behavior correct; incompatible data never reused. |
| T36 | Required versus optional cache outage | Required blocks; optional uses declared fallback, not improvised live reconfiguration. |
| T37 | Security boundaries | Method/path and destination allowlists, credential redaction, no remote shell privilege escalation. CapyCTL's runtime helpers follow the owner-only rule (group write only through the owner's private group); its private state admits no group write; engine installations get no permission rule (§13.3). A non-loopback inference bind without a key warns; no constant key (ADR 0019). |
| T38 | Server crash with live inference | Honest request failure semantics; no exactly-once/resumable stream claim. |
| T39 | Numerical default change and replay | Existing deployment retains its pinned effective contract until explicit update. |
| T40 | Performance comparison | Reproducible phase/TTFT distributions with cache conditions and pinned profiles; no unsupported speedup claim. |

The first real-hardware proof is two managed deployments sharing one exclusive pool with correct restart-only service, a live-verified deep-park path where permitted, and repeated recovery tests. The second proof adds mixed-engine concurrent serving when capacity permits, sequential preinitialization, and pressure-driven warm switching without changing the controller model. Remote and two-Spark certification follow their explicit gates.

## 21. Revision history and source boundary

### Changes from revision 0.1

Added standalone/remote role boundaries, per-host agents and head-only ingress, explicit deployment/attachment ownership, action-first CLI, control-plane deployment with durable IDs, host invitation enrollment, native/custom runtime contracts, aggregate resource-owner accounting, shared-cache capacity, default generation, and a traceable acceptance matrix. Replaced the previous combined host/deployment resource configuration. Remote control is now a first-class designed interface, with live verification staged in delivery.

### Later amendments

- 2026-09-25, [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md): discrete NVIDIA GPUs as `device` memory domains with the host-RAM park tier, one GPU per model picked by CapyCTL, and the inference listener on all interfaces behind its key (§6.2, §7.2, §13.3, §15.1, §15.2, §16.2, §16.5, T26, T37).

### Sources

Original project source: the owner's `capyctl-initial-design.md` revision 0.1 and the design decisions that followed it. Primary documentation below was inspected on September 10, 2026. It is live/unversioned documentation; implementers must recheck against pinned builds. No inference was executed on the owner's machines while this specification was prepared.

- **[S1]** vLLM, Sleep Mode — levels, startup prerequisites, explicit restoration: <https://docs.vllm.ai/en/latest/features/sleep_mode/>.
- **[S2]** vLLM, Security — development-mode warning and endpoint exposure: <https://docs.vllm.ai/en/latest/usage/security/>.
- **[S3]** SGLang, SGLang for RL Systems — memory release/resume and weight updates: <https://docs.sglang.io/docs/advanced_features/sglang_for_rl>.
- **[S4]** NVIDIA, DGX Spark Hardware Overview — unified system memory: <https://docs.nvidia.com/dgx/dgx-spark/hardware.html>.
- **[S5]** vLLM, Engine Arguments — cache/memory parameter scope: <https://docs.vllm.ai/en/latest/configuration/engine_args/>.
- **[S6]** vLLM, KV Offloading Usage Guide — offload connectors and storage tiers: <https://docs.vllm.ai/en/latest/features/kv_offloading_usage/>.
- **[S7]** LMCache, Configuration Reference — service capacity and isolated quotas: <https://docs.lmcache.ai/mp/configuration.html>.
- **[S8]** gRPC, Core Concepts — bidirectional streaming: <https://grpc.io/docs/what-is-grpc/core-concepts/>.
- **[S9]** gRPC, Authentication — TLS and client authentication: <https://grpc.io/docs/guides/auth/>.
- **[S10]** vLLM, Parallelism and Scaling — API-facing and headless multi-node deployment: <https://docs.vllm.ai/en/latest/serving/parallelism_scaling/>.
- **[S11]** SGLang, HiCache Best Practices — private host tiers and external storage scope: <https://docs.sglang.io/docs/advanced_features/hicache_best_practices>.

**Handoff rule:** implement the declared contracts, report unsupported combinations, and retain operational evidence. Do not convert design sketches, upstream endpoints, or previous conversational performance examples into claims of working functionality.
