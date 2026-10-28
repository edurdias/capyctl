# ADR 0035 — One model across several GPUs on one host

**Status:** Accepted (owner decisions 2026-10-25). Proposed 2026-10-24; the owner answered
every open question on 2026-10-25, and the answers are recorded below ("Owner decisions of
2026-10-25"). One answer differs from the recommendation: CapyCTL gains `topology: auto`
(§1a). Target release 0.1.9. Nothing is implemented yet; the SPEC, ADR, guide and
operations texts listed in §15 change with the implementation, as for ADRs 0030, 0031 and
0033.
**Amends (with the implementation):** `SPEC.md` §7.2 ("one GPU serves one model" becomes
"one or more GPUs of one host serve one model"), §8.2 (CapyCTL renders the tensor-parallel
size and the device list), §9.1 and §9.2 (deep parking with several local ranks), §9.4
(llama.cpp's split across GPUs), §11 (a model on several GPUs of one host is not a group),
§15.1 (`topology` without `placement.hosts`, and `topology: auto`), §20 (T46 and live rows
MG1–MG14); ADR 0019 §6 (`multi_gpu_unsupported` narrowed) and §4 (residents read per
device); ADR 0027 §1 (rank processes are engine processes); ADR 0028 §2 (a world size above
1 without named hosts is one host); ADR 0029 §6 (llama.cpp's `--split-mode`,
`--tensor-split` and `--main-gpu` rendered by CapyCTL).
**Related:** ADR 0007 (phase-aware admission, resident credit), ADR 0012 (deep parking and
its protections), ADR 0013 (instances and placement), ADR 0014 (engine configuration; A2
measured startup peak, A13 measured parked residue, A19 parked growth), ADR 0017
(capability gating), ADR 0025 (standalone memory limits, TensorFold's cap), ADR 0030
(decision models stay on one machine and one GPU), ADR 0031 §7 (the loopback listener
check), ADR 0033 and ADR 0034 (LMCache), ADR 0036 (embeddings stay on one GPU).

## Context

### The rule today

ADR 0019 §6: every discrete GPU of a host is a `device` memory domain `gpuN`; placement
picks one GPU per instance, and the engine sees only that GPU
(`CUDA_VISIBLE_DEVICES=<UUID>`). A deployment naming two or more devices is refused
`multi_gpu_unsupported` (exit 5) before any phase is resolved
(`check_single_device`, `crates/capyctl-config/src/effective/core.rs:588-609`). ADR 0028
added `topology` (`tensor_parallel`, `pipeline_parallel`) for one model over several
machines, with one device per host: a world size above 1 needs at least two
`placement.hosts`, and `local_ranks` must be 1 (`group_shape_unsupported`,
`crates/capyctl-config/src/topology.rs:97-150`). Standalone refuses any `topology` with a
world size above 1 (`refuse_in_standalone`, same file).

So a home user with a workstation holding two 24 GB or 48 GB cards cannot run a model that
fits the two cards together but not one of them, although vLLM, SGLang and llama.cpp all
do that on their own with one flag. The only way out today is a second machine and a
group, which is the harder, riskier shape (ADR 0028 §13: unauthenticated peer listeners on
every interface).

### What CapyCTL already has

- **Inventory and domains.** The host samples every GPU (`index, uuid, pci.bus_id, name,
  memory.total, memory.used, memory.free`; `crates/capyctl-agent/src/gpu_memory.rs:27-56`)
  and standalone publishes one `device` domain per discrete GPU with its own limits
  (`crates/capyctl-cli/src/standalone_config.rs:238-286`). Admission already checks every
  domain of a multi-domain footprint (`admit_phase`,
  `crates/capyctl-scheduler/src/residency.rs:177-298`), and a footprint on a discrete host
  already has two allocations per phase (ADR 0019 §3).
- **Per-rank memory.** ADR 0028's amendment of 2026-10-07 sizes a rank's share of the
  weights from the checkpoint's safetensors layout (`share = ceil(stage / T) + replicated`)
  and keeps the KV cache per rank; it was written for one rank per host and does not care
  where the ranks run.
- **Per-process GPU readings.** The host runs `nvidia-smi
  --query-compute-apps=pid,used_memory` every 2 s and attaches each row to a process
  identity (`crates/capyctl-agent/src/process_residency.rs:27-37,146-209`). The driver
  returns one row per process and device ("List of processes having compute context on
  the device", `nvidia-smi --help-query-compute-apps`), but the query asks for no device
  column, the parser keeps repeated PID rows unsummed, and the credit is keyed by process
  identity only (`crates/capyctl-store/src/resident_floors.rs:75-140`). A process holding
  memory on two GPUs is therefore credited once, to the instance's one device domain, with
  one of its two readings. [INFERENCE from the code; no multi-GPU host has run it.]
- **The loopback pin.** Every single-rank vLLM and SGLang launch runs with
  `GLOO_SOCKET_IFNAME=lo`, `NCCL_SOCKET_IFNAME=lo`, no `MASTER_*` or `HOST_IP`, a file
  rendezvous (vLLM's own `file://` store on CUDA; SGLang through
  `SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE`) and `VLLM_HOST_IP=127.0.0.1`, checked again
  right before the engine starts (`runtime/loopback_rendezvous.py:1-120`). It exists because
  torch's TCP store binds every interface whatever address it is given (found live
  2026-09-23, M08).
- **A listener check.** ADR 0031 §7 lists the TCP listening sockets of a launch's owned
  processes and stops the launch on any address other than `127.0.0.1` or `::1`. It runs
  for custom engines and LMCache servers only
  (`crates/capyctl-adapters/src/custom/listeners.rs:113-220`,
  `crates/capyctl-adapters/src/lmcache.rs:547-580`).
- **Ownership.** A launch owns its process group; at readiness the leader is `api`, its
  children `worker-N`, everything else `helper-N`, and only `api` and `worker-N` exits are
  engine exits (ADR 0027). A stop signals the whole group and releases only once every
  recorded process is gone and the group is empty
  (`crates/capyctl-launchers/src/owned_launch.rs:252-270,341-490`).
- **Parking evidence.** vLLM parks on its engine-wide `/sleep` and `/is_sleeping` answers
  and the measured residue (`crates/capyctl-adapters/src/vllm/residency.rs:124-155`).
  SGLang parks on the saver map of its one scheduler process; the Python side pins the
  topology to TP 1 (`runtime/sglang_saver_residency.py:52-55,75-106`) and the controller's
  observer reports a map spanning several devices as unknown, on purpose: "multi-rank
  runtimes fail closed until per-rank evidence is implemented"
  (`crates/capyctl-controller/src/sglang_observer.rs:118-146`).
- **Engine flags.** The vLLM frozen plan pins `tensor_parallel_size` 1
  (`crates/capyctl-adapters/src/vllm/frozen.rs:178-182`), SGLang's server args pin
  `tp_size`, `dp_size` and `pp_size` to 1 (`runtime/sglang_server_args.py:82-88`), and
  every alias of them is reserved (`crates/capyctl-config/src/engine_policy.rs:150-223`).
  llama.cpp's `--device`, `--split-mode`, `--tensor-split` and `--main-gpu` are reserved and
  never rendered (ADR 0029 §6).

### Hardware we have

Host A and host B are GB10 machines: one integrated GPU each, unified memory. The
discrete-GPU laptop has one 16 GB card. None of them has two GPUs, so nothing in this ADR
can be qualified on hardware the project has today; see §14 and owner decision 2. CPU and
Fake-engine tests pin configuration, accounting and lifecycle; they are not qualification.

### What was read

Each engine's public source at the pinned version, on GitHub (no engine was run and no host
was used):

- vLLM [v0.32.0](https://github.com/vllm-project/vllm/tree/v0.32.0), with the single-node
  rendezvous checked identical in
  [v0.30.0](https://github.com/vllm-project/vllm/blob/v0.30.0/vllm/v1/executor/multiproc_executor.py#L138-L143)
  and [v0.31.0](https://github.com/vllm-project/vllm/blob/v0.31.0/vllm/v1/executor/multiproc_executor.py#L138-L143).
  Paths below are under `vllm/`.
- SGLang [v0.5.22](https://github.com/sgl-project/sglang/tree/v0.5.22). Paths are under
  `python/sglang/srt/`. SGLang 0.5.21 was not re-read for this ADR.
- llama.cpp [b11669](https://github.com/ggml-org/llama.cpp/tree/b11669), the commit of the
  approved v0.7.0 build. The v0.6.0 build was not read.
- NCCL [master](https://github.com/NVIDIA/nccl) (`src/init.cc`, `src/bootstrap.cc`) for
  what a communicator opens.

TensorFold was not read; ADR 0028 §10 records that it runs `--tp 1` or `2` with one GPU
per process and a TCP master port.

## Evidence

### 1. vLLM on one host

- **Processes.** With a tensor-parallel size above 1 on one node, the multiprocessing
  executor starts one worker process per local rank (`v1/executor/multiproc_executor.py:190-215`).
  They are children of the engine core, so under ADR 0027 they are `helper-N`, not
  `worker-N` (ADR 0027, Consequences).
- **Rendezvous.** On CUDA the executor uses a file store,
  `file://<tempdir>/vllm_dist_<uuid>` (`multiproc_executor.py:138-143`,
  `utils/network_utils.py:153-154`); a TCP store on loopback is used only for ROCm's AITER
  all-reduce (`network_utils.py:157-165`). The scheduler-to-worker broadcast queue uses
  shared memory and a ZeroMQ `ipc://` socket for local readers, and binds TCP only when a
  reader is on another node (`distributed/device_communicators/shm_broadcast.py:526-533,551-562`).
  The `ipc://` paths are under `VLLM_RPC_BASE_PATH` (`network_utils.py:175-177`).
- **Collectives.** Workers form NCCL and gloo groups. NCCL's bootstrap listens on a TCP
  socket of its bootstrap interface on every rank (`src/bootstrap.cc:408-416,550-555`);
  `NCCL_SOCKET_IFNAME` chooses that interface
  ([NCCL environment variables](https://docs.nvidia.com/deeplearning/nccl/user-guide/docs/env.html#nccl-socket-ifname)).
  Gloo binds the interface `GLOO_SOCKET_IFNAME` names. CapyCTL's loopback pin sets both to
  `lo`.
- **Devices.** `CUDA_VISIBLE_DEVICES` may list UUIDs; vLLM maps a UUID to its NVML index
  (`platforms/cuda.py:751-756`), so the UUID pin of ADR 0019 §6 extends to a list.
- **Sleep.** Sleep is a collective call to every worker (`v1/executor/abstract.py:382-389`)
  and the sleep-mode guide says it "works with tensor parallelism, pipeline parallelism"
  ([docs/features/sleep_mode.md](https://github.com/vllm-project/vllm/blob/v0.32.0/docs/features/sleep_mode.md)).
  CapyCTL's deep wake reloads weights through a collective RPC, which also reaches every
  worker. ADR 0028's MN4 rows passed 5/5 deep park cycles of vLLM 0.30.0 at TP 2 across two
  hosts with the same calls.
- **A rank dying.** The executor waits on every worker's process sentinel; when one dies it
  logs it, shuts the executor down and calls the engine's failure callback
  (`multiproc_executor.py:299-333`). The engine core is CapyCTL's `worker-0`, so its exit is
  seen; that it exits promptly after a worker's death is [INFERENCE] from the callback, not
  observed.

### 2. SGLang on one host

- **Processes.** The launcher starts one scheduler process per TP rank
  (`entrypoints/engine.py:916-956`); they are children of the server process, so they are
  already `worker-N` under ADR 0027.
- **Rendezvous and sockets.** Without DP attention, every inter-process channel is an
  `ipc://` file (`server_args.py:790-802`) and `torch.distributed` uses
  `SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE` when set (`distributed/bootstrap.py:227-242`),
  which CapyCTL already sets to a file store. With DP attention the channels become TCP on
  `127.0.0.1` at ports derived from the server port (`server_args.py:803-893`), which
  CapyCTL's leased port ranges do not reserve.
- **Release and resume.** Every TP rank runs `release_memory_occupation` and
  `resume_memory_occupation` itself, with barriers on the TP CPU group around the weights
  (`managers/scheduler_components/weight_updater.py:337-381,383-406`); each rank's memory
  saver pauses and resumes its own device. ADR 0028's MN4 rows passed SGLang 0.5.21 group
  deep park and wake at TP 2 after the 2026-10-16 fix.
- **A rank dying.** A scheduler that raises sends SIGQUIT to the server, whose handler
  kills the process tree (`entrypoints/engine.py:1941-1951`). A scheduler killed by a signal
  sends nothing; CapyCTL's exit watcher sees a `worker-N` gone.

### 3. llama.cpp on one host

- One process uses every visible GPU. `--split-mode` is `none`, `layer` (the default: layers
  and their KV split across GPUs, run as a pipeline), `row` (weights split by rows, the
  main GPU holding intermediate results and KV) or `tensor` ("split weights and KV across
  GPUs (parallelized, EXPERIMENTAL)") (`common/arg.cpp:2820-2843`, `common/common.h:490`).
  `--tensor-split` gives each GPU's proportion (`arg.cpp:2844-2846`), `--main-gpu` the main
  GPU (`arg.cpp:2871-2873`), `--device` the device list (`arg.cpp:2741-2744`).
- CapyCTL's single-GPU pin works today because the engine sees one GPU; with several
  visible GPUs and no flags it would split by layer across all of them.
- The `tensor` mode's all-reduce uses NCCL when the build links it
  (`ggml/CMakeLists.txt:211`, `GGML_CUDA_NCCL` defaults on and is used only if NCCL is found;
  `ggml/src/ggml-cuda/ggml-cuda.cu:1004-1027`). It creates its communicators with
  `ncclCommInitAll` (`ggml-cuda.cu:1217-1230`), which calls `ncclGetUniqueId`
  (`nccl/src/init.cc:2973-3017`), whose bootstrap root listens on a TCP socket
  (`bootstrap.cc:433-459,408-416`). So even a single-process llama.cpp opens an NCCL
  listener in `tensor` mode, on whatever interface NCCL picks unless `NCCL_SOCKET_IFNAME`
  says otherwise. Whether the approved v0.7.0 build linked NCCL was not checked.
- llama.cpp has no sleep; it stays `restart_only` (SPEC §9.4).

### 4. TensorFold and custom engines

TensorFold 0.6.5 runs one process per GPU, `--tp 2` at most, and its ranks meet at a TCP
master port (ADR 0028 §10). [INFERENCE] Its store would bind every interface as torch's
does, and it has no file-store option CapyCTL knows of. TensorFold 1.0 is its own change for
0.1.9; its support data says whether it runs several GPUs of one host. A custom engine runs
one process CapyCTL cannot configure (ADR 0031) and stays on one GPU.

### Summary

| | vLLM 0.30–0.32 | SGLang 0.5.22 | llama.cpp v0.7.0 | TensorFold 0.6.x | custom |
|---|---|---|---|---|---|
| Several GPUs, one host | `--tensor-parallel-size N` | `--tp-size N` | `--split-mode layer` + `--tensor-split` | not in this ADR | no |
| Processes | one per rank, children of the engine core | one per rank, children of the server | one | one per rank | one |
| Rendezvous | file store (CUDA) | file store (CapyCTL's override) | none (NCCL only in `tensor` mode) | TCP master port | — |
| Off-loopback listeners with CapyCTL's pin | none expected | none expected without DP attention | none in `layer` mode | — | — |
| Deep park at TP > 1 | yes, collective sleep | yes, every rank releases | no | no | no |

## Decision

### 1. Configuration: `topology` on one host, `devices` to pin

A deployment asks for several GPUs with ADR 0028's existing field, without naming hosts:

```yaml
name: qwen3-32b
engine: vllm
model: Qwen3-32B
topology: {tensor_parallel: 2}
```

- `topology.tensor_parallel: N` with no `placement.hosts` (or with `host:`,
  `placement.selector`, `strategy`, `max_per_host`, which stay allowed) is **one instance on
  N GPUs of one host**. With two or more `placement.hosts` it is an ADR 0028 group,
  unchanged, still one device per host.
- `pipeline_parallel` above 1 on one host is refused `group_shape_unsupported:<engine>` in
  0.1.9 (owner decision 9).
- `devices` may pin the GPUs: exactly N distinct entries (`devices: [{id: gpu0}, {id:
  gpu1}]`), else `invalid_config` (`topology_devices_mismatch`, exit 2). A deployment that
  names two devices without a `topology` is still refused `multi_gpu_unsupported`, now with
  the hint to add `topology: {tensor_parallel: 2}`. `sharing` keeps its meaning per device.
- `instances` above 1 is allowed: each instance places on its own N GPUs.
- Standalone accepts it (it is one host).
- Refused, with `multi_gpu_unsupported` (exit 5) and the reason: a unified-memory host (it has
  one GPU domain; ADR 0019 §2); an engine whose support data has no local multi-GPU mode
  (TensorFold until its own change says otherwise, custom engines). A decisions deployment
  keeps ADR 0030 §7 item 1's refusal of any `topology`, and an embeddings deployment ADR
  0036's, each with its own code.
- The N GPUs must be the same product with the same total memory (within 64 MiB, the
  tolerance of the ADR 0019 sample); a host whose candidate GPUs differ is not eligible, and
  pinned GPUs that differ are refused `multi_gpu_mixed_devices` (exit 5) (owner decision 4).
- For vLLM and SGLang, N must divide the model's attention heads (`num_attention_heads` in
  its `config.json`): vLLM refuses any other N at start (`config/model.py:1532-1549` at
  v0.32.0), and SGLang's layers split heads the same way [INFERENCE: its check was not
  located]. CapyCTL refuses such an N at resolution, `tensor_parallel_indivisible` (exit 5),
  naming the head count and the sizes that divide it. llama.cpp's layer split has no such
  rule.
- `tensor_parallel` must be at most the host's number of discrete GPUs; a deployment no
  allowed host can hold is refused at placement `insufficient_gpus` (exit 4, naming the
  count needed and each host's count).
- Engine flags stay reserved and are rendered from `topology`: vLLM `--tensor-parallel-size`,
  `--distributed-executor-backend mp` (rendered explicitly, so a Ray backend, which opens its
  own listeners, is never chosen); SGLang `tp_size` (and `base_gpu_id: 0`, `gpu_id_step: 1`);
  SGLang `enable_dp_attention`, `dp_size` and `ep_size` stay refused on one host in this
  version (§1 of the evidence: their TCP ports are derived, not leased). llama.cpp: §4.

`capyctl validate config` and `deploy` show the GPUs and the per-GPU charge. Every refusal
for memory or GPU count names what would fit: when a deployment with an explicit
`tensor_parallel` (or none) is refused `insufficient_device_memory` or `insufficient_gpus`,
the hint names the smallest tensor-parallel size whose per-GPU charge fits the host's cards
and how many GPUs that needs, or says that no size up to the host's GPU count fits and
gives the per-GPU charge at that count. CapyCTL chooses the size itself only when the
deployment says `topology: auto` (§1a).

### 1a. `topology: auto` (owner decision 10)

```yaml
name: qwen3-30b-a3b
engine: sglang
model: Qwen3-30B-A3B
topology: auto
```

- **Meaning.** One host, identical GPUs, the smallest tensor-parallel size N that fits.
  `auto` is a value of `topology` as a whole (pipeline parallel stays 1). It is refused
  beside `placement.hosts` with two or more hosts (`group_topology_invalid`: a group's shape
  is the operator's), beside `devices` (`topology_devices_mismatch`: to pin GPUs, state N),
  and on decision and embeddings deployments by their own rules.
- **Candidates.** Resolution records, in the revision, the candidate table: for each N from
  1 up to the largest discrete GPU count among the allowed hosts, skipping an N the engine
  cannot run (above its support data's maximum, not dividing the attention heads, any N > 1
  for an engine with no local multi-GPU mode), the per-GPU and host-RAM phases derived with
  `T = N` by §3. A unified host, TensorFold and custom engines have N = 1 only, so `auto`
  there is today's single-GPU deployment. A requirement only N = 1 can meet narrows the
  table to N = 1: a declared `deep` or `host_backed` while the engine's N > 1 parking gate
  is closed (§9). (`lmcache: required` narrowed it to N = 1 too until [ADR 0034](0034-lmcache-by-default.md) §6
  made a model on several GPUs run LMCache uncertified under `required`, implemented
  2026-10-28.)
- **Choice at placement, on capacity.** On each candidate host, N is the smallest candidate
  for which the host has N identical eligible GPUs whose managed limits each hold the
  per-GPU charge of every phase (the cold phase included: a measured peak for that N when
  one is recorded, else the placeholder). N is chosen from the cards' limits, never from
  their current free memory, so the same deployment on the same host and cards gets the same
  N whatever else is running: a busy card leads to eviction at that N (§2), never to a larger
  N, and an idle second card does not tempt a smaller model onto two. Then §2 picks the GPU
  set for that N and §3 charges it. Weights not yet measured are not guessed: a host chooses
  N only once the checkpoint is measured (ADR 0014 §7).
- **Frozen per launch.** The chosen N and GPU set are recorded with the instance and the
  launch plan and hold for the launch's whole life, parks and wakes included (a parked
  instance's residue is on those GPUs). The engine command is rendered from that N exactly as
  for an explicit `tensor_parallel: N` (§4): the same flags, the same device list, the same
  frozen-plan pins and `tp_drift` check; with N = 1 it is today's single-GPU command. The
  charge is that N's row of the candidate table; residency, LMCache and the listener check
  follow that N as they would for an explicit N. A later start after a stop chooses again,
  and gets the same N unless the host's cards, its limits or a measured figure changed; when
  a measured peak no longer fits N, the next start takes a larger N and journals
  `topology_auto_changed` with both sizes.
- **Measured figures** are kept per N (§3), so a peak measured at N = 2 never sizes a
  start at N = 1.
- **Status.** `status deployment` shows `TP auto (2)` for a running or parked instance
  chosen at 2, and `TP auto` for one not placed; the JSON gives `topology: {requested:
  "auto", tensor_parallel: 2, devices: ["gpu0", "gpu1"]}` per instance. `validate config
  --host` and `deploy` print the candidate table and the N the host would choose now.
- **Refusals still name what would fit.** When no candidate fits any allowed host, `auto` is
  refused `insufficient_device_memory` with the per-GPU charge at the largest N a host offers
  and the number of such cards that would hold it, or `insufficient_gpus` when only a host
  with more identical GPUs would.

### 2. Placement: a set of GPUs

ADR 0019 §6 evaluates (host, GPU) pairs; for N > 1 placement evaluates (host, set of N GPUs):

- Without a pin, the candidate sets are the host's N-subsets of eligible GPUs (at most
  C(8, 4) = 70 on an eight-GPU host). A set fits when every GPU in it holds the per-rank
  charge (§3). Among fitting sets, placement takes the one whose smallest headroom is
  largest, then the lowest indices; a stopped instance's last set wins when it fits. Under
  `auto`, N is fixed first (§1a) and the set is chosen the same way.
- When none fits, the switch planner computes victims per GPU as today and takes the set
  whose union of minimal victim sets is smallest, then least recently used, then the lowest
  indices. Only owners charged on those GPUs' domains are candidates. A victim on several
  GPUs (another multi-GPU instance) is parked or stopped whole.
- The chosen set is recorded with the instance in rank order (rank r on the r-th GPU), in
  the store beside today's chosen GPU (a new schema version, forward-only).
- GPU interconnect (NVLink against PCIe) is not considered in this version; placement does
  not read `nvidia-smi topo`.

### 3. Accounting: one owner, N device allocations

The instance stays one owner, `deployment:<id>/instance:<k>`, whose footprint has one
allocation per chosen GPU's `device` domain plus the host's `system` domain. ADR 0007's
all-domain admission checks them together, so reservation is all or nothing across the GPUs
without any new transaction.

- **Per GPU (derived phases).** Each GPU is charged one rank: its share of the weights by
  ADR 0028's formula with `T = N`, `P = 1` (`ceil(sharded / N) + replicated`, from the
  checkpoint's layout), its KV cache, the margin (share × 0.10), the CUDA context and graph
  allowance (1.25 GiB, ADR 0019 §3) and a communication allowance for NCCL and
  custom-all-reduce buffers and the contexts a rank opens on its peers' GPUs (placeholder
  1 GiB per GPU, labelled `derived` until MG1 and MG2 measure it). The startup placeholder
  is `share × 2.25 + margin` per GPU, as for a group member. vLLM's
  `--gpu-memory-utilization` and `--kv-cache-memory-bytes`, and SGLang's static fraction, are
  per rank already (ADR 0028, amendment of 2026-10-07), so they render from one GPU's grant.
- **Host RAM.** The engine host overhead becomes 4 GiB plus 3 GiB for each rank after the
  first (placeholder, `derived`): each rank is a Python process that imports torch and the
  engine. [INFERENCE: about 2–3 GiB resident each; MG1 and MG2 measure it.] A
  `host_backed` copy is the sum of the ranks' shares × 1.5, charged as today. The system
  domain's limits are the operator's on an enrolled host and settable on standalone
  (ADR 0025), unchanged; a deployment that does not fit them is refused with today's codes.
- **Declared `resources`.** A deployment with `topology` that declares `resources` names
  each GPU's domain or uses the short form `{gpu, ram}`, where `gpu` is per GPU and `ram` is
  for the whole instance. A declared `memory.request` is per GPU. Beside `topology: auto`,
  `resources`, `memory.request` and `memory.startup` are refused (`invalid_config`): the
  charge is derived for each candidate N, and a fixed per-GPU figure would mean something
  different at each N. A declared `memory.kv_cache` is per GPU and is used for every N.
- **Measured figures.** ADR 0019 amendment A2 measures each domain's startup peak from the
  launch's own processes; with N GPUs the peak recorded is the largest any of the chosen
  GPUs showed, stored once per revision, host, installation and N as the per-GPU device
  peak (the GPUs are identical, §1) and applied to every GPU of a later start at that N,
  whichever set it lands on. The parked residue (ADR 0014 A13) is recorded the same way, per
  GPU and N, and A19's growth bound applies per GPU against that GPU's
  `parked_growth_limit`. A record from before this ADR is the N = 1 record.
- **Parked limits.** A parked multi-GPU instance counts against each of its GPUs'
  `parked_limit` with that GPU's residue, and once in `max_parked`. Until a park at N
  measures it, each GPU's parked residue is charged the context-and-graph and
  communication allowances above (2.25 GiB), what a sleeping rank keeps on its GPU
  (implementation note, 2026-10-25); one GPU keeps its 1 GiB placeholder.

### 4. Launch per engine

| | vLLM | SGLang | llama.cpp |
|---|---|---|---|
| Devices | `CUDA_VISIBLE_DEVICES=<uuid0>,<uuid1>,...` in rank order (index list with `CUDA_DEVICE_ORDER=PCI_BUS_ID` when UUIDs are not published, as ADR 0019 §6) | same | same |
| Parallel size | `--tensor-parallel-size N --distributed-executor-backend mp` | `tp_size=N`, `base_gpu_id=0`, `gpu_id_step=1` | `--split-mode layer --tensor-split <s0>,...,<sN-1> --main-gpu 0` |
| Rendezvous | its own file store; the loopback pin | CapyCTL's file store override; the loopback pin | none |
| Sizing | per-rank utilization and KV bytes from one GPU's grant | static fraction of one GPU's total | `--tensor-split` from the per-GPU grants (equal when the GPUs are) |

- The loopback pin of `runtime/loopback_rendezvous.py` applies unchanged: a model on several
  GPUs of one host is a single-host launch, never a group. `GLOO_SOCKET_IFNAME=lo` and
  `NCCL_SOCKET_IFNAME=lo` keep every collective listener on loopback; no rendezvous port is
  leased because none is opened. For llama.cpp with N > 1, the launch environment gains
  `NCCL_SOCKET_IFNAME=lo`, so a build that links NCCL stays on loopback if a later version
  opens the `tensor` mode.
- The protected entries take N: `runtime/vllm_entry.py`'s frozen plan pins
  `tensor_parallel_size` to the plan's N instead of 1, and `runtime/sglang_server_args.py`
  pins `tp_size` to N; any drift refuses the launch, as the group mode does
  (`group_drift:<field>`; for a single-host launch `tp_drift:<field>`).
- llama.cpp's `--split-mode` is rendered `layer`; `row` and `tensor` stay reserved (owner
  decision 5). `--fit` and the context fit work from the per-GPU grants;
  MG3 checks that llama.cpp's own fit agrees.

### 5. Listeners: checked, not assumed

The evidence above says no engine opens a listener off loopback in these shapes, but that is
read from source and depends on build options and environment. So every launch with N > 1
runs ADR 0031 §7's listener check, the existing code, on its owned processes once readiness
passes and whenever a host re-proves the launch: a TCP listener on any address other than
`127.0.0.1` or `::1` stops the launch, with gone evidence required, and fails it
`engine_listener_exposed`, naming the process role, address and port. As in ADR 0031, a
listener opened and closed before the check, or opened later, is not seen: this narrows the
exposure, it does not prove it absent.

Loopback listeners stay: NCCL's bootstrap and proxy sockets and gloo's pair sockets on
`127.0.0.1`. They are unauthenticated, and gloo's object collectives carry pickled objects
(ADR 0028 §13). [INFERENCE] A local user who finds and connects to one could disturb or
crash the instance, and possibly run code as the engine's user, as with the unauthenticated
TensorFold and llama.cpp APIs on loopback. That matches SPEC §9.1's line (controls never
leave loopback; local users are not a boundary CapyCTL claims) and ADR 0012's protections are
unchanged: the engine's API listener stays loopback-only, keyed per launch and behind the
key guard, and no engine control path goes through ingress or the router. Status shows
`collectives: loopback` for such an instance, and the guide says so (owner decision 7).

### 6. Ownership: rank processes are engine processes

- **Ranks named at readiness.** At readiness, on a launch with N > 1, the host matches its
  process group against a fresh per-process, per-GPU sample (§8): for each chosen GPU, the
  owned process holding the most memory on it is that GPU's rank process. A rank process
  that is `api` or `worker-N` keeps its role and records its rank; any other gets the role
  `rank-N` (vLLM's workers, which ADR 0027 now calls helpers). Readiness waits until each
  chosen GPU has one rank process (bounded by the initialize timeout), as an ADR 0028 worker
  waits for a process per local rank (amendment of 2026-10-15). A process that would be the
  rank of two GPUs is no evidence; the launch stays unready and fails at the timeout with
  `tp_rank_unobserved`.
- **Engine processes.** `rank-N` joins `api` and `worker-N` in `engine_members` and
  `same_engine` (ADR 0027 §2): the exit watchers judge it, and every later liveness proof
  (re-proof after a restart or session loss, park, wake, residency claims) requires it alive.
  Helpers keep ADR 0027's rule.
- **Kernel facts, not names.** Ranks are identified by the driver's attribution of memory
  to process identities (PID, boot id, start ticks), never by process titles the engine sets.
- **Remote hosts.** The host's own observation names the ranks, as it names helpers today;
  the server reads them. A host without the capability of §11 receives no N > 1 launch.

### 7. Failure of one rank

- **A rank exits.** That is an engine exit (§6): the watcher closes dispatch and stops the
  instance's whole process group, which reads `failed` with the event naming the rank and its
  GPU (`engine_rank_exited`). The reservation on every GPU is released only once every
  recorded process is gone and the group is empty; a GPU whose process cannot be proven gone
  keeps the whole footprint charged. No rank is replaced or restarted alone: an NCCL group
  cannot re-admit a rank. Under `recovery: reconcile` the instance relaunches whole, as
  today.
  If several recorded ranks are observed gone in one scan, the event names all
  of them and explicitly says the exit order is unknown. A sampled observation
  does not establish which rank initiated a cascading engine failure. The
  report carries the observed process identities together so an API-process
  exit cannot hide rank exits from the same scan; no exit status is inferred
  for an additional process.
- **A rank hangs.** A rank stuck in a collective (a GPU fault, a deadlock) does not exit and
  the other ranks wait for it. ADR 0028 §11's stall rule applies to every instance with
  N > 1: a request without a first token within the stall timeout (`groups.stall_timeout`,
  default 120 s) triggers one bounded probe through the instance's own loopback endpoint; if
  it fails too, the instance fails `tp_stalled` (status only) and is stopped. An idle instance
  is never probed (owner decision 8).
- **A GPU disappears from the sample.** Its domain is `device_unobserved` (ADR 0019 §4):
  admission closes on it and every charge stays; the instance itself fails only through
  the two rules above.
- **Shared memory.** NCCL's SHM transport and vLLM's broadcast queue use `/dev/shm`. SGLang's
  entry already removes the files a stopped launch's processes held (status runbook,
  2026-10-23); vLLM has no such cleanup. MG7 checks `/dev/shm` after a normal stop and after a
  rank is killed, and a cleanup for vLLM is added only if a file is found.

### 8. Per-process readings per GPU

- The collector asks `--query-compute-apps=pid,gpu_uuid,used_memory` and keeps one reading
  per (process, GPU). A row whose UUID is not in the host's inventory refuses the whole
  sample, as a malformed row does today.
- A process's device bytes are credited to the domain of the GPU they are on, not to the
  instance's chosen device; on a single-GPU launch this is the same figure as today. The
  resident credit of a domain is the sum over the owner's recorded identities of their bytes
  on that GPU, capped at the owner's allocation there (ADR 0007).
- Wire: `ProcessResidency` gains a repeated per-device entry (device id, bytes) beside
  `device_bytes`, which stays their sum; additive field numbers, under the capability of §11.

### 9. Parking

- **vLLM and SGLang: `deep` and `host_backed` at N > 1** use the same calls as one GPU (the
  collective sleep, wake and reload; SGLang's release, resume and reload, which every rank
  runs). The evidence becomes per GPU:
  - vLLM: after the engine's reply, each chosen GPU's bytes held by the launch's processes
    (§8) are at or below that GPU's parked budget for a park, and above it for a wake; the
    residue is recorded per GPU (§3). A GPU with no fresh reading leaves the park unsettled
    and the full charge kept.
  - SGLang: each scheduler process enrols its saver map, as the single scheduler does today.
    The topology check (`runtime/sglang_saver_residency.py`) accepts `tp_size = N` for a
    launch whose plan says N; the observer replaces its `single_device` gate with a
    per-device judgment: the park settles only when, on every chosen GPU, the weights and
    KV regions are unmapped; the wake only when they are mapped on every GPU. Any GPU missing
    or disagreeing reports unknown, which fails closed as today.
  - The wake canary and the commit order of ADR 0028's note of 2026-10-16 apply unchanged.
- **Gate.** Deep parking at N > 1 is built now and enabled per engine in its support data
  (§10) only once that engine's row passes (MG4 for vLLM, MG5 for SGLang), both run in the
  rental session of §14 (owner decision 3). Until then a multi-GPU deployment that states
  no residency resolves `restart_only`, and one that states `deep` or `host_backed` is
  refused `capability_missing:deep_park` with the hint to declare `restart_only`, the rule
  ADR 0028 used for SGLang groups on 2026-10-15. Under `topology: auto` the residency that
  states none follows the chosen N per launch, and a stated `deep` or `host_backed` narrows
  the candidates to N = 1 instead of being refused (§1a).
- **llama.cpp, TensorFold, custom:** `restart_only`, unchanged.
- **LMCache.** LMCache stays off for N > 1 with the reason `multi_gpu_unqualified` until an
  LC row qualifies it (ADR 0034 owns the rule and the row). *Amended 2026-10-28 by
  [ADR 0034](0034-lmcache-by-default.md) §6:* that holds under `lmcache: auto`; under `lmcache: required` the
  instance runs LMCache, its ranks connected to its one server, as an uncertified setup.

### 10. Support data

One function per engine, beside `group_support` (`crates/capyctl-config/src/group_support.rs`),
declares the local multi-GPU shape: `max_tensor_parallel` (none for vLLM and SGLang, which
accept any N up to the host's GPU count; llama.cpp the same), `pipeline` (false in this
version), `deep_park` (false until MG4/MG5 pass, §9) and `split_modes` (llama.cpp:
`layer`). TensorFold and custom engines have no entry. The configuration layer reads it for
§1's checks, as ADR 0028 §10's `GroupSupport` is read.

### 11. Protocol and capability

A new ADR 0017 capability, `local_tensor_parallel` (server to host), covers: a launch plan
carrying a device list instead of one device (new field), the per-device process readings
(§8), the `rank-N` role (§6), and the listener check on built-in engines (§5). A host without
it is refused `host_capability_missing:local_tensor_parallel` for an N > 1 launch before
anything is sent; any other launch needs nothing new. `PROTOCOL_VERSION` and the command
encoding version do not change.

### 12. Relation to groups (ADR 0028)

A model on several GPUs of one host is not a group: no group plan, no peer address, no
rendezvous port, no `engine_groups` capability, no `peer transport: unauthenticated` mark,
and the loopback pin applies. Groups keep `local_ranks == 1`. The pieces built here (a
device list per launch, per-GPU accounting and readings, rank processes) are what a later
amendment would need to run several GPUs per member (for example TP 4 as two hosts with two
GPUs each); that amendment is out of scope.

### 13. Codes

| Where | Code | When | Exit |
|---|---|---|---|
| resolution | `multi_gpu_unsupported` (narrowed) | two devices without `topology`; a unified host; an engine without a local multi-GPU mode | 5 |
| resolution | `topology_devices_mismatch` | `devices` pins a count other than `tensor_parallel`, or any `devices` beside `topology: auto` | 2 |
| resolution | `multi_gpu_mixed_devices` | pinned GPUs of different products or memory | 5 |
| resolution | `tensor_parallel_indivisible` | vLLM or SGLang with an N that does not divide the model's attention heads | 5 |
| resolution | `group_topology_invalid` | `topology: auto` beside two or more `placement.hosts` | 2 |
| resolution | `invalid_config` | `resources`, `memory.request` or `memory.startup` beside `topology: auto` | 2 |
| resolution | `group_shape_unsupported:<engine>` | `pipeline_parallel` > 1 on one host | 5 |
| placement | `insufficient_gpus` | no allowed host has enough identical discrete GPUs (the hint names the N that would fit) | 4 |
| placement | `insufficient_device_memory` (existing) | no N up to a host's GPU count holds the per-GPU charge (the hint names the charge and the cards that would hold it) | 4 |
| placement | `host_capability_missing:local_tensor_parallel` | an older host | placement refusal |
| launch | `tp_drift:<field>` | the engine's parsed tensor-parallel size or devices differ from the plan | launch refusal |
| launch | `engine_listener_exposed` | an owned process listens off loopback (§5) | launch failure |
| launch | `tp_rank_unobserved` | no distinct rank process per GPU by the initialize timeout | launch failure |
| status | `engine_rank_exited`, `tp_stalled` | §7 | none |
| event | `topology_auto_changed` | a start under `auto` chose a different N than the previous one (§1a) | none |

No exit number is added.

### 14. Hardware qualification (owner decision 2)

Qualification needs one Linux host with at least two identical discrete NVIDIA GPUs. The
project has none (each GB10 host has one integrated GPU; the laptop has one card), and
nothing can stand in for it: tensor parallelism needs one distinct device per rank
[INFERENCE: two ranks on one GPU were not tried and would not exercise transfers between
cards anyway], and neither GB10 nor the laptop card supports MIG.

**The machine.** When the implementation is ready, the owner rents one x86_64 machine with
two NVIDIA RTX A6000 48 GB PCIe cards and gives SSH access to it. Two 48 GB cards run ADR
0028's bring-up model, Qwen3-30B-A3B in BF16 (about 61 GB of weights), at TP 2, so MG9 can
be compared with the MN rows; PCIe without NVLink is the common home-workstation shape. A
four-GPU host is not needed: TP 2 exercises every mechanism, and larger N are allowed by
support data as groups' larger shapes were (ADR 0028, decided 2026-10-06).

**The session.** The machine is authorized as a live host for that one session only, under
AGENTS.md's hard constraints otherwise (one live session at a time; fault injection only by
signals to CapyCTL-owned processes). The engine environments on it are fresh installs of the
approved versions only: vLLM 0.32.0, SGLang 0.5.22 and the llama.cpp v0.7.0 build, their
x86_64 CUDA builds, in the home directory; their fingerprints are recorded with the rows,
since they are not byte copies of the lab hosts' aarch64 environments. The session runs live
rows MG1–MG14 (MG4 and MG5 qualify deep parking at N > 1, owner decision 3) and a download
bandwidth test (the bring-up model materialized from Hugging Face, with the throughput and
the time recorded). The machine is shut down as soon as the session ends, so it costs nothing
idle. AGENTS.md records the authorization and the environments when the session is
scheduled, with the implementation's pull request.

### 15. Texts that change with the implementation

- **SPEC** §7.2: "One GPU serves one model" becomes "one GPU, or with `topology` several
  identical GPUs of one host, serve one model; CapyCTL picks them unless the deployment pins
  them". §8.2: the tensor-parallel size and the device list are rendered. §9.1 and §9.2: deep
  parking at N > 1 per the support data. §9.4: llama.cpp's layer split. §11: a sentence that a
  model on several GPUs of one host is one single-host launch, not a group. §15.1: `topology`
  without `placement.hosts`, and `topology: auto`. §20: T46 and the MG rows.
- **ADR 0019** §6 and its code table, **ADR 0027** §1–§2, **ADR 0028** §2, **ADR 0029** §6:
  amendments recording §1, §6, §1 and §4 above.
- **Guide:** `docs/guide/deploy.md` (`topology` on one machine beside `devices`, and
  `topology: auto`), a section "One model across several GPUs" in
  `docs/operations/install.md`'s "Discrete NVIDIA GPUs" (the per-GPU charge, how `auto`
  chooses, `collectives: loopback`, what parks), `docs/guide/errors.md` and the exit table
  in `install.md` (the codes of §13), `docs/guide/parking.md` (what parks at N > 1),
  `docs/guide/several-machines.md` (a pointer: one machine with several GPUs needs no
  group), `docs/operations/configuration.md` (`topology` on one host and `auto`;
  `groups.stall_timeout` also covers it).

## Consequences

- A workstation with two or more identical cards serves a model larger than one card,
  through one route, with each card accounted on its own domain and the engines' collectives
  kept on loopback, which is safer than the group alternative.
- Readings of a process's GPU memory become per GPU everywhere, which also makes a
  single-GPU launch's credit independent of which GPU a stray context lands on.
- vLLM's worker processes become engine processes for a multi-GPU launch, so CapyCTL sees a
  rank die directly instead of through the engine core.
- The placeholders for host RAM per rank and for the communication buffers are estimates
  until measured; until then a multi-GPU deployment may be refused or overcharged on a
  machine where it would fit.
- Loopback collective listeners, unauthenticated, exist while a multi-GPU instance runs;
  they are reachable by local users.
- `topology: auto` lets a deployment written once run on one card or several, and the same
  deployment on the same host and cards always gets the same N. It also means the engine
  command, the charge and the parking tier can differ between two hosts, or after a
  measurement, for one revision; status and the `topology_auto_changed` event say so.
- Nothing ships qualified without the rented two-GPU session. CPU and Fake-engine tests are
  not qualification.

## Alternatives considered

- **A new `gpus: N` field.** Friendlier for llama.cpp, whose split is not tensor parallel,
  but it would be a second spelling of what `topology.tensor_parallel` already means for
  groups, and every check would need to reconcile the two. Rejected (owner decision 1).
- **Pass `--tensor-parallel-size` through `extra_args`.** The engine would use GPUs CapyCTL
  neither placed nor charged. Rejected: SPEC §8.2 makes devices and ranks CapyCTL's.
- **Run single-host TP as a one-host group.** Reuses ADR 0028's plumbing, but a group drops
  the loopback pin, opens peer listeners on every interface and needs a peer address and a
  rendezvous port for no reason. Rejected.
- **Charge the whole model to one GPU and leave the others unaccounted.** Simple, and wrong:
  the planner would place a second model on a card the first one fills.
- **Rely on the engine to report a rank's death.** vLLM does shut down, SGLang usually does,
  but CapyCTL's rule is to act on its own kernel evidence (ADR 0027). Rejected.
- **Restart-only for every multi-GPU deployment.** Simplest and safe, but vLLM and SGLang
  both support it and the groups already park at TP 2; it would make multi-GPU models the
  slowest to switch on exactly the machines that have room to park. Kept only as the state
  until MG4/MG5 pass.
- **A heterogeneous split (a 24 GB and a 12 GB card).** vLLM and SGLang take one token
  capacity for every rank, the smallest, so the larger card's extra memory is wasted, and
  placement and the measured peaks would need per-GPU figures. Rejected for 0.1.9 (owner
  decision 4).
- **`auto` chosen from free memory.** Picking the smallest N that fits the cards as they are
  now would make N depend on what else runs: a busy card would push a model onto two, and
  the measured peaks and parked residue would follow N around. Rejected: `auto` chooses on
  the cards' limits, and eviction makes the room (§1a).
- **Never choosing N (the recommendation of the proposal).** Overruled by owner decision 10;
  a refusal still names the N that would fit.

## Implementation plan (target 0.1.9)

1. **Configuration** (`capyctl-config`): `topology` without hosts as one host
   (`topology.rs`, `refuse_in_standalone`), the device-count, mixed-GPU, head-count and
   engine checks, `local_support`, the narrowed `check_single_device`, reserved flags
   rendered from N; `topology: auto` and its candidate table in the revision.
2. **Placement and accounting** (`capyctl-scheduler`, `capyctl-config::derive_resources`):
   GPU sets, per-set eviction, per-GPU derived phases with ADR 0028's share, host overhead
   per rank, the per-GPU peak and residue records keyed by N, the store schema for the
   chosen N and set, `auto`'s choice on capacity and the hints naming what would fit.
3. **Readings** (`capyctl-agent::process_residency`, `capyctl-store::resident_floors`):
   `gpu_uuid` per row, credit per (process, GPU), the protocol field and capability.
4. **Launch** (`capyctl-adapters` vLLM, SGLang, llama.cpp; `runtime/vllm_entry.py`,
   `runtime/sglang_server_args.py`): device lists, N in the frozen plans, `tp_drift`,
   llama.cpp's split flags, `NCCL_SOCKET_IFNAME=lo` for llama.cpp.
5. **Ownership and failure** (`capyctl-launchers::owned_launch`, domain `same_engine`,
   exit watchers, `group_stall`): `rank-N`, readiness waiting for ranks, the listener check
   on built-in engines, the stall rule for N > 1.
6. **Parking** (vLLM residency, `runtime/sglang_saver_residency.py`, `sglang_observer.rs`):
   per-GPU evidence, kept behind `deep_park: false` until the rows pass.
7. **Status, docs, tests** (`TP auto (N)`, `collectives: loopback`), then the live rows in
   the rental session (§14).

Every task skips live checks; T46 is CPU and Fake-engine only.

## Verification

### Conformance row T46 — one model across several GPUs of one host

`topology.tensor_parallel: N` without hosts resolves to one instance on N GPUs, on an
enrolled host and on standalone; a world size of 1 keeps every revision byte-identical;
`devices` with a count other than N, pinned GPUs of different products, `pipeline_parallel`
> 1, a unified host, TensorFold, a custom engine, `api: decisions` and `api: embeddings` are
refused with their codes, and so is an N that does not divide the attention heads on vLLM
or SGLang; a host with fewer GPUs is `insufficient_gpus`. `topology: auto` records the
candidate table, chooses the smallest N whose per-GPU charge fits the cards' managed limits
(never their free memory), keeps it for the launch's life, parks and wakes included,
renders the same command as the explicit N, narrows to N = 1 under a requirement only one
GPU meets, is refused beside `devices`, named hosts or a declared per-GPU charge, and
journals `topology_auto_changed` when a later start chooses differently; every refusal for
memory or GPU count names what would fit. Placement picks the set with the largest smallest
headroom and evicts by the smallest union of victims; admission charges each GPU its rank
and the system domain the per-rank overhead, all or
nothing. The collector's per-GPU rows credit each GPU's domain separately; an unknown UUID
refuses the sample. vLLM, SGLang and llama.cpp render the device list and N, and a drift is
refused; the loopback pin holds. A launch whose owned process listens off loopback fails
`engine_listener_exposed`. Ranks are named from per-GPU readings; a rank's exit fails the
instance and nothing is released until every process is gone; a stalled request with a
failing probe fails `tp_stalled`. Deep parking at N > 1 is refused while the support data
says false, and with it true the park settles only on every GPU's evidence. An older host is
refused `host_capability_missing:local_tensor_parallel`.

### Live rows (the rented two-GPU host, §14)

| Row | What | Passes when |
|---|---|---|
| MG1 | vLLM 0.32.0, TP 2, `restart_only`: deploy, start, chat through the router | Ready; the readiness probe answers; each GPU's charge matches its card's use within the derived margin; host RAM and per-GPU startup peaks measured and recorded; no owned listener off loopback (`ss -ltnp` and the check agree) |
| MG2 | SGLang 0.5.22, TP 2: as MG1 | as MG1 |
| MG3 | llama.cpp v0.7.0, two GPUs, `layer` split | as MG1; both cards hold their part; llama.cpp's own fit agrees with the grants |
| MG4 | vLLM TP 2 `deep`: 5 park and wake cycles | every park settles on both GPUs' readings; residue recorded per GPU; every wake's canary matches |
| MG5 | SGLang TP 2 `deep`: 5 cycles | every park and wake settles on both ranks' saver maps |
| MG6 | vLLM and SGLang TP 2 `host_backed`: 3 cycles each | the copy charged and released as measured |
| MG7 | Rank failure: SIGKILL to rank 1 (PID from ownership evidence), each engine; also a normal stop | the instance fails naming rank 1; every process gone before any release; `/dev/shm` holds nothing of the launch after the normal stop (recorded after the kill) |
| MG8 | Rank hang: SIGSTOP to rank 1 during a request | `tp_stalled` after the stall timeout plus the probe; the instance stopped; nothing released early |
| MG9 | Throughput: vLLM and SGLang TP 2 through the router against the same build launched by hand with the environment CapyCTL renders | within about 10 % (ADR 0028's bar) |
| MG10 | Switching: a TP 2 model parked, a single-GPU model started on one card, then the TP 2 model requested | the planner evicts the single-GPU model and the TP 2 model wakes; the switch record names both GPUs |
| MG11 | Enrolled host: the rented machine as a host of the control-plane server; and a host without the capability | MG1 passes remotely; the older host is refused before anything is sent |
| MG12 | Refusals: TP 3 on two GPUs; pinned count mismatch | `insufficient_gpus`; `topology_devices_mismatch` |
| MG13 | `topology: auto`: Qwen3-4B on vLLM; Qwen3-30B-A3B on SGLang; then, with the 4B model serving on gpu0, the 30B model started; the 30B model parked and woken | the 4B model chooses N = 1 and its effective command equals an explicit single-GPU deployment's; the 30B model chooses N = 2 and its command equals MG5's explicit TP 2 deep launch with the same residency; status shows `TP auto (1)` and `TP auto (2)`; with gpu0 busy the 30B model still chooses 2 and the 4B model is evicted; the wake keeps N = 2 and the same GPUs |
| MG14 | Refusals name what would fit: Qwen3-30B-A3B with `tensor_parallel: 1`; the same under `auto` with a declared `memory.kv_cache` no two cards hold | `insufficient_device_memory` with the hint naming `tensor_parallel: 2`; `insufficient_device_memory` naming the per-GPU charge at N = 2 and the cards that would hold it |

All rows and the download bandwidth test run in the one rental session (§14). MG4 and MG5
flip each engine's `deep_park` in the support data when they pass (owner decision 3); an
engine whose row fails keeps `restart_only` at N > 1. MG1–MG3, MG6–MG9 and MG13 are
required before the release notes say the feature is qualified.

Owner clarification, 2026-10-26: MG13 compares identical residency settings.
MG2 uses `restart_only`, whereas MG13's park/wake requires `deep`; their
memory-saver switches must differ. MG5 supplies the explicit TP 2 deep baseline,
so the command comparison remains exact rather than ignoring that switch.

## Owner decisions of 2026-10-25

The proposal of 2026-10-24 asked ten questions; the owner answered all of them. The
decision sections above already read as decided.

1. **The field:** `topology.tensor_parallel: N` without `placement.hosts`, as proposed
   (§1). No `gpus` field.
2. **Hardware:** when the implementation is ready, the owner rents one x86_64 machine with
   two RTX A6000 48 GB PCIe cards and gives SSH access. The MG rows and a download bandwidth
   test run in one session, and the machine is shut down immediately afterwards, so it costs
   nothing idle. It is authorized as a live host for that session only, with fresh installs
   of the approved engine versions only (vLLM 0.32.0, SGLang 0.5.22, llama.cpp v0.7.0),
   x86_64 CUDA builds (§14).
3. **Deep parking at N > 1:** built now and qualified in the rental session (MG4 for vLLM,
   MG5 for SGLang). Per engine it stays off, with `restart_only` the default, until its row
   passes (§9).
4. **Different GPUs on one host:** identical GPUs only (§1).
5. **llama.cpp split modes:** `layer` only; `row` and `tensor` stay reserved (§4).
6. **Listener check:** on every launch with N > 1 (§5).
7. **Loopback collectives:** accepted and documented; unauthenticated loopback collective
   listeners are a stated property of a multi-GPU instance, and status shows
   `collectives: loopback` (§5).
8. **Stall detection:** ADR 0028's stall rule covers every instance with N > 1 (§7).
9. **Pipeline parallel on one host:** refused in 0.1.9 (§1).
10. **Choosing N (changed from the recommendation):** CapyCTL gains `topology: auto`,
    which picks the smallest N that fits on one host with identical GPUs. N is resolved at
    placement and frozen per launch; the charge is that N's row of the derived candidate
    table, the engine command is the explicit N's, status shows `TP auto (N)`, and a refusal
    still names what would fit (§1a). Live rows MG13 and MG14 cover it.
