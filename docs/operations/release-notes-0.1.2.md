# CapyCTL 0.1.2

## Engines

- **vLLM 0.30.0** is verified beside 0.29.0. Its `--enable-scale-out` is
  reserved, and `--watermark-config` and `--engram-config` need a named host
  approval like every other `*-config` option. vLLM 0.29 and 0.30 stream a
  reasoning trace as `delta.reasoning`; CapyCTL now relays it, where before a
  deployment with `--reasoning-parser` failed its readiness check.
- **SGLang 0.5.21** is verified beside 0.5.20. A request to a parked thinking
  model now wakes it on both versions; before, the wake check's few tokens went
  to the thinking trace and the wake was left `uncertain`.
- **TensorFold 0.6.2, 0.6.3 and 0.6.5** are verified beside 0.6.0 and 0.6.1.
  0.6.4 was not checked and shows `custom yes`. TensorFold 0.6.5's
  `--api-key`, `--api-key-file` and `--metrics-open` are reserved: CapyCTL owns
  authentication, and a key on the engine would lock CapyCTL out of its routes
  and metrics.
- A new guide, [Install an engine](../guide/install-engines.md), walks through
  installing vLLM, SGLang or TensorFold in a `uv` environment and registering
  it. The engines guide has a first model on each engine, with deployment
  files in `docs/examples/`.

## Memory

- **16 GB cards.** A model whose charge reaches the card's managed limit
  starts: memory the card already holds outside CapyCTL (driver reservation,
  display server) now comes out of the device reserve instead of being counted
  twice, which left such a deployment `queued`. A `memory.request` declared for
  a card derives its KV cache from the request. Checked with a 9 GB model on a
  16 GB RTX 4090 Laptop GPU on vLLM 0.29 and 0.30 and SGLang 0.5.21.
- **Standalone memory limits are settable.**
  `host.resource_policy.memory.system.managed_limit` and `free_reserve` take
  `auto` (50 % and 20 %, as before), a size (`90GiB`) or a percentage (`75%`),
  in YAML, with `--set` or with `CAPYCTL_SET__...`. On a discrete machine they
  set host RAM; card limits stay derived. See "Standalone memory limits" in the
  [settings reference](configuration.md).
- **TensorFold is capped by its declaration.** CapyCTL launches TensorFold
  with `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` set to the deployment's Ready
  allocation. Before, TensorFold sized itself from the machine's free memory
  and could grow past its declaration under long prompts.
- **Kernel builds leave the startup peak alone.** A first start of vLLM,
  SGLang or TensorFold that compiles kernels no longer records the compilers'
  memory as the deployment's startup peak, which on a unified-memory host left
  every later start `capacity_blocked`. Builds inside the engine process
  (Triton, torch.compile workers) are still counted.
- **A parked model is charged what it holds.** After its first park, CapyCTL
  measures the memory a parked engine keeps and charges that, never below the
  placeholder (1 GiB on a card, 2 GiB unified) and never above its Ready
  charge. `capyctl status deployment <name> --json` shows it under `parked`.
- **SGLang `memory.kv_cache` is the KV cache.** It now sets SGLang's KV pool,
  as on vLLM. A hybrid model's per-request state is sized beside it for
  `max_concurrent_requests`, or as many requests as fit, up to 32. An explicit
  `memory.request` too small for it is refused before launch, naming the
  smallest request that fits; a derived one runs as many requests as fit, and
  status shows the limit. On a discrete GPU, a derived request keeps the state
  inside the KV cache CapyCTL chose, up to half of it, and the fitted context
  is held to the rest; status and the start name the same request. Before, a
  hybrid model could be held to 2 running requests.
- **First starts and draft models.** The first-start reservation on vLLM and
  SGLang covers CUDA graph capture and the load transient
  (`max(request + graphs, weights x 2.25 + margin)` for new deployments). A
  draft model's weights and KV layers are counted in the request and in the
  fitted context. vLLM gets `--max-num-seqs 32` by default, and the fit for
  gated-delta-net hybrids follows vLLM's own block layout.

## Reliability

- An engine is no longer stopped when helper processes it started exit on
  their own, such as the torch inductor compile workers SGLang starts with
  CUDA graphs on. Only the engine's own processes exiting stops it; a stop
  still ends the helpers.
- **Long prompts.** A 256k-token prompt on TensorFold is bounded by the request
  deadline until its first token, instead of being cut by the 120 s idle bound
  during prefill. SGLang's refusal of a prompt longer than its KV pool now
  reaches the client as `400 engine_rejected` with SGLang's message, instead of
  a 500 or a stream with no `[DONE]`. A request the router cuts for its bounds
  is cancelled on the engine, as a hang-up is.
- **Standalone queue bounds** (`host.resource_policy.queue`) are settable, and
  a restart applies changed bounds.
- **Refusals.** `capacity_blocked` names the host, the memory domain, what the
  deployment needs, what is free and the limit. A start during a slow stop is
  answered `still_stopping` (retryable, exit 25) instead of
  `reconciliation_required`. A launch refusal a retry cannot change (an SGLang
  sizing refusal, `checkpoint_mismatch`, a missing capability) gives up after
  one attempt. `checkpoint_mismatch` explains that `model.content_fingerprint`
  is CapyCTL's digest over the whole model directory, and status prints the
  declared and measured digests.
- **`--wait`.** `start deployment --wait` and `deploy --activate --wait` end
  at once with exit 13 and the reason when the start gives up, instead of
  waiting out the Initialize window. After a redeploy, `--wait` waits for the
  previous stop instead of returning `still_stopping`.
- **TensorFold first request.** The readiness check waits out the kernels
  TensorFold 0.6.1 and later build on their first request, within the startup
  budget, instead of killing the engine mid-build after 60 s. A build lock left
  by a killed start is removed once no recorded process is alive.
- **Engine registrations.** On standalone, a registered profile's approvals,
  `security.extra_args`, `env` and `log_policy` are now applied, as on a host.
  Before, standalone dropped them, so an approved drafter path was refused and
  `extra_args: denied` was not enforced. `capyctl engine add` takes
  `--approve-option` and `--approve-path`.
- `capyctl engine remove` works with no role running: it edits `engines.yaml`
  and the role publishes the removal when it starts. A role skips a profile
  for an engine it does not know, with a warning, instead of refusing to start.
- A local model path outside the model store (for example a Hugging Face
  snapshot directory) is measured inside its own root. A foreign listener on a
  leased engine port is refused `port_conflict` before the launch. A failed
  deployment can be updated with `deploy --revision`.

## Features

- **Parsers by model family.** For Qwen3, Qwen3.5, Qwen3.6 and Qwen3.8
  checkpoints, CapyCTL picks vLLM's and SGLang's tool-call and reasoning
  parsers from the checkpoint's `config.json` and chat template, so tool calls
  come back structured and reasoning comes back apart from `content` with no
  `extra_args`. Set them with `engine_config.vllm|sglang.tool_call_parser` and
  `reasoning_parser` (`auto`, `none` or a parser name). A parser in
  `extra_args` or host-fixed arguments wins over `auto`. Status shows a
  `Parsers` line.
- **TensorFold runs requests together.** `engine_config.max_concurrent_requests`
  renders TensorFold's `--parallel`, 8 when undeclared; before, TensorFold
  decoded one request at a time. Status shows `Streams`. TensorFold deployments
  can turn drafts off with `extra_args: [--no-drafts]`.
- **SGLang CUDA graphs are on by default** while a model can park, about 75 %
  faster decoding on a 16 GB card. `engine_config: {cuda_graphs: false}` turns
  them off.
- `capyctl init host` and `init server` write YAML to `.yaml` and `.yml`
  paths. JSON results are indented on a terminal and stay one line when piped.
  A missing `--config` file is named in the error. Status splits a startup
  charge into card and host RAM, and shows a pending operation's reason under
  the table. Non-streamed vLLM answers now carry `usage`.

## Packaging

The release archive and the installer no longer ship the guides; the
installer and the systemd units point to
https://edurdias.github.io/capyctl/docs/install/. An upgrade removes the old
`share/capyctl/docs`.

## Upgrade

Replace the binary and restart the roles, the server before its hosts. 0.1.1
state is reused; the server store migrates on first start, so back up each
role's state directory first if you may roll back (see "State and migrations"
in the [install guide](install.md)). Then check these:

- **A deployment refused `capacity_blocked` since a first start that built
  kernels** keeps the peak 0.1.1 recorded. Delete it and deploy it again once.
- **SGLang with speculative decoding** (`--speculative-algorithm` in its
  arguments) now defaults to `restart_only`: it is stopped and started instead
  of parked, because a wake would not restore the draft model's weights. A
  deployment that states `residency: deep` or `host_backed` with it is refused;
  remove the residency or set `restart_only`, and deploy it again.
- **SGLang hybrid models with a stated `memory.request` and `kv_cache`** are
  refused when the request cannot hold the per-request state beside the
  KV cache. For example, a 48 GiB request with a 16 GiB KV cache and a
  DFlash2 draft model no longer fits 8 running requests. Raise the request to
  the value the message names, or lower `max_concurrent_requests`.
- **TensorFold deployments** now start with `--parallel 8` and a memory cap at
  their Ready allocation. A `resources` block sized for one stream may be
  refused at start with "TensorFold's memory cap cannot hold context_length
  beside its streams"; lower `max_concurrent_requests` or `context_length`, or
  raise the Ready allocation. A `--parallel` in `extra_args` still wins, but
  is refused beside `max_concurrent_requests`. Remove `--api-key`,
  `--api-key-file` and `--metrics-open` from `extra_args`.
- **SGLang deployments created on 0.1.1** keep CUDA graphs off. State
  `engine_config: {cuda_graphs: true}` and deploy again to turn them on.
- **vLLM and SGLang Qwen deployments without parsers** get them from their
  model family at their next launch, which changes the response shape for
  clients that parsed tool calls or `<think>` out of `content`. Set the
  parser to `none` to keep the old output.
- **Standalone memory limits** now follow the role document at every start.
  With no limit stated, the default is unchanged.
