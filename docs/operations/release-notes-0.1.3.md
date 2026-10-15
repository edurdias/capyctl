# CapyCTL 0.1.3

CapyCTL 0.1.3 runs one model across two machines: vLLM, SGLang and TensorFold
groups at `tensor_parallel: 2` are qualified live on two GB10 hosts over a
direct link, with deep parking for vLLM groups. llama.cpp's `llama-server`
joins as a fourth engine, and TensorFold 0.6.6 and vLLM 0.31.0 are verified.
Memory sizing is more accurate on unified memory and for SGLang hybrid and
speculative models, every chat answer carries per-request engine figures, and
the management API reports live load and engine log tails. A wake that fails
or answers differently now stops the model instead of leaving it stuck.

## Engines

- **TensorFold 0.6.6 verified.** CapyCTL lists TensorFold 0.6.6 as verified
  beside 0.6.0 to 0.6.3 and 0.6.5. Its new `--name-priority ID=background`
  option is an ordinary extra argument; on the second machine of a two-machine
  group, which CapyCTL starts without `--name`, it may fail.
- **vLLM 0.31.0 verified and launches.** The reserved argument check now
  follows the destinations the installed vLLM parser itself defines, so a build
  that moved an option reserves the name it spells: vLLM 0.31.0 moved logging
  into the `--logging-config` object (`--logging-config.<field>`, with
  `--log-level` and `--log-config-file` as flat overrides), and every spelling
  of it stays reserved beside `--uvicorn-log-level` — a deployment or host
  cannot set the logging configuration, on 0.30.0 or 0.31.0. vLLM 0.31.0
  answers carry the same per-request figures as 0.30.0; `cached_tokens` still
  maps from `usage.prompt_tokens_details.cached_tokens`, which 0.31.0 reports
  only with `--enable-prompt-tokens-details` passed as an extra argument, and
  the figure is left out when the engine reported none, never shown as zero.
- **llama.cpp's `llama-server` as a fourth engine.** Register a `llama-server`
  built from llama.cpp v0.6.0 with `capyctl engine add <path to llama-server>`
  and deploy a GGUF model directory with `engine: llamacpp`. CapyCTL reads the
  GGUF header and derives the memory from the weights and a full-window KV
  cache for each of the deployment's slots (`max_concurrent_requests`, 4 by
  default). A llama.cpp model does not park: when CapyCTL needs the memory, it
  stops it and starts it again on the next request. Answers carry llama.cpp's
  `timings` and CapyCTL's per-request figures; tool calls and reasoning come
  from the model's chat template. Every llama.cpp build lists as custom for
  now. See [llama.cpp](../guide/engines.md#llamacpp).
- **TensorFold's key variable is reserved.** `TENSORFOLD_API_KEY` can no
  longer be set in a profile's or deployment's `engine_env`, whatever the
  approvals: it turned on TensorFold's own authentication and locked CapyCTL
  out of the engine's health and metrics.
- **Prefill time on TensorFold's CUDA engine.** A TensorFold answer from a
  CUDA Qwen model now carries the engine's `prefill_ms`; its time to first
  token and decode rate are measured by the router.

## Multi-node groups

Qualified live on two GB10 hosts at `tensor_parallel: 2` over their direct
link, with RDMA carrying the group traffic: vLLM 0.30.0 groups pass every row
(bring-up, five deep park and wake cycles with the wake canary, a killed head
or worker, a host agent going away and coming back, and alternating with
another model under `--evict`); SGLang 0.5.21 groups pass bring-up, failure
and recovery, and restart instead of parking; TensorFold 0.6.5 groups pass
bring-up and failure, restart only. Groups of three or more machines are the
next milestone.

- **One model across machines.** A deployment with a `topology` and
  `placement.hosts` runs one engine rank per machine, head first. vLLM 0.30.0
  and SGLang 0.5.21 run tensor and pipeline parallel on any number of machines
  that divides `tensor_parallel` × `pipeline_parallel`; TensorFold 0.6.5 runs
  `tensor_parallel: 2` on exactly two machines. Only two machines are
  qualified. See
  [One model across machines](../guide/several-machines.md#one-model-across-machines).
- **vLLM groups park deep.** A vLLM group's park completes once each host
  proves its own rank released its memory, and each member is then charged
  what it really holds parked. A worker's recorded processes include the rank
  process that holds the GPU, and a member that cannot prove its state fails
  the park within a minute instead of fifteen, with every charge kept until
  the group stop releases it.
- **SGLang groups restart instead of parking.** An SGLang group across
  machines gets `restart_only` by default, and a group that states
  `residency: deep` or `host_backed` is refused at deploy with
  `capability_missing:deep_park` and a hint to declare `restart_only` (see
  Known issues). SGLang on one machine still parks
  ([Groups across machines](../guide/engines.md#groups-across-machines)).
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
- **Ports a group opens.** The guide lists every listener a vLLM, SGLang or
  TensorFold group opens, the address it binds and that none is
  authenticated, with example nftables and ufw rules for the direct link
  ([Ports a group opens](../guide/several-machines.md#ports-a-group-opens)).
- **DP attention ports.** With SGLang DP attention, the head's start check
  also covers the handshake port `P+13`, and a rendezvous port above 65522 is
  refused because SGLang cannot bind the handshake there. The per-rank ports
  SGLang picks at start cannot be checked
  ([Ports a group opens](../guide/several-machines.md#ports-a-group-opens)).
- **TensorFold groups start and become ready.** A TensorFold group member
  starts with its installation's libnccl (`TF_NCCL_LIB`); an installation
  without the nvidia-nccl wheel is refused with `nccl_library_missing`. CapyCTL
  reads the token ids a TensorFold head returns where TensorFold puts them, so
  the group passes its readiness check.
- **A member on an unreachable host reads `uncertain`.** When a member host's
  agent goes offline, its member shows `uncertain` (`group_member_uncertain`)
  at once and keeps its memory charge. When the agent comes back with its
  journal, the member shows `launched` again.
- **A killed group worker leaves no engine processes behind.** When a worker
  member's engine process died while processes it had started still ran (a
  vLLM `Worker_TP1` holding device memory), the host now stops every process
  of the member's launch and the group settles. A worker member whose launch
  was never recorded settles on its host's evidence that every process of that
  launch is gone.

Catalog models that run as groups in this release, on two machines at
`tensor_parallel: 2` (not part of the live rows):

| Model | SGLang 0.5.21 | vLLM 0.30.0 | TensorFold 0.6.5 |
|---|---|---|---|
| Qwen3.8-Flash-Next | NVFP4 | NVFP4 (stock build unverified) | MLX 4-bit only |
| GLM-5.3-Flash | no (container image only) | unverified | MLX 4-bit, or EXL3 (experimental) |

Not yet: DeepSeek V4 Flash 0731, DeepSeek V4 Flash Vision and MiMo-V2.5 have
a working build only as a container image; MiniMax M3, Hy4 Preview and
Nemotron 3 Ultra need three or more machines; Hy3 has no validated command.
Next milestones: container launchers, and a live row on three or more
machines.

## Memory

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
- **Short `resources`.** A model that restarts instead of parking can state
  its memory as `resources: {gpu: 11GiB, ram: 2GiB}` instead of five phases:
  CapyCTL reserves both figures while it starts, runs and stops and nothing
  once it is stopped. On a unified-memory machine the pool is charged their
  sum. The five phases are still accepted, and `capyctl validate config` shows
  what the short form stands for. See the TensorFold section of the
  [engines guide](../guide/engines.md#tensorfold).
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
- **vLLM's weight loader while parking stays `eager` by default.** A
  deep-parking vLLM deployment that does not set
  `engine_config.vllm.safetensors_load_strategy` loads with `eager`, as in
  0.1.2, on every machine. Set `lazy` to map the weights instead: on unified
  memory it holds far less memory while loading, running and parked
  (Qwen3.6-35B NVFP4 on vLLM 0.30: 34.5 against 54.2 GiB while loading, 8.3
  against 28.3 GB parked), at the cost of a slower start and wake (see Known
  issues). `--safetensors-load-strategy` in `extra_args` is now refused for
  every vLLM deployment, parking or not; move it to the field. See
  [vLLM weight loading while parking](../guide/engines.md#vllm-weight-loading-while-parking).
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
  request, CapyCTL now adds a hybrid model's per-request state to the request
  it derives, for `max_concurrent_requests` (or 8) as far as the machine holds
  it, instead of borrowing it from the margin or the KV cache. Qwen3.8-27B at
  the default KV cache runs 8 requests where it ran 5, and asks for about
  6 GiB more (16 GiB with DFlash2). A revision measured before this release
  keeps its sizing; a new revision of it gets the state.
- **SGLang hybrid models run 8 requests by default.** Without
  `max_concurrent_requests`, a hybrid model on SGLang runs up to 8 requests at
  once, as TensorFold does, instead of up to 32; dense models are unchanged.
  The router still accepts 32 per deployment and the rest wait in SGLang's
  queue. Set `max_concurrent_requests` to run more, and CapyCTL sizes the
  state for that count.
- **SGLang hybrid models with speculative decoding get the memory they are
  sized for.** A hybrid model (Qwen3.5 and later) launched with
  `--enable-linear-replayssm-spec` no longer has per-draft-token recurrent
  states counted against its memory request, and one drafting with EAGLE from
  its own MTP head now has that draft's KV layer counted in its KV cache and
  its start-up copy of the embedding and LM head in SGLang's static pool, so
  SGLang holds the KV pool CapyCTL passes instead of a smaller one. A stated
  memory request that cannot hold the state `--max-mamba-cache-size` fixes,
  with the weights, the KV cache, SGLang's own allocations and the margin, is
  now refused before the start, naming the request that does, instead of
  running on a smaller margin.
- **SGLang with speculative decoding parks again.** A single-machine
  deployment with `--speculative-algorithm` in its arguments parks `deep` by
  default instead of restarting: the park gives back the KV cache and keeps
  the weights, the draft model's included, so a wake reloads nothing. It
  frees less than a model without a draft model, and its parked charge is
  measured on its first park. `residency: host_backed` is still refused for
  it; a deployment that states `restart_only` keeps restarting. On a 128 GB
  GB10, Qwen3.8-27B NVFP4 with DFlash2 parks in about a second, holds
  33.4 GiB parked and answers about 2 s after a request wakes it. That is
  above standalone's default parked limit of a quarter of the memory (32 GiB),
  so once its first park has measured it, later parks are refused and it
  stays loaded, unless you raise the parked limit as below.
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
- **A model whose engine keeps a large table on disk is sized without it.**
  With SGLang's `--ple-offload-backend file` or TensorFold's `--ple-on-ssd` in
  a deployment's engine arguments, Qwen3.8-Flash-Next's 47.7 GiB n-gram table
  counts against the models disk instead of memory, plus the engine's
  in-memory cache of it (8 GiB for SGLang, 1 GiB for TensorFold). Without
  those options nothing changes.
- **A start refused for memory says why, with numbers.** When a deployment
  fits the managed limit but the memory available now, less its startup
  charge, would not leave the free reserve, status and `start --wait` give the
  available memory, the charge, the reserve and the shortfall, and `--wait`
  exits with `insufficient_resources`. A role whose managed limit plus free
  reserve is more than the memory available when it starts warns about it in
  its log and in `capyctl list hosts`
  ([memory limits](configuration.md#standalone-memory-limits)).

## Requests

- **Per-request metrics.** Every chat answer now carries the engine's
  figures for that request: time to first token, queue time, prompt time,
  generation speed and cached prompt tokens, each marked as measured by the
  engine or by CapyCTL, and left out when unknown rather than shown as zero.
  A non-streaming answer has them under `capyctl.metrics`; a stream ends
  with a `: x-capyctl-metrics` comment line before `data: [DONE]`. vLLM now
  starts with `--enable-per-request-metrics` and SGLang with
  `--enable-cache-report`; neither can be turned off. See
  [Make a request](../guide/requests.md#per-request-metrics).
- **Live load on the management API.** `GET /management/v1/metrics/load`
  (`?deployment=<id>` for one) reports, for every model, the requests CapyCTL
  is forwarding and the ones waiting, with their limits, and for every
  instance the engine's running and waiting requests, how many requests the
  engine runs at once (SGLang's own figure, else the one CapyCTL passed, with
  where it came from) and when the host last sampled it, with the sample's age
  and whether it is still fresh. A client that balances across several
  servers can poll it every second. See
  [reading load](../guide/parking.md#reading-load).
- **A model can let no request wait.** `max_pending_per_deployment: 0` in a
  host's `resource_policy.queue` (or `--set`, or `CAPYCTL_SET__…`) makes
  CapyCTL refuse a request at once, with `429 queue_full` and `Retry-After`,
  where it would have made it wait: beyond the 32 running requests, or for a
  model that is not loaded (which still starts loading). The default stays
  64. See [no waiting](../guide/parking.md#no-waiting).
- **`cache_salt` keeps tenants' prompt caches apart.** A chat request may
  carry `cache_salt`, a non-empty string of at most 1024 bytes; CapyCTL passes
  it unchanged to vLLM and SGLang, which then reuse cached prefixes only
  between requests with the same salt. It was refused as an unknown field
  before. TensorFold ignores the field, so a request carrying it to a
  TensorFold model is refused with `400 cache_salt_unsupported` instead of
  being served without the isolation it asks for. See
  [Make a request](../guide/requests.md#keep-tenants-prompt-caches-apart).

## Standalone

- **Live engine load in standalone.** `GET /management/v1/metrics/load` now
  shows each standalone instance's engine load, about once a second, as it
  does on a server with hosts: the engine's running and waiting requests, its
  KV-cache use and, for SGLang, how many requests it runs at once (`source:
  engine`). CapyCTL also uses that load to choose between instances. See
  [reading load](../guide/parking.md#reading-load).
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
  after about 13 minutes), and SGLang's weight reload fails. A `deep` or
  `host_backed` deployment of a gpt-oss checkpoint (its `config.json` names
  `gpt_oss`) is now refused at start with `capability_missing:deep_park`, by
  standalone and by a host agent, on both engines. Deploy gpt-oss
  `restart_only`, as the gpt-oss recipes do.

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
- **A downloaded model is ready sooner.** A Hugging Face or `http` model
  CapyCTL downloads is checked file by file against its pins as it arrives.
  Its checkpoint digest is now built from those checks, so the first start no
  longer reads every weight file a second time. The digest is the same one a
  full read gives. A file changed after the download, and a tar archive's
  contents, are still read in full.
- **A host may trust a declared digest.** With
  `checkpoints.trust_declared_digest: true` (`--trust-declared-digest true`,
  `CAPYCTL_TRUST_DECLARED_DIGEST=true`; off by default), a host takes a local
  model's declared `model.content_fingerprint` as its digest the first time it
  sees the model, reading only the small files. Any later change to a file is
  measured in full, and a digest that no longer matches is refused. Turning
  the setting off measures the model again.
- **Status says where a digest came from.** `capyctl status deployment`
  reports `provenance` (`measured`, `fetched` or `declared_trusted`) with the
  checkpoint digest, and says so in text when the digest was not measured in
  full.
- **A drafter can be downloaded beside the model.** A deployment may declare
  its speculative drafter as a source of its own, `model.draft`, in any form
  `model.source` takes (a local path, a pinned Hugging Face commit, an HTTPS
  payload pinned by SHA-256). CapyCTL downloads and verifies a remote drafter
  into the sources store like the weights, waits for both before the
  deployment activates, counts its weights with the model's, and passes its
  directory to the engine (SGLang's draft model path, vLLM's
  `--speculative-config` `model`, TensorFold's `--drafter`), with no
  `approved_paths` entry. The engine arguments still turn speculation on
  (SGLang `--speculative-algorithm`, vLLM `--speculative-config` with
  `num_speculative_tokens`) and may not name a draft path of their own. Hosts
  and the server must run this release; the server's store migrates to
  schema 47. A deployment without `model.draft` is unchanged, fingerprints
  included. See [configuration](configuration.md#models-and-downloads).

## Model downloads

- **Plain `http://` downloads where the host approves them.** An `http`
  model source may name an `http://` URL, for example an internal mirror.
  A host fetches it only when it allows plain HTTP:
  `model_sources.plain_http: allowed`, `--model-sources-plain-http allowed`
  on `start host` and `start standalone`, or
  `CAPYCTL_MODEL_SOURCES_PLAIN_HTTP=allowed`. The default stays HTTPS only,
  and the download is still checked against its `sha256`. See
  [configuration](configuration.md#models-and-downloads).
- **A download URL kept as a secret.** An `http` source may state
  `url_ref: secret://<name>` instead of `url`, naming an owner-only file
  `<state dir>/secrets/<name>` on the host that holds the URL, for example a
  short-lived presigned link. The deployment, its revisions, status and logs
  only ever hold the reference; the host reads the URL when it downloads
  and checks it against its own `model_sources` rules first. Replacing an
  expired link means rewriting the file; the deployment does not change.
  Like every deployment field, it is set in the deployment YAML only.

## Lifecycle

- **Models that move only on your command.** `lifecycle.activation: explicit`
  in a deployment, or in a host document for every model the host may run
  (`host.lifecycle.activation` in standalone, also through `--set` and
  `CAPYCTL_SET__…`), makes CapyCTL start, stop, park and wake that model only
  when an operator asks. A request while it is stopped or parked is refused
  at once with 409 `deployment_inactive` and starts nothing; it is never
  parked or stopped to make room for another model, never by the idle
  timers, and never stopped to make room among parked models. `start`,
  `stop`, `park` and the rest work as before. Omitted, nothing changes. See
  [Only on your command](../guide/parking.md#only-on-your-command). With a
  queue of zero, a model that only an operator may start (stopped by one, or
  with explicit activation) is still answered 409, not 429 `queue_full`.

## Engine logs

- **Every engine keeps a redacted log.** SGLang's output was discarded unless
  CapyCTL ran with `--debug-engine-logs`; vLLM and TensorFold kept theirs
  unredacted. Now every engine's output is kept at its default level (info
  for SGLang), and a writer between the engine and the file replaces the
  launch's keys and other credentials, URL credentials, presigned URL query
  strings, bearer values and credential-shaped text with `<redacted>` before
  anything is written. Model paths are kept whole. Request logging stays off,
  so no prompt or completion is logged. The log stays owner-only and rotates
  at 16 MiB, keeping two older files. See
  [engine logs](install.md#engine-logs-and-troubleshooting).
- **Read the end of an engine log through the management API.**
  `GET /management/v1/deployments/<id>/engine-log?instance=<n>&kib=<N>`
  returns up to 256 KiB (64 by default) of an instance's log, whole lines,
  redacted, with the admin token. A log written under `--debug-engine-logs`
  is never served.
- **`--debug-engine-logs`** still raises SGLang to debug level; every engine's
  output is then written without the writer, as before (SGLang's Python-level
  output is still scrubbed of credential shapes).

## Containers

- **A role runs correctly as a container's main process.** See
  [Running in a container](install.md#running-in-a-container). CPU tests
  only; not yet run live in a container.
  - **Memory limits are honoured.** With cgroup v2, host memory is bounded by
    the tightest `memory.max` of the role's cgroup and its ancestors: the
    capacity by the limit, the available memory by the limit minus the
    cgroup's usage (its inactive page cache counted as free). A unified
    (GB10) machine's single domain follows the same bound. The role says at
    start which cgroup bounds it, and `inspect host --json` shows it as
    `memory_source`. cgroup v1 limits are not read, and the role warns.
  - **`--init` is not required.** A host or standalone role makes itself a
    child subreaper at start and reaps the processes its engines leave
    behind; as PID 1 it reaps every orphan. Before, a zombie read as a
    running process, so with no init a stop's verified cleanup never
    completed. An exited process whose parent never waits for it is now
    reported as such instead of read as running.
  - **`nvidia-smi` is also found on `PATH`**, after `/usr/bin` and `/bin`, so
    an image with the NVIDIA tools elsewhere observes its GPUs.

## Fixes

- **A wake that fails or hangs no longer leaves a model stuck.** If a parked
  model's wake does not finish within its wake window (`timeouts.wake`), or
  vLLM reports that reloading the weights failed, CapyCTL now stops the model,
  with `wake_uncertain` or `wake_reload_failed` as its last error, instead of
  leaving it uncertain with its memory held until you stopped it by hand. The
  next request loads it fresh. A vLLM weight reload during a wake may now take
  the whole wake window instead of at most two minutes. See
  [Errors](../guide/errors.md).
- **A woken model must answer as it did before parking.** After a vLLM or
  SGLang model on one machine wakes, CapyCTL asks it a fixed prompt and
  compares the answer with the one it gave when it first became ready. A
  different answer, for example from weights that did not reload intact,
  fails the wake with `wake_mismatch` and stops the model instead of serving
  wrong answers. See [Parking and switching](../guide/parking.md#park-and-wake).
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
- **First starts on a GB10 no longer run out of memory compiling kernels.**
  The engines compile some GPU kernels after the model has loaded. On a
  unified-memory machine CapyCTL now sizes the number of compile jobs from the
  memory left once the deployment's Ready memory is taken, not from the free
  memory before the model loads; a model using most of the machine compiles
  with 1 or 2 jobs. Discrete GPUs are unchanged. A `MAX_JOBS` set in the
  engine profile's `env` or the deployment's `engine_config.env` is still used
  as written ([registering engines](install.md#registering-engines)).
- **Deep parking of SGLang failed on hosts whose state directory path is
  long.** When the state directory's path was longer than about 63 bytes,
  the engine's memory-saver observation could not start, and every park of
  an SGLang deployment was refused as "not quiescent". The observation now
  works with a state directory of any length. If it still cannot start, the
  engine log says why, and a refused park says the engine's saver
  observation is not enrolled.
- **Engine logs no longer hide model paths.** A model path containing a
  `.` (for example under `~/.cache/huggingface`) showed up in SGLang's
  argument line with part of it replaced by `<redacted>`. Paths are now kept
  whole. A credential-length segment inside a path, keys, URL credentials and
  bearer values are still redacted.
- **A start refused for its memory request says what to raise it to first.**
  SGLang's sizing refusals now begin with `raise engine_config.memory.request
  to "<size>"` and give their figures after it, so status and
  `capyctl start --wait`, which shorten long reasons, never cut it off. A
  shortened reason ends in `…`.

## Documentation

- **Engine options page.** A new [Engine options](../guide/engine-flags.md)
  page has one table per engine (vLLM 0.30, SGLang 0.5.21, TensorFold 0.6.5)
  for the options people ask about most: speculative decoding, parsers,
  context and concurrency, state and KV caches, memory, CUDA graphs,
  quantization, logging, KV offload and plugins. Each row says whether the
  option is a typed field, an extra argument, an extra argument that needs
  host approval, or reserved, and what to use instead. A test checks every
  row against CapyCTL's argument checks. The page also shows how to reach a
  given SGLang memory fraction with `memory.request` and `memory.kv_cache`;
  there is still no setting for the fraction itself.
- **Streamed fields.** The
  [requests guide](../guide/requests.md#streamed-fields) lists every delta,
  choice and usage field each supported vLLM, SGLang and TensorFold version
  streams, read from their sources. All are relayed, including
  `prompt_tokens_details.cached_tokens`; a delta field outside the list still
  ends the stream as unverified.

## Known issues

- **SGLang groups do not park.** In a live two-machine SGLang 0.5.21 group,
  the park completed but the wake failed: after the head reloaded its weights
  from disk (186–222 s), the restore became uncertain and the group stopped.
  SGLang groups therefore run `restart_only` in 0.1.3, and `deep` or
  `host_backed` is refused at deploy.
- **A vLLM wake under `lazy` is slow.** On a GB10 with vLLM 0.30.0 and
  Qwen3.8-27B NVFP4, a deep-parked deployment loading with `lazy` took about
  2 minutes (123 s) to reload its weights in place on its first wake, against
  23–28 s with `eager`. That wake now gets the whole wake window instead of
  failing at 120 s, but a `lazy` wake of this model has not been rechecked
  live. Keep the default `eager` where wake time matters.
