# ADR 0019 — Discrete NVIDIA GPUs and a network inference endpoint

**Status:** Accepted (owner decisions A and B, 2026-09-25, and the owner's decisions on PR #38
the same day).
**Amends:** `SPEC.md` §6.2 (`host_backed` is supported, and the default, where device memory is
distinct from host RAM), §7.2 (a `device` domain per GPU beside a `distinct` system domain),
§13.3 (the inference listener may be reachable from the network, behind its key), §15.1 (one
sanctioned rewrite of an administrator document), §15.2 (listener defaults), §16.2 (a
discrete-host resource policy), §16.5 (the generated inference bind), and §20 T26 and T37.
**Related:** ADR 0007 (phase-aware admission, resident credit), ADR 0010 (declared park tiers),
ADR 0012 (deep parking default-on, engine listener protections), ADR 0013 (instances and
placement), ADR 0014 (deployment engine configuration, derived budgets), ADR 0017 (capability
gating). Design: `docs/specs/2026-09-25-discrete-gpu-and-network-endpoint-design.md`.

## Context

mllm was built and verified live on hosts whose device and system memory are one physical
pool (GB10, `memory: unified`). Every memory reading came from `/proc/meminfo`: standalone
published one `unified` domain sized from host RAM, the launch check refused any policy
that was not exactly one unified domain, and a process's GPU memory and anonymous RSS were
summed into one resident credit.

On a machine with a 16–32 GB discrete card and 32–128 GB of RAM, standalone therefore
admitted against host RAM. The switch planner saw room for a second model and evicted
nothing, the launch check saw free RAM and passed, and the second engine then failed or
stalled in CUDA allocation on a full GPU. The planner and the launch check agreed with each
other and both disagreed with the hardware. A remote host with such a card was refused
outright, because its observation was unknown.

Separately, the inference endpoint was loopback-only on both roles. A home user could not
reach their models from a laptop or a Tailscale peer without building a proxy. The router
already enforced a constant-time bearer key (SPEC §13.3, T37), so opening the listener was
mostly a configuration and warning problem, with one defect to remove first: standalone
fell back to a constant key, `mllm-local`, when it created credentials on a boot and could
not read them back.

## Decision

### 1. Detection

One bounded collector (`mllm_agent::gpu_memory`), shared by standalone and the host agent,
runs `nvidia-smi --query-gpu=index,uuid,pci.bus_id,name,memory.total,memory.used,memory.free
--format=csv,noheader,nounits` from a fixed absolute path (`/usr/bin/nvidia-smi`, then
`/bin/nvidia-smi`) with a cleared environment, stdin closed, stderr discarded, a 3 s bound
and at most 64 KiB of output. No NVML binding and no new crate. Parsing is closed: a
malformed row, a duplicate index or UUID, or `used + free` above `total + 64 MiB` invalidates
the whole sample. A row whose memory fields read `[N/A]` or `[Not Supported]` is an
integrated device (the GB10 reports its memory that way); nothing is inferred from a
product name. The host shape follows: no GPU; `Unified` (all integrated, unchanged);
`Discrete` (one or more discrete devices); mixed integrated and discrete is refused at boot
with `unsupported_gpu_topology`.

The host agent samples on its own thread every second and keeps the last reading; a reading
older than 5 s counts as unobserved. The session loop never runs `nvidia-smi` inline, so a
slow sample cannot delay a heartbeat.

### 2. The `device` domain

`resource_policy.domains.<name>.memory` gains `device`. A device domain names exactly one
device (`device: gpuN`), that device maps to it, and no other device does. `host_kv_limit`
is refused on a device domain (host-KV lives in RAM). A policy declares at most one
`unified` domain, and a policy with a `unified` domain declares no `device` domain
(`unsupported_gpu_topology`). A discrete host declares one `distinct` system domain (host
RAM) and one device domain per GPU. Existing unified documents are unchanged and keep their
stored identity and digest: the stored `device` field is serialized only when present.

Standalone derives its policy from the observation. Unified hosts keep today's shares
(managed 50 %, reserve 20 %, parked 25 %, host-KV 10 % of RAM). On a discrete host the
system domain takes the same shares of RAM, and each GPU's domain `gpuN` has
`free_reserve = max(1 GiB, 8 % of total)`, `managed_limit = total − reserve` and
`parked_limit = min(2 GiB × max_parked, 25 % of total)`. The reserve absorbs a desktop
compositor on a workstation card; memory already used at boot lowers availability through
the ordinary observation.

A remote host declares the same shape in `host.yaml`. `mllm validate config` checks the
rules offline; the agent checks the declaration against the observation at start (every
device domain's device observed, `managed_limit + free_reserve` within the observed total,
the UUID matching the inventory) and refuses to start with `device_policy_mismatch`
otherwise.

### 3. Derived budgets charge both domains

On a host whose selected device maps to a device domain, `derive_resources` produces two
allocations per phase:

| Phase | Device domain | System domain |
|---|---|---|
| cold | startup peak (at least the request) | engine host overhead |
| ready, parking, wake | request | engine host overhead (parking and wake also carry the `host_backed` copy) |
| parked (`host_backed`) | parked device residue | engine host overhead + weights copy |
| parked (`deep`) | parked device residue | engine host overhead |
| parked (`restart_only`) | 0 | 0 |

The engine host overhead is the placeholder `ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES`
(4 GiB) and the parked device residue `PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES` (1 GiB),
both labelled `derived` until a first run measures them. The unified placeholder
`PARKED_RESIDUAL_PLACEHOLDER_BYTES` (2 GiB) is unchanged. The `host_backed` weights copy is
1.5 times the checkpoint's weight bytes (`HOST_BACKED_COPY_FACTOR`: pinned host memory is
rounded up per tensor; measured live at 1.37 times with vLLM 0.29 on two models), charged on
the system domain as parked residue, so it counts against the system `parked_limit` as well
as `managed_limit`. SGLang's weights CPU backup holds the copy for the engine's whole life,
and so does vLLM 0.29 in practice (level 1 frees its backup tensors on wake, but the pinned
allocator keeps the memory; measured live), so a `host_backed` deployment carries it in
every phase on both engines. A host-KV budget charges the system domain, never the device
domain.

A deployment that states no memory gets a device request of `weights × 1.10 + kv_cache`,
with `kv_cache = min(4 GiB, 25 % of the device managed limit)`. A vLLM request is at least
75 % of the card: vLLM 0.29 with CUDA graphs needs `--gpu-memory-utilization` of at least
0.75 to start a 4B model on a 16 GB card (observed on the discrete-GPU laptop host). The
device domain is charged the request plus the engine's CUDA context and graphs, which it
holds beyond the request (a 1.25 GiB placeholder for both engines; measured live, vLLM
0.29 held 13.2 GiB against a 12.0 GiB request), so the planner, admission and the launch
check judge what the engine really holds. A request whose charge is larger than the device
managed limit is refused at deploy with `insufficient_device_memory`, before anything is
stored. Unknown weights (a Hugging Face or
HTTP source not yet downloaded) are accepted provisionally and re-resolved once the
checkpoint digest measures them (ADR 0014 §7). A declared KV cache
(`local_engine.kv_cache`, `--kv-cache`, `MLLM_KV_CACHE_BYTES`) is honoured within the
device budget or refused with the same code; it is never ignored. Explicit `resources:` on
a discrete host must name the system domain too, or they are refused
`missing_system_allocation` (SPEC §16: omission is not unlimited). A deployment written for
`domain: unified` does not place on a discrete host; derived budgets are the portable form.

### 4. Observation, residents and the launch check

- **Observation.** The system (or unified) domain keeps the `/proc/meminfo` reading. Each
  device domain is reported from the collector (`capacity = total`, `available = free`). A
  device missing from a fresh sample is unknown: admission on that domain closes
  (`device_unobserved`, SPEC §7.2) and every existing reservation stays charged.
- **Residents.** On a discrete host a process's GPU bytes are credited to the device domain
  its device maps to and its anonymous and shared RSS (`RssAnon` + `RssShmem`) to the
  system domain; on a unified host the two are summed as before. Found live on a 16 GB
  card: a `host_backed` copy is pinned host memory, which the kernel counts as shared, not
  anonymous (vLLM's parked copy of Qwen3-4B was 11.2 GB of `RssShmem` beside 1.9 GB of
  `RssAnon`). A settled parked owner is credited too (its residue and its copy are in
  use). A park or a wake is charged only what it adds beyond the owner's own charge, and a
  domain it adds nothing to is not judged on free memory. These are one rule on every
  host shape (decided 2026-09-26: mllm behaves the same on discrete GPUs and unified
  memory; only which sampled figure counts differs, because the hardware pools the memory
  differently). The credit stays bound to the recorded runtime identity (ADR 0007),
  and every allocation of a multi-domain footprint is credited.
- **Admission and switching** keep their algorithms; the device domain is simply the
  binding constraint on a small card. Two models whose device requests do not fit together
  produce a victim even when their host RAM fits.
- **Launch check.** Every domain of the cold footprint is checked against its own source:
  declared, within `managed_limit`, the limit within the observed capacity, and
  `bytes + free_reserve` within the observed availability. It refuses with
  `insufficient_memory` for a system or unified domain and `insufficient_device_memory` for
  a device domain; an unobservable device is uncertain, never a pass. The fresh GPU sample
  is taken before the host journal lock is acquired, never while holding it. A co-residence
  check covers every domain.
- **Engine sizing.** vLLM renders `--gpu-memory-utilization` as the device request's share
  of the card, rounded up to a whole percent, between 0.75 and 0.99, beside
  `--kv-cache-memory-bytes`. SGLang's static fraction is computed against the device total,
  passed in the launch as `device_total_bytes`, instead of `MemAvailable` (this closes ADR
  0014 open issue 2 for discrete devices, subject to live evidence).

### 5. Three residency tiers on a discrete host

| Tier | Park | Wake | Device after park | Host RAM after park |
|---|---|---|---|---|
| `host_backed` | weights copied to pinned host RAM, KV dropped | weights copied back, KV reallocated, prefix cache reset | residue | weights copy |
| `deep` | weights and KV dropped | weights reloaded from disk | residue | overhead only |
| `restart_only` | never parks; eviction stops it | cold start | 0 | 0 |

- **vLLM `host_backed`** launches with `--enable-sleep-mode` and parks with
  `POST /sleep?level=1`; it wakes with `wake_up?tags=weights`, then `tags=kv_cache`,
  `reset_prefix_cache`, `is_sleeping` false and the probe. No `reload_weights` call is
  made: the adapter answers the reload step from the level-1 restore's evidence, so the
  coordinator's persisted step sequence is unchanged.
- **SGLang `host_backed`** launches with `--enable-memory-saver` and
  `--enable-weights-cpu-backup`; park is `release_memory_occupation`, wake is
  `resume_memory_occupation` (restoring the weights from the pinned copy), then
  `flush_cache` and the probe. No `update_weights_from_disk` call is made. mllm's saver
  binding accepts the weights backup only for a `host_backed` launch.
- **Default.** A deployment that states no residency gets `restart_only` when its profile
  opted out of deep parking. Otherwise, on a discrete host, it gets `host_backed` when the
  weights copy plus the engine host overhead fits the system domain's parked room (the
  smaller of its `parked_limit` and `managed_limit`), and `deep` when it does not or when
  the weights are not known yet. On a unified host it gets `deep`. The rule is one function
  (`mllm_config::deployment_defaults::default_residency`), used by standalone's template
  and by every deployment on every role. A re-resolution with measured weights chooses
  again.
- **Unified hosts.** `host_backed` stays refused at resolution there
  (`host_backed_unavailable`, ADR 0010 decision 5: the copy would free nothing). A declared
  `host_backed` whose copy exceeds the system domain's parked room on a discrete host is
  refused with the same code.
- **When a copy does not fit at switch time.** The switch planner parks a victim when its
  parked footprint still fits after the switch and stops it otherwise, never silently
  parking `deep` and never overcommitting. "Fits" is the smaller of the ledger's room and
  the host's fresh observation (free memory plus what the victims' sampled processes
  return, less every charge and the free reserve), the rule the arm applies, so a copy that
  memory other programs hold leaves no room for is planned a stop up front. The victim set
  is the ledger's; nothing is released on the observation alone. A switch park the arm
  still refuses for memory is refused `parked_capacity` on every host shape and the victim
  is stopped. The switch record says which:
  `released: parked`, `released: stopped (host RAM full)` or `released: stopped`. On a
  unified host a `deep` victim whose parked residue would not fit beside the waiting
  instance is stopped for the same reason.
- **Capability.** A `host_backed` launch is gated on the engine's parking capability like
  `deep`: a build whose probe lacks it is refused before any effect with
  `capability_missing:deep_park` (the existing capability, whose hint names parking of
  either tier and `restart_only`), not with a new code.
- **SGLang ModelOpt (NVFP4) checkpoints** are refused `capability_missing:deep_park` for
  `host_backed` as for `deep`: a wake of such a checkpoint is unproven, so it fails closed
  and those deployments stop instead of parking.

### 6. One GPU per model; mllm picks the GPU

Every observed discrete GPU is published as device `gpuN` (N is the driver index at boot)
with its own device domain `gpuN` and its physical UUID. A deployment that pins no device
is resolved once per GPU of the host; placement evaluates each (host, GPU) pair and picks,
on the chosen host, the GPU where the instance fits with the most headroom (ties by index;
a stopped instance's last GPU wins when it fits). When none fits, the switch planner
evicts per GPU and takes the GPU whose minimal victim set is smallest, then least recently
used, then the lowest index; only owners charged on that GPU's domain are candidates. The
chosen GPU is recorded with the instance (store schema v37, forward-only). A deployment
pins a GPU with `devices: [{id: gpuN, sharing: shared}]`; placement then considers only that
GPU. An instance started through a path that does not place it runs on the lowest-index GPU.

Both engines are pinned to the chosen GPU on every multi-GPU or device-domain host, never
given all GPUs: by `CUDA_VISIBLE_DEVICES=<UUID>` when the UUID is published, else by index
with `CUDA_DEVICE_ORDER=PCI_BUS_ID`, and the launch is refused when neither is known. A
deployment naming two or more devices, or `tensor_parallel > 1`, is refused
`multi_gpu_unsupported` ("one GPU per model in 0.1.0; tensor-parallel and multi-device
models are planned after 0.1.0").

### 7. Remote hosts: capability `device_memory_domains`

A host reports device domains as `DomainObservation` entries with `kind: "device"` and
`device_id` (field 9), and splits each resident as `ProcessResidency.device_bytes`
(field 5) and `host_bytes` (field 6), beside the existing `resident_bytes` (their sum).
These additions and launches whose footprint and chosen GPU name a device domain form one
ADR 0017 capability, `device_memory_domains`. The server places a device-domain footprint
only on a host that declared it; any other host is refused
`host_capability_missing:device_memory_domains` before anything is sent. A remote unified
host keeps exactly its previous credit and needs the capability for nothing.
`PROTOCOL_VERSION` and the command encoding version do not change.

### 8. The inference listener binds all interfaces

- **Default.** The inference listener of both the server and standalone defaults to
  `0.0.0.0:8443` (one shared default, `mllm_config`). The standalone validator accepts any
  non-multicast socket address with a non-zero port for inference; IPv6 (`[::]:8443`) is
  accepted.
- **Narrowing.** The address follows the settings rule: `--listen <addr:port>` on
  `mllm start server` and `mllm start standalone`, then `MLLM_INFERENCE_ADDR` (read by both
  roles; `MLLM_STANDALONE_INFERENCE_ADDR` is still read, with a deprecation warning), then
  `listeners.inference.bind` (`server.listeners.inference.bind` in standalone), then the
  default. `--set` and `MLLM_SET__…` apply on top, as for any setting.
- **One-time migration.** On the first start of this release, a server or standalone
  document whose inference bind is exactly `127.0.0.1:8443` (the old generated default) is
  rewritten to `0.0.0.0:8443`. The rewrite replaces that value's single occurrence, so
  comments and layout survive, writes a temporary file in the same directory, syncs it and
  renames it over the document, keeping the original beside it as `<file>.pre-0.1.0` with
  the same mode. If the value does not occur exactly once, or the write fails, the document
  is left unchanged and the role serves on `0.0.0.0:8443`
  (`config_migration_failed`, a warning, not an exit); the migration stays pending, so
  every later start that finds the unchanged old default does the same until the document
  states another address. The marker
  `<state_dir>/migrations/inference-bind-v1` is written on that first start whatever it
  found (as pending when the document could not be rewritten), so a completed migration
  never runs twice and an operator who sets `127.0.0.1:8443` back keeps it. Any other address is never migrated, and authentication is never changed. The
  start prints a one-time notice to stderr:

  ```
  NOTICE: mllm 0.1.0 serves inference on all interfaces: 0.0.0.0:8443 (was 127.0.0.1:8443).
  The API key is still required. Configuration updated: <path> (previous copy: <path>.pre-0.1.0).
  To keep inference local, start with --listen 127.0.0.1:8443 or set listeners.inference.bind.
  ```

- **TLS.** The inference listener serves plain HTTP. That is acceptable on loopback and
  over Tailscale, which encrypts the path; beyond a private network a TLS reverse proxy in
  front of a loopback or tailnet bind is required. mllm does not terminate TLS in 0.1.0.

### 9. Authentication stays on unless turned off explicitly

The API key is required by default. It is turned off in exactly three ways:
`listeners.inference.authentication: none` in the document, `--no-inference-auth` for one
run, or `MLLM_INFERENCE_AUTH=none` (flag over variable over document). When the effective
bind is not loopback and authentication is off, the role prints to stderr, before it accepts
connections:

```
WARNING: the inference endpoint on 0.0.0.0:8443 accepts requests without an API key.
Anyone who can reach this address can use your models and GPU.
Set listeners.inference.authentication: api_key, or bind to 127.0.0.1 or a Tailscale address with --listen.
```

A loopback bind without a key prints nothing. `mllm status` repeats it as
`inference: unauthenticated on <addr>`, read from the management view
`GET /management/v1/inference-listener`. The constant `mllm-local` key is removed:
credentials that cannot be read, including ones created on this boot, stop the start
(`MissingCredentials`). The key is never printed or logged; it lives only in the
owner-only credentials file.

### 10. Management and engines are unchanged

The management listener stays on loopback with its admin token (any loopback port, in
standalone set with `--management-listen` or `MLLM_MANAGEMENT_ADDR`); remote administration
is `ssh` plus the local CLI. A server's bootstrap and control listeners keep mutual TLS.
Engine listeners stay on loopback with per-launch keys and mllm's key-guard middleware (ADR
0012); the router remains the only path from the network to an engine and forwards only
the allowlisted inference routes. The router's constant-time key comparison, method and
path allowlist, header stripping and queue bounds apply unchanged on every bind.

### 10a. Upgrading a generated policy whose machine changed shape

Decided 2026-09-25. Standalone generates its host's resource policy from the
machine it observes. A release that reads the machine differently (0.1.0-rc.4 recorded a
discrete-GPU machine as one `unified` domain) must not refuse to start. On the first start
that observes a different shape (or another engine port range), the generated policy is
replaced, under the accounting rules every other change follows:

- every deployment still holding memory under the previous policy is stopped by the
  ordinary Stop first; only its verified cleanup releases the charge. Nothing is released on
  the observation alone. A Stop that cannot prove its engine gone within the drain bound
  plus one minute keeps the reservation, and the start is refused naming the deployment
  (the next start retries);
- the policy, the host's ledger keys and the ledger epoch change in one transaction, journaled
  as `host_resource_policy_migrated`;
- each deployment resolved against the previous policy is accepted again from its stored
  document as a new revision for the new shape; one that does not resolve there (for
  example a file that names the `unified` domain) is listed with the instruction to deploy
  it again;
- the role prints one notice for the migration; the next start finds the policy current.

No running engine is kept across the change: every frozen revision names the policy
context it was resolved against, so an engine charged under the old context could no
longer be parked or stopped through it. An enrolled host's policy is written by its
operator and is never replaced: a changed shape is refused at publication with the recorded
and declared domains and what to do (restore the recorded domains, or stop everything on the
host and enroll the machine again as a new host).

### 11. Closed codes

| Code | Where | Meaning | CLI exit |
|---|---|---|---|
| `insufficient_device_memory` | deploy, launch check, placement status | the device domain cannot hold the allocation with its reserve | 4 |
| `device_unobserved` | admission, status | a device domain has no fresh observation; admission is closed on it | 4 |
| `device_policy_mismatch` | host start, validation | the declared device domain does not match its rules or the observed GPU | 2 |
| `unsupported_gpu_topology` | host and standalone start, validation | integrated and discrete GPUs mixed, or a unified domain beside another unified or a device domain | 5 |
| `missing_system_allocation` | resolution | explicit resources on a discrete host omit the system domain | 2 |
| `multi_gpu_unsupported` | resolution | more than one device, or tensor parallelism, in one deployment | 5 |
| `host_backed_unavailable` | resolution | `host_backed` on a unified domain, or a copy larger than the system domain's parked room | 5 |
| `capability_missing:deep_park` | launch | a `host_backed` (or `deep`) launch on a build whose probe lacks parking, or an SGLang ModelOpt checkpoint | launch refusal |
| `host_capability_missing:device_memory_domains` | placement | an older host cannot report device memory or take a GPU choice | placement refusal |
| `config_migration_failed` | start | the listener migration could not rewrite the document; the role serves on `0.0.0.0:8443` and says so | warning, no exit |

The design's error table listed `host_backed_unavailable` also for a build lacking the
parking capability. The implementation reuses `capability_missing:deep_park` for that case
(controller ruling, 2026-09-25): it avoids a new closed code across the probe, the parser
and the protocol, at the cost of a less specific capability name in one refusal. The CLI
maps each code to an existing exit (`StructuredError::closed_code`); no exit number is
added.

## Consequences

- A discrete-GPU machine is accounted as it is: the planner, admission and the launch
  check read the same device domain, so a switch the planner accepts passes the launch
  check with the same numbers once its victims are released.
- The host-RAM tier makes a wake a host-to-device copy instead of a disk reload on discrete
  hosts, at the cost of pinned host RAM that the ledger charges and bounds.
- Every deployment carries two allocations per phase on a discrete host; explicit
  `resources:` written for one domain are refused rather than read as unlimited.
- A fresh install serves inference to the network with a random 256-bit key. An existing
  install is widened once, with a notice, a backup and a marker; narrowing it is one flag or
  one line.
- One more closed-set capability exists; an older host with a discrete GPU stays refused,
  now with a precise reason.
- With the 0.75 utilization floor and the device overhead, a vLLM deployment needs a card
  of about 10 GiB or more (an 8 GB card's managed limit is below 0.75 of the card plus the
  overhead); on a smaller card the host still boots and each vLLM deployment is refused
  `insufficient_device_memory` with its numbers. SGLang has no floor.
- CPU and Fake-engine tests pin the accounting and the configuration. They are not
  qualification: that a native engine recipe works on a discrete card is established only
  by the live rows (DG1–DG7).

## Open issues

1. Moving a running or parked instance to another GPU is out of scope; a stopped instance
   prefers its last GPU.
2. The engine host overhead (4 GiB) and the parked device residue (1 GiB) are placeholders.
   They are to be replaced by measured values (process RSS and parked residue, kept per
   revision, host and installation as ADR 0014 keeps the startup peak) after live runs.
3. The SGLang ModelOpt refusal for `host_backed` may be lifted after a live check, since a
   wake from the CPU backup does not reload from disk.
