# Discrete NVIDIA GPU support and a network inference endpoint — design

Date: 2026-09-25. Status: owner decisions A and B (2026-09-25) are binding; this
document turns them into a design. It will be recorded as ADR 0019, amending SPEC
§7.2, §13.3, §15.2, §16.2 and §16.5, and §20 T26 and T37. Implementation plan:
`docs/plans/2026-09-25-discrete-gpu-and-network-endpoint.md`.

## Owner checks

These points are not settled by decisions A and B. The design below takes the
recommended option in each case; the owner should confirm or overrule before the
matching plan task starts.

1. **Existing standalone documents keep a loopback inference listener.** Every
   generated `standalone.yaml` written before this change states
   `inference.bind: "127.0.0.1:8443"` explicitly. Honouring it (recommended) means an
   upgraded install stays on loopback and the new `0.0.0.0` default reaches only new
   installs, plus a boot hint naming `--listen`. The alternative, treating that exact
   generated value as "default" (as §16.5 already does for the legacy `tls` block),
   silently widens exposure on upgrade. The same applies to an existing server
   document. (Plan Task 11.)
2. **Host-backed parking stays out of 0.1.0.** On a discrete host the fast park tier
   (vLLM sleep level 1, SGLang `--enable-weights-cpu-backup`) is meaningful for the
   first time, but no park path executes it today. Recommended: keep `deep` as the
   only parking tier, and refuse `residency: host_backed` at resolution on every host
   with `residency_unsupported:host_backed` until a later slice implements and
   live-verifies it. (Plan Task 6.)
3. **Multi-GPU hosts: explicit device selection only.** Publishing every GPU as its
   own device and memory domain, and letting a deployment name the device
   (`devices: [{id: gpu1}]`), is cheap and is in this design. Automatic choice of a
   free GPU by the scheduler is not: a multi-GPU host without an explicit device in
   the deployment places on `gpu0` only. Confirm that this is "cheap enough", or ask
   for a clear refusal of multi-GPU hosts instead. (Plan Tasks 2 and 3.)
4. **Live check on the 16 GB discrete-GPU laptop host.** The live rows need new
   vLLM and SGLang virtual environments (x86_64 wheels) on that host, and live
   engine work on it. `AGENTS.md` today authorizes live work only on the two lab
   hosts and forbids new environments beyond the listed exceptions. The owner update
   of 2026-09-25 asks for this check; the plan records it in `AGENTS.md` as a named
   exception (Task 15) and needs the owner to confirm that wording.
5. **Default for `mllm start server`'s inference listener.** Decision B names both
   roles. For a server the generated template changes to `0.0.0.0:8443`; management,
   bootstrap and control listeners are unchanged. Confirm that the server's
   generated default should move as well (recommended: yes, same rules as
   standalone).

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
with the plan for later.

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
reads. A request larger than the device managed limit is refused at deploy with
`insufficient_device_memory` and the numbers, before anything is stored. The
unified template is unchanged.

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

- **Deep** (the default, ADR 0012) frees device memory: vLLM sleep level 2 and
  SGLang's memory saver release weights and KV. The parked phase charges the device
  residue and the host overhead only, so a parked model does not block the next
  one on the GPU. `parked_limit` on the device domain bounds how many parked
  engines' contexts may sit on the card; `max_parked` still bounds the count.
- **Host-backed** stays unimplemented in 0.1.0 (owner check 2). Refusing it at
  resolution with `residency_unsupported:host_backed` replaces today's state, where
  it resolves on a `distinct` domain but no park path executes it.
- **Restart-only** is unchanged: it never parks, and eviction stops it.

### 6. Engine launch on a discrete device

- The guarded launcher already sets `CUDA_VISIBLE_DEVICES` to the device's physical
  UUID. With several devices, the device the deployment selected is the one set.
- **vLLM** renders `--kv-cache-memory-bytes` from the grant (unchanged). On a
  discrete device the adapter does not render `--gpu-memory-utilization`; vLLM 0.29
  sizes the KV pool from the explicit bytes. The live check verifies that vLLM does
  not refuse to start because another engine's parked context holds part of the
  card.
- **SGLang** needs `mem_fraction_static` as a fraction of the device's total memory
  on a discrete device, not of `MemAvailable`. The agent passes the observed device
  total in the launch placement (a new field of the entry's closed launch spec,
  `device_total_bytes`), and `sglang_server_args.static_fraction` uses it when
  present; the unified path keeps `MemAvailable`. This closes ADR 0014 open issue 2
  for discrete devices, subject to the live check.

### 7. Multi-GPU hosts

- Every observed discrete GPU is published as device `gpuN` (N = the driver index at
  boot) with its own device domain `gpuN` and its physical UUID. The inventory
  publication stops withholding UUIDs when more than one device is present; each
  device entry carries its own.
- A deployment selects its device by id (`devices: [{id: gpu1}]`); the derived
  budget charges that device's domain. Without a `devices` entry, a standalone
  deployment and the documentation examples use `gpu0`.
- A deployment naming two or more devices, or `tensor_parallel > 1`, is refused
  `multi_gpu_unsupported` with the message "one GPU per model in 0.1.0;
  tensor-parallel and multi-device placement are planned after 0.1.0".
- The scheduler does not choose among GPUs of one host. Automatic device choice is
  the planned follow-up and needs its own design: device choice becomes part of the
  placement result and the command's resource plan.

### 8. Remote hosts and version skew

- A host reports device domains in its inventory as `DomainObservation` entries with
  `kind: "device"` and the device id. That is a protocol addition, so it is a new
  ADR 0017 capability, `device_memory_domains`, declared by hosts that can observe
  device memory.
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
  `0.0.0.0:8443`. A document stating another address keeps it (owner check 1). The
  standalone validator accepts any `bind` for `inference` that parses as a socket
  address with a non-zero port and is not multicast; the management listener keeps
  its loopback-only rule.
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
| `residency_unsupported:host_backed` | resolution | owner check 2 |
| `host_capability_missing:device_memory_domains` | placement | an older host cannot report device memory |

CLI exit codes: `insufficient_device_memory` and `device_unobserved` use the
existing insufficient-resources exit (4); `multi_gpu_unsupported`,
`unsupported_gpu_topology` and `residency_unsupported:host_backed` use the existing
unsupported exit (5); `device_policy_mismatch` and `missing_system_allocation` use
the invalid-configuration exit (2). No new exit number is introduced.

### 12. Testing

CPU and Fake-engine tests are not qualification; they pin the accounting and the
configuration. The live rows are the evidence that a native engine recipe works.

Deterministic (tagged with SPEC §20 IDs):

- **T26** collector parsing: discrete rows, integrated `[N/A]` rows, malformed,
  duplicate, oversized and timed-out output; shape detection.
- **T26** standalone host policy on a discrete fixture: system plus device domains,
  limits from the table, device UUID per device; unified fixture unchanged
  byte-for-byte.
- **T26/T23** derived budgets: two allocations per phase; parked residue; explicit
  resources without a system allocation refused.
- **T27/T16** switching: two 8 GiB-weight models on a 16 GiB device with 61 GiB of
  RAM — the planner chooses the first as victim; the launch check with the same
  fixture refuses before the victim's release and admits after it.
- **T29** device observation missing or stale closes admission and keeps
  reservations.
- **T34** capability gating: a host without `device_memory_domains` is refused
  typed; the command is never sent.
- **T21/T37** listener and auth: default bind, `--listen`, loopback-only management,
  `authentication: none` warning text on non-loopback and silence on loopback,
  unauthorized request rejected on a `0.0.0.0` bind, no constant key.
- **T02/T03** generated documents: new standalone and server templates; an existing
  loopback document still starts on loopback.
- Python: `static_fraction` against a device total; unified path unchanged.

Live (the 16 GB discrete-GPU laptop host, x86_64, RTX-class 16 GB card, 61 GB RAM;
owner check 4). Two small instruct models whose derived device requests do not fit
together in the device managed limit (for example a 4B model and a 3B model in
bf16):

- **DG1** standalone vLLM: deploy both; request A, then B — A deep-parks, B serves;
  request A — B parks, A wakes; `status` shows device-domain charges; `nvidia-smi`
  confirms the parked engine's residue is within the placeholder.
- **DG2** standalone SGLang: the same sequence with the memory saver.
- **DG3** mixed: A on vLLM, B on SGLang, switching both ways.
- **DG4** refusal: a model whose request exceeds the device limit is refused at
  deploy with `insufficient_device_memory`; nothing hangs.
- **DG5** network: from a lab host over the tailnet, a request with the key
  succeeds and one without is 401; with `--listen 127.0.0.1:8443` the peer cannot
  connect; `authentication: none` on `0.0.0.0` prints the warning.
- **DG6** remote host (if the laptop host runs a host role against a server): the
  device domain appears in `mllm list hosts` and a deployment places and switches.
  Optional for 0.1.0 if DG1–DG5 pass; recorded as pending otherwise.

The GB10 lab hosts rerun one existing unified switching row to show no regression.

## Out of scope

- Tensor parallelism and multi-device models (parked since the two-host program).
- Automatic GPU choice on multi-GPU hosts (§7).
- Host-backed parking (owner check 2).
- AMD, Intel and Apple GPUs; MIG partitions; containers.
- TLS termination in mllm, per-client API keys, rate limiting.
- Remote access to the management API.
- Changing engine environments or drivers on any existing host.
