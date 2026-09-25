# Discrete NVIDIA GPU support and a network inference endpoint — design

Date: 2026-09-25. Status: owner decisions A and B (2026-09-25) are binding, and the
owner decided the open points on PR #38 the same day (below); this document turns
them into a design. It will be recorded as ADR 0019, amending SPEC §6.2, §7.2,
§13.3, §15.1, §15.2, §16.2 and §16.5, and §20 T26 and T37. Implementation plan:
`docs/plans/2026-09-25-discrete-gpu-and-network-endpoint.md`.

## Owner decisions on PR #38 (owner-decided, 2026-09-25)

1. **Existing configurations migrate to `0.0.0.0`.** On upgrade, a server or
   standalone document whose inference listener is the old loopback default moves
   to `0.0.0.0:8443`. The API key stays required unless the document opts out. The
   upgrade prints a clear one-time notice, and the release notes say how to narrow
   the address again (`--listen` or the document). (§9; plan Tasks 14 and 15.)
2. **The host-RAM park tier is in 0.1.0.** A `host_backed` park keeps the weights in
   pinned host RAM and a wake copies them back: vLLM sleep level 1, SGLang's memory
   saver with `--enable-weights-cpu-backup`. The host-RAM copy is charged in the
   ledger on the system domain and bounded by its `parked_limit`. `deep` (drop the
   weights) stays. The tier is chosen per deployment, with `host_backed` as the
   default on a discrete-GPU host. (§5; plan Tasks 6, 11 and 12.)
3. **mllm picks the GPU on a multi-GPU host.** Each GPU is its own device-memory
   domain; placement chooses the GPU with room, evicting on that GPU when needed;
   the launch sets `CUDA_VISIBLE_DEVICES` to the chosen GPU. A deployment may pin a
   GPU with `devices: [{id: gpuN}]`. (§7; plan Task 7.)
4. **Live work on the maintainers' local machines.** `AGENTS.md` authorizes live
   work on the maintainers' local machines, a discrete-GPU laptop included: no
   driver, CUDA or system-package changes, engine virtual environments only in the
   home directory. (Plan Task 19.)
5. **The server's generated inference listener is also `0.0.0.0:8443`**, with the
   same rules as standalone. Management, bootstrap and control listeners are
   unchanged. (§9; plan Task 14.)

## Problem

### A. A discrete GPU is invisible to accounting

mllm was built and live-verified on hosts whose device and system memory are one
physical pool (GB10, `memory: unified`). Everything that measures memory reads
`/proc/meminfo`:

- **Standalone** (`crates/mllm-cli/src/standalone_config.rs`) publishes a single
  domain named `unified`, sized from host RAM, and the device `gpu0` maps into it.
  Its deployment template states fixed shares of that RAM figure (cold 20 %, ready
  15 %, KV 10 %).
- **Observation** (`crates/mllm-cli/src/host_observation.rs`, the agent's
  `inventory()` refresh and `remote_roles.rs` connect report) reads one
  `/proc/meminfo` sample and reports it for every declared domain; the remote
  report sends `-1` (unknown) for anything that is not a single unified domain.
- **The launch check** (`native_execution/refusal.rs::admit_memory`) refuses any
  host whose policy is not exactly one unified domain, and otherwise compares the
  cold allocation plus free reserve with `MemAvailable`.
- **SGLang's static fraction** (`runtime/sglang_server_args.py`) is computed against
  `MemAvailable`; the code notes discrete devices are unmodelled (ADR 0014 open
  issue 2).
- **Per-process residents** (`crates/mllm-agent/src/process_residency.rs`) add a
  process's GPU memory and its anonymous RSS into one credit, which is right only
  when both come from one pool.

On a machine with a 16–32 GB RTX card and 32–128 GB of RAM, standalone therefore
admits against host RAM. The switch planner (`mllm-scheduler::switching`) sees
room for a second model and evicts nothing; the launch check sees free RAM and
passes; the second engine then fails or stalls in CUDA allocation on a full GPU.
The planner and the launch check agree with each other and both disagree with the
hardware. On a remote host the same machine is refused outright, because the
observation is `-1` and the launch check wants one unified domain.

The schema already anticipates this. `DomainMemory` has a `Distinct` variant, the
host policy maps each device to a domain (`resource_policy.devices.<id>.domain`),
admission and the switch planner are per-domain (`placement::fits`,
`choose_victims`), and SPEC §16 already says "a discrete-GPU schema also needs
per-device `device_memory` phase budgets; omission must not be interpreted as
unlimited VRAM". What is missing is a device-memory domain with its own
observation, derived budgets that charge it, and launch checks that read it.

### B. The inference endpoint is loopback-only

Standalone's listeners are fixed to loopback (`crates/mllm-config/src/standalone.rs`
refuses any other `bind`; `MLLM_STANDALONE_INFERENCE_ADDR` must be loopback). A
server's inference listener is also forced to loopback (`remote_roles.rs`
`listener(.., local = true)`). A home user cannot reach their models from a laptop
or a Tailscale peer without a hand-built proxy. The router already enforces a
bearer key (constant-time, SPEC §13.3, T37), so opening the listener is mostly a
configuration and warning problem, with one defect to fix first: standalone falls
back to the constant key `mllm-local` when it created credentials on this boot and
cannot read them back (`crates/mllm-cli/src/roles.rs`, `read_api_key`).

## Decisions (owner, 2026-09-25)

A. Support one discrete NVIDIA GPU before 0.1.0. Detect GPU memory with NVML through
`nvidia-smi` or `libnvidia-ml`, with no heavy new dependency. Account a separate
device-memory domain on standalone and on remote hosts, one GPU per model (TP stays
parked). Eviction, parking (deep park frees device memory; host RAM limits govern
parked and host-KV bytes) and switching must work on a 24–32 GB RTX card. Multi-GPU
hosts: select one GPU per model by device id if cheap, otherwise refuse clearly
with the plan for later. (Refined on PR #38: mllm picks the GPU; the host-RAM park
tier is included; see the owner decisions above.)

B. The server and standalone inference endpoint listens on `0.0.0.0` by default (the
port is unchanged) so other machines and Tailscale peers can reach it. An API key
is required by default, with an explicit opt-out (configuration or flag) that prints
a loud warning at start when bound to a non-loopback address without a key.
`--listen <addr:port>` or configuration narrows the address (for example to a
Tailscale IP). The management API keeps its authentication and stays as today
unless this design argues otherwise; engines stay loopback-only (ADR 0012).
Documentation covers a Tailscale example and a TLS reverse proxy for internet
exposure.

## Design

### 1. Detection

One bounded collector, in Rust, in `mllm-agent` (new module `gpu_memory`), shared by
standalone and the host agent:

```
nvidia-smi --query-gpu=index,uuid,pci.bus_id,name,memory.total,memory.used,memory.free \
           --format=csv,noheader,nounits
```

- Same execution rules as the existing residency sampler: absolute path
  (`/usr/bin/nvidia-smi`, then `/bin/nvidia-smi`), cleared environment, stdin
  closed, stderr discarded, 3 s bound, at most 64 KiB of output, killed on timeout.
  No `libnvidia-ml` binding and no new crate: `nvidia-smi` ships with every driver
  that can run the engines, and mllm already runs it.
- Values are MiB; bytes are `MiB × 1 048 576`. A row whose memory fields read
  `[N/A]` or `[Not Supported]` is an **integrated** device: the GB10 reports its
  memory that way because it has none of its own. That is the detection signal for
  a unified host; nothing is inferred from a product name.
- The result per device is `{index, uuid, pci_bus_id, name, total, used, free,
  sampled_at_ms}`. Parsing is closed: a malformed row, a duplicate index or UUID,
  or `used + free > total + 64 MiB` makes the whole sample invalid.
- The UUID must equal the one the existing inventory collector
  (`runtime/sglang_device.py`) observes for the same PCI address; the device
  publication keeps using that collector's digest. A mismatch publishes nothing
  (fail closed, as today).

`HostShape`, computed once at boot and on every observation:

| Observation | Shape |
|---|---|
| No `nvidia-smi`, or no devices | `NoGpu` — as today; SGLang fails placement at the native gate |
| All devices integrated | `Unified` — today's single `unified` domain, unchanged |
| One or more discrete devices | `Discrete { devices }` |
| Mixed integrated and discrete | refused at boot: `unsupported_gpu_topology` |

### 2. Domain model

A discrete host has one **system** domain and one **device** domain per GPU:

```yaml
resource_policy:
  domains:
    system:
      memory: distinct          # host RAM only
      managed_limit: "24GiB"
      free_reserve: "8GiB"
      parked_limit: "12GiB"      # parked residue in host RAM
      host_kv_limit: "4GiB"      # host-KV offload budget
    gpu0:
      memory: device             # new variant
      device: gpu0               # the device whose VRAM this is
      managed_limit: "14848MiB"
      free_reserve: "1536MiB"
      parked_limit: "2GiB"       # CUDA context left by parked engines
  devices:
    gpu0: {domain: gpu0, sharing: shared}
```

- `DomainMemory` gains `Device`. A `device` domain names exactly one device, that
  device maps to it, and no other device does. `host_kv_limit` is refused on a
  device domain (host-KV lives in RAM). A host policy may declare at most one
  `unified` domain, and a host with a `unified` domain declares no `device` domain
  (mixed shapes are refused `unsupported_gpu_topology`).
- **Migration.** Existing unified documents are unchanged and keep their meaning.
  The new variant is additive; the stored policy (`StoredDomain.memory`) already
  stores the string, and `"device"` plus an optional `device` field serialized only
  when present keeps every existing stored policy's identity and digest unchanged.
  No store schema change is expected; if one proves necessary it is forward-only.
- Ledger keys stay host-scoped (`remote_resources::ledger_key`): the device domain
  of a remote host is `host/<n>:<host>/domain/gpu0`.

**Standalone defaults** (`standalone_config::host_policy`), from the observation:

| Domain | managed_limit | free_reserve | parked_limit | host_kv_limit |
|---|---|---|---|---|
| unified (unchanged) | 50 % | 20 % | 25 % | 10 % |
| system (discrete host) | 50 % of RAM | 20 % of RAM | 25 % of RAM | 10 % of RAM |
| gpuN (device) | total − reserve | max(1 GiB, 8 % of total) | 2 GiB × max_parked, at most 25 % | — |

The device reserve absorbs the display server and desktop compositor on a laptop or
workstation GPU; `memory.used` observed at boot above the reserve is reported (not
hidden) and lowers availability through the ordinary observation.

**Remote hosts** declare the same shape in `host.yaml`. `mllm validate config`
checks the declaration against its rules; the agent checks it against the
observation at start: every `device` domain's device must be observed, the
declared `managed_limit + free_reserve` must not exceed the observed total, and the
device's UUID must match the inventory. A mismatch refuses the agent's start with
`device_policy_mismatch`, naming the domain and the observed total.

### 3. Deployment budgets

`derive_resources` (`crates/mllm-config/src/effective/engine_config.rs`) today needs
one domain for the selected devices. On a host whose selected device maps to a
`device` domain, it derives **two** allocations per phase:

| Phase | device domain | system domain |
|---|---|---|
| cold | startup peak (≥ request) | engine host overhead |
| ready, parking, wake | request | engine host overhead |
| parked (`host_backed`) | parked device residue | engine host overhead + weights copy (parked residue category) |
| parked (`deep`) | parked device residue | engine host overhead |
| parked (`restart_only`) | 0 | 0 |

- `engine host overhead` is the host RAM an engine process holds outside the GPU:
  interpreter, CUDA runtime, tokenizer, pinned staging buffers. A new placeholder,
  `ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES = 4 GiB`, is used until a first run
  measures the process RSS (the same measured-peak store ADR 0014 keeps for the
  startup peak, keyed by revision, host and installation). The value is
  deliberately a placeholder and labelled `derived` in `effective`.
- The parked device residue is what a deep-parked engine still holds on the GPU
  (CUDA context, NCCL and allocator buffers). Placeholder
  `PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES = 1 GiB`; measured replaces it the same
  way. The existing `PARKED_RESIDUAL_PLACEHOLDER_BYTES` (2 GiB) stays for unified
  hosts.
- The `host_backed` weights copy is the checkpoint's weight bytes (ADR 0014
  checkpoint facts), charged on the system domain as parked residue, so it counts
  against the system `parked_limit` as well as `managed_limit`. SGLang's
  `--enable-weights-cpu-backup` keeps that copy for the engine's whole life, so for
  an SGLang `host_backed` deployment the copy is charged in every phase, not only
  when parked; vLLM level 1 allocates it only while asleep.
- A KV-cache host-memory budget (`kv_cache.per_host.host_memory_budget`) charges the
  system domain's `host_kv_bytes`, never the device domain.
- A deployment that declares `resources:` explicitly must name both domains on a
  discrete host; one naming only the device domain is refused
  `missing_system_allocation` (SPEC §16: omission is not unlimited).
- A deployment written for a unified host (`domain: unified`) does not place on a
  discrete host: placement refuses it as today (`HostRefusal::Invalid`). Derived
  budgets are the portable form, and the documentation says so.

**Standalone's deployment template** stops stating fixed shares of capacity on a
discrete host. It states `engine_config.memory.request` instead and omits
`resources:`, so phases derive as above. The request is
`weights × 1.10 + kv_cache`, with `kv_cache` defaulting to `min(4 GiB, 25 % of the
device managed limit)`; the weights come from the checkpoint facts ADR 0014 already
reads. vLLM's request is at least `0.75 ×` the device total, because vLLM 0.29
with CUDA graphs needs `--gpu-memory-utilization ≥ 0.75` to start a 4B model on a
16 GB card (observed on the 16 GB discrete-GPU laptop host). A request larger than
the device managed limit is refused at deploy with `insufficient_device_memory` and
the numbers, before anything is stored. The template states `residency:
host_backed` on a discrete host when the host has deep parking on and the weights
fit the system `parked_limit`; otherwise `deep`; `restart_only` when deep parking
is off. The unified template is unchanged.

### 4. Observation, admission and switching

- **Observation.** `HostMemoryObservation` becomes shape-aware. The system (or
  unified) domain keeps the `/proc/meminfo` reading. Each device domain is reported
  from the collector: `capacity = total`, `available = free`. A device that is not
  observed in a sample (collector failed, timed out, or the device disappeared) is
  reported unknown, which closes admission on that domain (SPEC §7.2: unknown
  topology closes unsafe admission) and leaves every existing reservation charged.
- **Residents.** `process_residency` keeps sampling
  `nvidia-smi --query-compute-apps`. On a discrete host the per-process GPU bytes
  are credited against the device domain the process's device maps to, and the
  anonymous RSS against the system domain. On a unified host the two are summed as
  today. The credit is still bound to the recorded runtime identity (ADR 0007).
  `ProcessResident` therefore carries the two figures separately, and
  `resident_floors` credits each allocation of a multi-domain footprint; today it
  skips any owner with more than one allocation, which would bring back the
  double counting found live in matrix row M33 on every discrete host.
- **Admission and switching** need no new algorithm. `placement::fits`,
  `admission::admit` and `switching::choose_victims` already iterate over every
  domain in the footprint and every limit; with two allocations per phase the
  device domain is simply the binding constraint on a small card. The work is in
  the inputs: the limits list must carry the device domain, and the ledger must
  carry the device allocations. Tests pin the case that motivated this design: two
  models whose device requests together exceed the device limit, while their host
  RAM fits, must produce a victim.
- **The launch check** (`admit_memory`) is generalized from "one unified domain" to
  "every domain of the cold footprint": for each allocation, the domain must be
  declared, the allocation must not exceed its `managed_limit`, the limit must not
  exceed the observed capacity, and `bytes + free_reserve` must not exceed the
  observed availability of that domain, read from the matching source (meminfo for
  system or unified, the collector for a device). The refusal names the domain:
  `insufficient_memory` for a system or unified domain (unchanged), and
  `insufficient_device_memory` for a device domain. An unobservable device is
  `LaunchVerdict::Uncertain`, never a pass.
- **Planner and launch check agree** because both now read the device domain. A
  switch that the planner accepts, after its victims' verified release, passes the
  launch check with the same numbers; a test drives both with one fixture.

### 5. Parking on a discrete host

Three tiers, chosen per deployment (`residency`, SPEC §6.2, ADR 0010):

| Tier | Park | Wake | Device after park | Host RAM after park |
|---|---|---|---|---|
| `host_backed` | weights copied to pinned host RAM, KV dropped | weights copied back, KV reallocated, prefix cache reset | residue only | weights copy |
| `deep` | weights and KV dropped | weights reloaded from disk | residue only | none beyond overhead |
| `restart_only` | never parks; eviction stops it | cold start | 0 | 0 |

- **Default.** On a discrete host the standalone template and the documentation
  examples use `host_backed` (wake is a host-to-device copy, several times faster
  than a disk reload, ADR 0010 "What the engines actually do"). On a unified host
  `host_backed` stays refused at resolution (ADR 0010 decision 5: it frees
  nothing), and `deep` stays the default.
- **vLLM `host_backed`**: launch with `--enable-sleep-mode` (as for `deep`); park is
  `POST /sleep?level=1` and `/is_sleeping`; wake is `POST /wake_up?tags=weights`
  (copies the weights back), then `POST /wake_up?tags=kv_cache`,
  `POST /reset_prefix_cache`, `/is_sleeping` false, then the probe. There is no
  `reload_weights` call; the adapter answers the reload step with `WeightsUsable`
  from the level-1 restore's evidence, so the coordinator's persisted step
  sequence is unchanged. The level is fixed by the deployment's residency, never
  chosen at park time.
- **SGLang `host_backed`**: launch with `--enable-memory-saver` and
  `--enable-weights-cpu-backup`; park is `release_memory_occupation` for both tags
  (the saver copies weights to its pinned host buffer); wake is
  `resume_memory_occupation`, which restores the weights from that buffer. There is
  no `update_weights_from_disk`; the reload step is answered from the restore's
  evidence as for vLLM, then `flush_cache` and the probe. mllm's saver observer and
  binding (`runtime/sglang_saver_binding.py`, `sglang_saver_residency.py`) refuse
  `enable_weights_cpu_backup` today and must accept it for the weights tag of a
  `host_backed` launch only.
- **Accounting.** The parked phase charges the device residue, the engine host
  overhead and, for `host_backed`, the weights copy on the system domain (§3). The
  system `parked_limit` bounds the total of parked copies; `max_parked` still bounds
  the count; the device `parked_limit` bounds parked CUDA contexts.
- **When a copy does not fit.** The switch planner releases a victim by parking it
  when its parked footprint fits the host after the switch, and by stopping it
  otherwise (the same stop the planner uses for `restart_only`). A `host_backed`
  park that would exceed the system `parked_limit` therefore becomes a stop, never
  a silent `deep` park and never an overcommit. The status line says which
  happened.
- **Pinned memory.** Both engines pin the copy (page-locked). The system domain's
  observed availability already reflects pinned pages; the ledger charge is the
  authority, and the launch check refuses a `host_backed` launch whose copy does not
  fit the system domain.

### 6. Engine launch on a discrete device

- The guarded launcher already sets `CUDA_VISIBLE_DEVICES` to the device's physical
  UUID. With several devices, the device the deployment selected is the one set.
- **vLLM** renders `--kv-cache-memory-bytes` from the grant (unchanged) and, on a
  discrete device, `--gpu-memory-utilization` equal to the device request divided
  by the device total, rounded up to 0.01 and at least 0.75 (see §3). vLLM checks
  that fraction of the card is free at start, so the launch check and the planner
  must already have made that room; a parked engine's residue counts against it.
- **SGLang** needs `mem_fraction_static` as a fraction of the device's total memory
  on a discrete device, not of `MemAvailable`. The agent passes the observed device
  total in the launch placement (a new field of the entry's closed launch spec,
  `device_total_bytes`), and `sglang_server_args.static_fraction` uses it when
  present; the unified path keeps `MemAvailable`. This closes ADR 0014 open issue 2
  for discrete devices, subject to the live check.

### 7. Multi-GPU hosts: mllm picks the GPU

- Every observed discrete GPU is published as device `gpuN` (N = the driver index at
  boot) with its own device domain `gpuN` and its physical UUID. The inventory
  publication stops withholding UUIDs when more than one device is present; each
  device entry carries its own.
- **Placement picks the device.** A deployment on a discrete host without a pinned
  device is resolved once per device of the host (the derived budget names that
  device's domain). Placement evaluates each (host, device) pair with the existing
  `fits`, and picks, on the chosen host, the device where the instance fits with the
  most headroom; ties break by device index. The placement result and the launch
  command's resource plan carry the chosen device.
- **Eviction is per device.** When no device fits, the switch planner runs
  `choose_victims` per device and takes the device whose minimal victim set is
  smallest, then whose victims were least recently used, then the lower index.
  Only owners charged on that device's domain are candidates.
- **Launch.** The agent sets the engine child's `CUDA_VISIBLE_DEVICES` to the chosen
  device's physical UUID (the existing guarded mechanism), so the engine sees one
  device as `cuda:0`. The chosen device is recorded with the instance; a stopped
  instance prefers its last device the way it prefers its last host (ADR 0013 §4).
- **Pinning.** `devices: [{id: gpuN}]` pins the device; placement then evaluates
  only that device.
- A deployment naming two or more devices, or `tensor_parallel > 1`, is refused
  `multi_gpu_unsupported`: "one GPU per model in 0.1.0; tensor-parallel and
  multi-device models are planned after 0.1.0".

### 8. Remote hosts and version skew

- A host reports device domains in its inventory as `DomainObservation` entries with
  `kind: "device"` and the device id. That is a protocol addition, so it is a new
  ADR 0017 capability, `device_memory_domains`, declared by hosts that can observe
  device memory. The same capability covers the two other additions a discrete
  host needs: the chosen device in a launch's resource plan (§7) and the split
  resident figures (§4).
- The server places a deployment whose footprint names a device domain only on a
  host that declared the capability; any other host refuses it with the typed
  reason `host_capability_missing:device_memory_domains`. An older host with a
  discrete GPU therefore stays refused as today, with a clearer reason. A unified
  host needs the capability for nothing.
- `PROTOCOL_VERSION` and the command encoding version do not change. Proto field
  numbers are assigned in the plan.

### 9. Network endpoint: bind and authentication

Configuration shape (the existing `listeners.inference` block of both roles):

```yaml
listeners:
  inference:
    bind: "0.0.0.0:8443"          # default for new documents
    authentication: api_key       # or: none
```

- **Default bind.** New generated standalone and server documents state
  `0.0.0.0:8443`. The standalone validator accepts any `bind` for `inference` that
  parses as a socket address with a non-zero port and is not multicast; the
  management listener keeps its loopback-only rule.
- **Migration of existing documents** (owner decision 1). At the first start of the
  new release, a server or standalone document whose `listeners.inference.bind` is
  exactly `127.0.0.1:8443` (the old generated default) is migrated to
  `0.0.0.0:8443`:
  - The document is rewritten once, atomically (write to a temporary file in the
    same directory, `fsync`, rename), keeping the original beside it as
    `<name>.pre-0.1.0` with the same mode. Only that one value changes: the
    rewrite replaces the value's single occurrence in the text, so comments and
    layout survive. If the value does not occur exactly once in the text, the
    document is not rewritten; the role binds `0.0.0.0:8443` for this run and the
    notice tells the operator to edit the line.
  - A marker `<state_dir>/migrations/inference-bind-v1` records that the migration
    ran. After it exists the document is never migrated again, so an operator who
    sets `127.0.0.1:8443` back keeps it.
  - The one-time notice, printed to stderr and the log at `warn` on that start:

    ```
    NOTICE: mllm 0.1.0 serves inference on all interfaces: 0.0.0.0:8443 (was 127.0.0.1:8443).
    The API key is still required. Configuration updated: <path> (previous copy: <path>.pre-0.1.0).
    To keep inference local, start with --listen 127.0.0.1:8443 or set listeners.inference.bind.
    ```

  - Any other address (a different loopback port, a tailnet address) is an
    operator's choice and is never migrated. Authentication is never changed by the
    migration.
  - This is the one sanctioned rewrite of an administrator document; ADR 0019
    amends SPEC §15.1 and R13 for it.
- **Release notes** state the change, the notice, and both ways to narrow the
  address.
- **`--listen <addr:port>`** on `mllm start standalone` and `mllm start server`
  replaces the inference `bind` for that run (SPEC §15.2: a run-time override of an
  ordinary setting). `MLLM_STANDALONE_INFERENCE_ADDR` is kept and follows the same
  rule; `--listen` wins over it. IPv6 (`[::]:8443`) is accepted.
- **Authentication.** `api_key` stays the default. `authentication: none`, or the
  flag `--no-inference-auth` for one run, turns the router's key check off. (Owner
  decision B wrote `inference.auth: none`; the existing field is `authentication`,
  so the design uses it rather than adding a second spelling.)
- **The warning.** When the effective bind is not loopback and authentication is
  `none`, the role prints, to stderr and the log at `warn`, before it accepts
  connections:

  ```
  WARNING: the inference endpoint on 0.0.0.0:8443 accepts requests without an API key.
  Anyone who can reach this address can use your models and GPU.
  Set listeners.inference.authentication: api_key, or bind to 127.0.0.1 or a Tailscale address with --listen.
  ```

  A loopback bind without a key prints nothing. The warning is repeated in `mllm
  status` output as `inference: unauthenticated on <addr>`.
- **The constant key is removed.** Standalone's `mllm-local` fallback goes away:
  credentials that were just created and cannot be read back are a start failure
  (`MissingCredentials`), as they already are on every later boot.
- **Where the key is.** The start banner prints the inference URL(s) and the path of
  the owner-only credentials file, never the key (SPEC §15.2). The user copies the
  `api_key:` line from that file to configure a client on another machine; the
  documentation shows the one-line command. No new CLI verb is added.
- **Management API unchanged.** It stays on loopback with its admin token. Remote
  administration of a standalone machine is `ssh` plus the local CLI; exposing
  management widens the attack surface to lifecycle control for no requirement in
  decision B. A server's bootstrap and control listeners are unchanged (mTLS).
- **Engines unchanged.** Engine listeners stay on loopback with per-launch keys and
  the key-guard middleware (ADR 0012); the router remains the only path from the
  network to an engine, and it forwards only the allowlisted inference routes.
- **Plain HTTP.** The inference listener has no TLS (standalone refuses a `tls`
  block, SPEC §16.5). That is acceptable on loopback and over Tailscale, which
  encrypts the path. For exposure beyond a private network the documentation
  requires a TLS reverse proxy (Caddy example) in front of a loopback or tailnet
  bind; mllm does not terminate TLS in 0.1.0.

### 10. Security

- SPEC §15.2's "local-only listeners, no public binding" default is amended for the
  inference listener only, and only because it keeps a random 256-bit bearer key by
  default. The management listener, engine listeners and the key-guard protections
  keep their rules.
- The router's constant-time key comparison, method and path allowlist, and header
  stripping (SPEC §13.3, T37) already apply to every inference request; nothing
  bypasses them on a non-loopback bind.
- Request amplification bounds (queue count and bytes, request deadline) already
  apply per listener and are unchanged; they are the denial-of-service bound for a
  network-reachable endpoint. Rate limiting per client is out of scope.
- The API key is never logged or printed; it lives only in the owner-only
  credentials file.
- `nvidia-smi` runs with a cleared environment from a fixed absolute path; its
  output is parsed closed and bounded. It is not a new privilege.

### 11. Error codes

| Code | Where | Meaning |
|---|---|---|
| `insufficient_device_memory` | launch check, deploy, placement status | the device domain cannot hold the allocation with its reserve |
| `device_unobserved` | admission, status | a device domain has no fresh observation; admission is closed on it |
| `device_policy_mismatch` | host start | the declared device domain does not match the observed GPU |
| `unsupported_gpu_topology` | host and standalone start | integrated and discrete GPUs mixed, or two unified domains |
| `missing_system_allocation` | resolution | explicit resources on a discrete host omit the system domain |
| `multi_gpu_unsupported` | resolution | more than one device, or tensor parallelism, in one deployment |
| `host_backed_unavailable` | resolution | `host_backed` on a unified host (existing ADR 0010 refusal, renamed code), or a build whose probe lacks sleep mode or the weights backup |
| `host_capability_missing:device_memory_domains` | placement | an older host cannot report device memory or take a device choice |
| `config_migration_failed` | start | the one-time listener migration could not write the document; the role still starts on `0.0.0.0:8443` and says so |

CLI exit codes: `insufficient_device_memory` and `device_unobserved` use the
existing insufficient-resources exit (4); `multi_gpu_unsupported`,
`unsupported_gpu_topology` and `host_backed_unavailable` use the existing
unsupported exit (5); `device_policy_mismatch` and `missing_system_allocation` use
the invalid-configuration exit (2). `config_migration_failed` is a warning, not an
exit. No new exit number is introduced.

### 12. Testing

CPU and Fake-engine tests are not qualification; they pin the accounting and the
configuration. The live rows are the evidence that a native engine recipe works.

Deterministic (tagged with SPEC §20 IDs):

- **T26** collector parsing: discrete rows, integrated `[N/A]` rows, malformed,
  duplicate, oversized and timed-out output; shape detection.
- **T26** standalone host policy on a discrete fixture (one and two GPUs); unified
  fixture unchanged byte-for-byte.
- **T26/T23** derived budgets for all three tiers; the SGLang `host_backed` copy in
  every phase; explicit resources without a system allocation refused.
- **T27/T16** switching: two models whose device requests do not fit together on a
  16 GiB device with 61 GiB of RAM — the planner parks the first; the launch check
  refuses before the release and admits after it. A `host_backed` victim whose copy
  exceeds the system `parked_limit` is stopped, not parked.
- **T27** GPU picker (Fake devices): two GPUs, the instance lands on the one with
  room; both full, the device with the smaller victim set is chosen; a pinned
  device is honoured; the chosen device reaches the launch's `CUDA_VISIBLE_DEVICES`.
- **T20/T16** host-backed park and wake step sequences for both adapters against
  Fake engines: level 1 sleep, no `reload_weights` or `update_weights_from_disk`
  call, prefix cache reset, probe.
- **T29** device observation missing or stale closes admission and keeps
  reservations.
- **T34** capability gating: a host without `device_memory_domains` is refused
  typed; the command is never sent.
- **T21/T37** listener and auth: default bind, `--listen`, loopback-only management,
  `authentication: none` warning on non-loopback and silence on loopback, 401 on
  every route without the key on a `0.0.0.0` bind, no constant key.
- **T02/T03** generated documents; migration: the old default is rewritten once with
  a backup and a marker and the notice; a second start does nothing; a different
  loopback port is untouched; an unwritable document starts on `0.0.0.0` with
  `config_migration_failed`; authentication never changes.
- Python: `static_fraction` against a device total; the saver observer accepts the
  weights backup only for a `host_backed` launch.

Live, on the 16 GB discrete-GPU laptop host (single GPU, x86_64, 61 GB RAM), using
the vLLM 0.29 and SGLang 0.5.20 virtual environments already in that host's home
directory. vLLM needs `--gpu-memory-utilization` of at least 0.75 for a 4B model
with CUDA graphs on this card (§3); SGLang there has no flashinfer, so its profile
passes `--attention-backend triton`. Two small instruct models whose device
requests do not fit together (a 4B and a 3B model, bf16):

- **DG1** vLLM `host_backed`: A, then B (A parks to host RAM), then A (B parks, A
  wakes from host RAM); `status` shows the device and system charges and the
  parked copy; wake time recorded.
- **DG2** vLLM `deep`: the same sequence; wake time recorded for comparison.
- **DG3** SGLang `host_backed` and `deep`: the same sequences.
- **DG4** mixed: A on vLLM, B on SGLang, switching both ways.
- **DG5** refusal: a model whose request exceeds the device limit is refused at
  deploy with `insufficient_device_memory`; nothing hangs.
- **DG6** network and migration: upgrade a standalone install created by the
  previous release — the notice appears once and the document is migrated; from
  another machine on the tailnet a request with the key succeeds and one without is
  401; `--listen 127.0.0.1:8443` makes the peer's connection fail;
  `authentication: none` on `0.0.0.0` prints the warning.
- **DG7** remote host (optional for 0.1.0): the laptop host runs a host role against
  a server; `mllm list hosts` shows `gpu0`; a deployment places and switches.

The multi-GPU picker has no live row in this plan; it is covered by the CPU/Fake
tests above. The GB10 lab hosts rerun one existing unified switching row to show no
regression.

## Out of scope

- Tensor parallelism and multi-device models (parked since the two-host program).
- Moving a running or parked instance to another GPU.
- AMD, Intel and Apple GPUs; MIG partitions; containers.
- TLS termination in mllm, per-client API keys, rate limiting.
- Remote access to the management API.
- Changing engine environments or drivers on any existing host.
