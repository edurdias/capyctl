# CapyCTL 0.1.3

Unreleased.

## Memory

- **Short `resources`.** A model that restarts instead of parking can state
  its memory as `resources: {gpu: 11GiB, ram: 2GiB}` instead of five phases:
  CapyCTL reserves both figures while it starts, runs and stops and nothing
  once it is stopped. On a unified-memory machine the pool is charged their
  sum. The five phases are still accepted, and `capyctl validate config` shows
  what the short form stands for. See the TensorFold section of the
  [engines guide](../guide/engines.md).
- **SGLang on a discrete GPU accepts the context `status` shows.** SGLang took
  its memory fraction of the GPU memory free when it started, not of the
  card's total, so on a 16 GB laptop GPU it held about two thirds of the KV
  cache CapyCTL passed and refused inputs over 42,772 tokens where `status`
  showed 63,920. The fraction now covers that difference; SGLang still
  allocates only the KV cache and state CapyCTL sized.
- **SGLang hybrid models get their concurrency.** When you state no memory
  request, CapyCTL now adds a hybrid model's per-request state to the request it
  derives, for `max_concurrent_requests` (or 8) as far as the machine holds it,
  instead of borrowing it from the margin or the KV cache. Qwen3.8-27B at the
  default KV cache runs 8 requests where it ran 5, and asks for about 6 GiB
  more (16 GiB with DFlash2). A revision measured before this release keeps its
  sizing; a new revision of it gets the state.
- **SGLang hybrid models run 8 requests by default.** Without
  `max_concurrent_requests`, a hybrid model on SGLang runs up to 8 requests at
  once, as TensorFold does, instead of up to 32; dense models are unchanged.
  The router still accepts 32 per deployment and the rest wait in SGLang's
  queue. Set `max_concurrent_requests` to run more, and CapyCTL sizes the state
  for that count.
- **SGLang with speculative decoding parks again.** A deployment with
  `--speculative-algorithm` in its arguments parks `deep` by default instead of
  restarting: the park gives back the KV cache and keeps the weights, the
  draft model's included, so a wake reloads nothing. It frees less than a
  model without a draft model (the weights stay in memory), and the parked
  charge is measured on the first park. `residency: host_backed` is still
  refused for it; a deployment that states `restart_only` keeps restarting.
  On a 128 GB GB10, Qwen3.8-27B NVFP4 with DFlash2 parks in about a second,
  holds 33.4 GiB parked and answers about 2 s after a request wakes it. That
  is above a standalone's default parked limit (a quarter of the memory), so
  once that is measured on its first park, later parks are refused and it
  stays loaded unless the parked limit is raised (next item).
- **Standalone's parked limit is settable.** What parked models may hold
  together was fixed at a quarter of the memory. Raise it with
  `host.resource_policy.memory.system.parked_limit` in the document,
  `--set host.resource_policy.memory.system.parked_limit=40GiB` or
  `CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__PARKED_LIMIT=35%`, as
  a size or a whole percentage, up to the managed limit; a higher value is
  refused at start. `auto`, or leaving it out, keeps the quarter and the
  stored policy unchanged. See
  [configuration](configuration.md#standalone-parked-limit).

## Checkpoints

- **A checkpoint whose memory does not resolve says why.** When a model's
  memory is sized from its weights and the measured weights do not fit the
  configuration (for example a `memory.request` that leaves no KV cache),
  every start answered `checkpoint_mismatch`, which is about a digest that
  does not match, with no word of the real reason. The start is now refused
  `checkpoint_unusable` with the reason (exit 2), for example
  `the derived KV cache (request minus weights minus margin) is not positive`,
  and `capyctl status deployment <name>` shows it under `Checkpoint`. A card
  too small keeps `insufficient_device_memory` (exit 4). The host is also
  shown as refused for that revision instead of resolved. Deploy a corrected
  configuration; `checkpoint_mismatch` now means only that the digest differs.
  A revision that became unusable before this release still reads the old
  generic reason; deploy it again to see the real one.

## Standalone

- **Standalone no longer parks an SGLang ModelOpt model it cannot wake.** A
  `deep` or `host_backed` SGLang deployment with ModelOpt (NVFP4)
  quantization started in standalone, parked, and then failed to wake, since
  SGLang cannot reload those weights; the instance stayed `uncertain` until a
  stop. Standalone now refuses its start with `capability_missing:deep_park`,
  as a host agent already did, so nothing is parked. Deploy such a model
  `restart_only`. A model with speculative decoding, whose park keeps its
  weights, still parks.

## Multi-node groups

Pending live qualification: the features below pass CPU and fake-engine tests
only; the two-machine live rows have not run yet.

- **One model across machines.** A deployment with a `topology` and
  `placement.hosts` runs one engine rank per machine, head first. vLLM 0.30.0
  and SGLang 0.5.21 run tensor and pipeline parallel on any number of machines
  that divides `tensor_parallel` × `pipeline_parallel`, and park deep on every
  rank. TensorFold 0.6.5 runs `tensor_parallel: 2` on exactly two machines,
  restart only. Live qualification covers two machines. See
  [One model across machines](../guide/several-machines.md#one-model-across-machines).
- **New settings.** `--peer-address`, `--rendezvous-ports` (default
  `25000-25099`) and `--require-rdma true|false` (default `false`) on hosts,
  each also as a variable and a YAML key, and `--group-stall-timeout` (default
  `120s`) on the server ([settings](configuration.md#multi-node-groups)).
- **Engine variables.** `capyctl engine add --env` and `--approve-env`, and
  `capyctl deploy model --engine-env`, each also as a variable
  ([Engine environment](configuration.md#engine-environment)).
- **Status** lists each member with its host, rank, state and memory charged,
  and marks every group `peer transport unauthenticated`. Group peers talk on
  unauthenticated ports: keep group machines on a private direct link
  ([Network access](network-access.md#multi-node-groups)).

Catalog models that run as groups in this release, on two machines at
`tensor_parallel: 2` (all pending live qualification):

| Model | SGLang 0.5.21 | vLLM 0.30.0 | TensorFold 0.6.5 |
|---|---|---|---|
| Qwen3.8-Flash-Next | NVFP4 | NVFP4 (stock build unverified) | MLX 4-bit only |

Not yet: GLM-5.3-Flash, DeepSeek V4 Flash 0731, DeepSeek V4 Flash Vision and
MiMo-V2.5 have a working build only as a container image; MiniMax M3, Hy4
Preview and Nemotron 3 Ultra need three or more machines; Hy3 has no validated
command. Next milestones: container launchers, and a live row on three or more
rented machines.

## Fixes

- **Deployments with an engine environment start again.** A deployment
  stating `engine_config.env` was accepted and then failed to start or load
  with `CorruptStoredData`, since its stored revision lost the variables; it
  now keeps them, values stored for the launch and still shown redacted.
