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
- **SGLang starts with a small KV cache on unified memory.** On a GB10, SGLang
  refused to start ("Loaded weights leave no GPU memory for the KV cache")
  with `memory.kv_cache` of 2 GiB or less for sliding-window models such as
  Gemma 4 and gpt-oss, and for hybrid models whose arguments set
  `--max-mamba-cache-size`: their memory fraction held only the weights and
  the KV cache, with no room for what SGLang allocates while it loads. Every
  SGLang launch on unified memory now leaves 3 GiB for it (2 GiB before, and
  only when CapyCTL sized the pools) and counts the state the arguments fix,
  inside the memory request. A model whose pools SGLang sizes itself gets
  about the KV cache it states, so one that states only its memory request
  now uses more of it. gpt-oss-20b loads more than that on SGLang and still
  needs a stated `memory.request` (32 GiB on a GB10).
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
- **A model whose parked memory keeps growing restarts instead.** vLLM 0.30
  on a GB10 left memory behind on every park and wake: one launch's parked
  charge grew from 4.7 to 9.6 and then 13.4 GiB. Once a launch's parked charge
  has grown past its first measured park by more than the host's
  `resource_policy.parked_growth_limit` (default `auto`: the first charge
  again, so it may double), its next park, idle, switch or `capyctl park`, is
  a stop, and the next request starts it fresh. `capyctl status` says when
  that is about to happen and when it did. Set the bound as a percentage
  (`50%`), a size of growth (`8GiB`) or `off`, in the host document,
  `--set resource_policy.parked_growth_limit=…` or
  `CAPYCTL_SET__RESOURCE_POLICY__PARKED_GROWTH_LIMIT` (standalone:
  `host.resource_policy.parked_growth_limit`). A host that does not state it
  keeps its stored policy unchanged. See
  [configuration](configuration.md#parked-growth-limit).

- **A model split across machines reserves its share of the weights.** A
  deployment with a `topology` whose memory CapyCTL derives from the weights
  charged every machine the whole checkpoint, so a two-machine
  `tensor_parallel: 2` group reserved its weights twice and a model larger than
  one machine could never start. Each machine now reserves the weights divided
  by `tensor_parallel x pipeline_parallel`, plus the tensors every machine
  keeps whole, read from the checkpoint's safetensors headers (a tenth of the
  weights when they cannot be read), and the KV cache, margin and startup
  placeholder are derived from that share. A 126 GiB checkpoint at
  `tensor_parallel: 2` reserves about 64 GiB of weights per machine; with
  CapyCTL's startup placeholder (2.25 times the share) it still needs
  `memory.startup` on two 128 GB machines. Declared `resources` and
  single-machine deployments are unchanged. See
  [several machines](../guide/several-machines.md#a-model-split-across-machines).

- **Large models on unified memory keep a larger margin.** Beside the weights
  and the KV cache, a memory request on a unified-memory machine such as a
  GB10 keeps a margin for the engine's own memory: still 8 GiB, or the
  weights x 0.15 plus 4 GiB when that is more (above 26.7 GiB of weights).
  Measured on a GB10, every model up to 22 GiB of weights fitted in the
  8 GiB, so those deployments are unchanged; gpt-oss-120b on vLLM used
  3.3 GiB more than CapyCTL reserved for it. A request CapyCTL derives for
  such a model grows by the difference, and one you state leaves a smaller
  KV cache. A revision deployed before keeps its sizing. Discrete GPUs are
  unchanged.

- **vLLM's loader under deep parking is a setting.** CapyCTL starts a parking
  vLLM deployment with the `eager` weight loader, as before; set
  `engine_config.vllm.safetensors_load_strategy: lazy` to map the weights
  instead. On vLLM 0.30 NVFP4 models `eager` held about 15–17 GiB more once
  loaded without a faster wake. Leaving the field out changes nothing for an
  existing deployment. `--safetensors-load-strategy` in `extra_args` is now
  refused for every vLLM deployment, parking or not; move it to the field. See
  [Add an engine](../guide/engines.md#vllm-weight-loading-while-parking).

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
- **gpt-oss is not parked.** vLLM 0.30.0 and SGLang 0.5.21 cannot wake a
  parked gpt-oss model: vLLM reloads its weights wrongly and the woken model
  answers garbage (a request after such a wake failed with `engine_error`
  after about 13 minutes), and SGLang's weight reload fails. A
  `deep` or `host_backed` deployment of a gpt-oss checkpoint (its
  `config.json` names `gpt_oss`) is now refused at start with
  `capability_missing:deep_park`, by standalone and by a host agent, on both
  engines. Deploy gpt-oss `restart_only`, as the gpt-oss recipes do.

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
| GLM-5.3-Flash | no (container image only) | unverified | MLX 4-bit, or EXL3 (experimental) |

Not yet: DeepSeek V4 Flash 0731, DeepSeek V4 Flash Vision and MiMo-V2.5 have
a working build only as a container image; MiniMax M3, Hy4
Preview and Nemotron 3 Ultra need three or more machines; Hy3 has no validated
command. Next milestones: container launchers, and a live row on three or more
rented machines.

## Requests

- **Live load on the management API.** `GET /management/v1/metrics/load`
  (`?deployment=<id>` for one) reports, for every model, the requests CapyCTL
  is forwarding and the ones waiting, with their limits, and for every
  instance the engine's running and waiting requests, how many requests the
  engine runs at once (SGLang's own figure, else the one CapyCTL passed, with
  where it came from) and when the host last sampled it, with the sample's age
  and whether it is still fresh. A client that balances across several
  servers can poll it every second. Standalone shows no engine figures. See
  [reading load](../guide/parking.md#reading-load).
- **A model can let no request wait.** `max_pending_per_deployment: 0` in a
  host's `resource_policy.queue` (or `--set`, or `CAPYCTL_SET__…`) makes
  CapyCTL refuse a request at once, with `429 queue_full` and `Retry-After`,
  where it would have made it wait: beyond the 32 running requests, or for a
  model that is not loaded (which still starts loading). The default stays
  64. See [no waiting](../guide/parking.md#no-waiting).

## Requests

- **`cache_salt` keeps tenants' prompt caches apart.** A chat request may
  carry `cache_salt`, a non-empty string of at most 1024 bytes; CapyCTL passes
  it unchanged to vLLM and SGLang, which then reuse cached prefixes only
  between requests with the same salt. It was refused as an unknown field
  before. TensorFold ignores the field, so a request carrying it to a
  TensorFold model is refused with `400 cache_salt_unsupported` instead of
  being served without the isolation it asks for. See
  [Make a request](../guide/requests.md#keep-tenants-prompt-caches-apart).

## Fixes

- **The streamed fields CapyCTL relays are documented.** The
  [requests guide](../guide/requests.md#streamed-fields) lists every delta,
  choice and usage field each supported vLLM, SGLang and TensorFold version
  streams, read from their sources. All are relayed, including
  `prompt_tokens_details.cached_tokens`; a delta field outside the list still
  ends the stream as unverified.

- **Deployments with an engine environment start again.** A deployment
  stating `engine_config.env` was accepted and then failed to start or load
  with `CorruptStoredData`, since its stored revision lost the variables; it
  now keeps them, values stored for the launch and still shown redacted.
- **`status` shows the measured startup peak.** Once a first run measured a
  deployment's startup peak, the next start reserved it, but the STARTUP
  column of `capyctl status deployment` kept showing the estimate made before
  any run (55.2 GiB for an SGLang model whose peak measured 29.7 GiB). It now
  shows the figure the next start reserves, with provenance `measured` in
  `--format json`.
