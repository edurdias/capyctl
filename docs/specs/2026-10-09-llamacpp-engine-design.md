# llama.cpp engine — design

Date: 2026-10-09. Status: the owner chose the scope (restart-only, single model) on
2026-10-09; the rest are the recommended defaults, recorded as ADR 0029. Nothing is
implemented. Plan: `docs/plans/2026-10-09-llamacpp-engine.md`.

## Problem

CapyCTL runs vLLM, SGLang and TensorFold. llama.cpp's `llama-server` is the engine
most home users already run: one C++ binary, GGUF checkpoints, an OpenAI-compatible
API, first-class CUDA on discrete GPUs and on GB10. CapyCTL should deploy, route,
drain, stop-to-park and relaunch-to-wake it, as it does TensorFold.

## What the source reading established

The v0.6.0 tag (commit `d812350`, published 2026-10-05; nightly `b11429` is the same
commit) was cloned and read: `tools/server/README.md`, `common/arg.cpp`,
`common/common.{h,cpp}`, `tools/server/*.cpp`, `src/llama-context.cpp`,
`src/llama-kv-cache.cpp`, `src/llama-arch.cpp`, the CMake build-info files and the
release workflow. A CPU-only build of the tag (no GPU, no model) was run for
`--version`, `--help` and three parser checks. Nothing else was run.

- **Version.** `--version` writes `version: 0.6.0 (build N, commit d812350)` and the
  compiler line to standard error and exits 0 (`common/build-info.cpp.in`).
  `LLAMA_BUILD_IS_DEV` defaults to `ON`, which makes the version `0.6.0-dev`; N is
  `git rev-list --count HEAD`, so a shallow clone reports `build 1`. `/props`
  `build_info` and every response's `system_fingerprint` are `b<N>-<commit>`.
- **Build layout.** On Linux `BUILD_SHARED_LIBS` defaults to on: `llama-server` loads
  `libllama-server-impl`, `libllama-common`, `libmtmd`, `libllama` and `libggml*`
  through a RUNPATH. The release workflow's CUDA build adds `GGML_BACKEND_DL=ON`
  (backends loaded from the binary's directory) and `$ORIGIN` RUNPATH, and ships the
  CUDA runtime as a second archive to unpack beside it.
- **Hidden inputs.** Before parsing the command line, every llama.cpp tool applies
  `/etc/llama.cpp/config.ini`, then `$XDG_CONFIG_HOME/llama.cpp/config.ini` (default
  `~/.config/llama.cpp`), then `LLAMA_ARG_*` variables (`LLAMA_API_KEY` for the key)
  (`common/arg.cpp`, `common_params_apply_system_config`). A config file can set any
  option, including `--api-key` and `--tools`.
- **Parser.** Exact names only; `_` becomes `-` in `--` options (`--ctx_size` works);
  `--ctx-size=4096` is "invalid argument"; `--ctx` is "invalid argument" (no
  abbreviation); a repeated option keeps the last value with a warning. Every option
  has a `--` long form.
- **Context and slots.** `-c` is the KV pool of all slots. Without a unified cache
  each slot's window is `pad256(c / np)` (`llama-context.cpp`, `n_ctx_seq`), and the
  KV tensors are `n_ctx_seq × np` cells per layer (`llama-kv-cache.cpp`, one stream
  per slot). Slot windows are capped at the training context while the cache keeps
  its size (`n_ctx_slot`). `-np` defaults to `-1`, which becomes 4 slots with a
  unified cache (`server.cpp`). `-c` defaults to 0, the training context.
- **Self-sizing.** `--fit` (default on) adjusts unset settings to free device memory
  with a 1024 MiB margin; `-ngl` defaults to `auto`; `--cache-ram` keeps up to
  8192 MiB of idle-slot prompt caches in host RAM; `--cache-idle-slots` needs it.
- **Endpoints.** `/health` and `/v1/health` need no key; until the model loads every
  route but the web UI's assets answers 503 `Loading model` (`server-http.cpp`), then
  `/health` answers 200 `{"status":"ok"}`. `/metrics` needs `--metrics` (off by
  default). `/slots` is on by default (`--no-slots`); it shows prompts and outputs only
  with `LLAMA_SERVER_SLOTS_DEBUG`. `POST /slots/:id` needs `--slot-save-path`.
  `POST /props` needs `--props` and changes nothing in 0.6.0. `/v1/models` gives the
  alias as `id` and `meta.n_ctx` (the slot window); `/props` gives `total_slots`,
  `endpoint_metrics`, `endpoint_slots`, `model_path`, `is_sleeping`.
- **Metrics.** Counters `llamacpp:prompt_tokens_total`, `prompt_tokens_cached_total`,
  `prompt_seconds_total`, `tokens_predicted_total`, `tokens_predicted_seconds_total`,
  `n_decode_total`, `n_tokens_max` (largest sequence seen), the speculative counters;
  gauges `prompt_tokens_seconds` and `predicted_tokens_seconds` (throughputs averaged
  since the previous scrape, reset by each scrape), `requests_processing`,
  `requests_deferred`, `n_busy_slots_per_decode`. No latency histograms, no KV usage
  ratio (`server-task.cpp`).
- **Per-request figures.** `usage` with `prompt_tokens_details.cached_tokens`;
  `timings` with `cache_n`, `prompt_n`, `prompt_ms`, `prompt_per_token_ms`,
  `prompt_per_second`, `predicted_n`, `predicted_ms`, `predicted_per_token_ms`,
  `predicted_per_second` (and `draft_n`, `draft_n_accepted` when drafting). A stream
  with `stream_options.include_usage` ends with an empty-`choices` chunk carrying
  `usage` and `timings`; without it, `timings` rides on the finish chunk. The README
  does not document the usage chunk; the source implements it.
- **Cancellation.** A closed connection cancels the task (`server_response_reader::
  stop`). A stream opened with an `X-Conversation-Id` header is buffered and survives
  the disconnect (`server-stream.h`). The SSE writer sends a `:` comment every 30 s
  (`--sse-ping-interval`) while nothing else is ready.
- **Sleep.** `--sleep-idle-seconds` (default off) unloads model and KV when idle;
  the next task reloads them, and a failed reload aborts the process. `/health`,
  `/props`, `/models` and `/metrics` bypass it; `/slots` wakes it. Router mode adds
  `POST /models/unload`. Single-model mode has no on-demand sleep or wake.
- **Requests.** `cache_salt` is not read anywhere in the server. Tool calls and
  reasoning are parsed from the chat template: specialized parsers for a few template
  families and a parser generated from any other template (`common/chat.cpp`).
- **Defaults worth closing.** The web UI is on; CORS answers any origin with
  credentials; built-in tools (`--tools`, `--agent`) read and search the host's files
  when enabled; `--reuse-port` lets another socket share the port; SIGTERM starts a
  graceful shutdown and a second signal exits at once.
- **GGUF.** Keys `<arch>.block_count`, `<arch>.context_length`,
  `<arch>.attention.head_count_kv` (a number or one per layer),
  `<arch>.attention.key_length`, `<arch>.attention.value_length`,
  `<arch>.attention.sliding_window`, `<arch>.full_attention_interval`,
  `<arch>.attention.kv_lora_rank`, `<arch>.ssm.*`, `split.count`; shards are named
  `<name>-%05d-of-%05d.gguf`; magic `GGUF`, version 3, alignment 32.
- **Release notes.** MTP speculative decoding for Qwen4Exp (about 1.5x decode on DGX
  Spark), a W4A4 NVFP4/MXFP4 CUDA path, ggml v0.26.0.

## Owner decisions

- Scope (2026-10-09): restart-only, one model per deployment, for v1.
- The defaults below are adopted with it as ADR 0029, each with its alternatives.

## Scope

In scope: the `llamacpp` engine kind; detection and `engine add` of a bare binary;
`local_engine.llamacpp`; launch; readiness; drain; restart-only park and wake; typed
settings; GGUF header sizing; per-request metrics and load reports; docs; CPU and
Fake-engine tests; live rows on a maintainer's discrete-GPU machine, then on GB10
after owner approval.

Out of scope: deep and host-backed residency; router mode; multi-rank groups and
`--rpc`; embeddings and reranking; non-CUDA backends (unqualified); `--tools`,
`--agent` and MCP; slot save and restore; the engine-owned RAM prompt cache.

## Design

ADR 0029 states each decision and its alternatives. The implementation shape:

### 1. Engine kind and registration

`Engine::Llamacpp`, serde name `llamacpp`, default profile name `llamacpp`, in every
closed engine set (`engine_policy::Engine`, `LaunchSettings`, `AdapterSpec`,
`PreparedLaunch`, the CLI's engine names, the `local_engine` schema).

- Detection adds a binary source beside the dist-info one: an executable regular file
  named `llama-server` in the ADR 0029 §2 locations, no execution, no symlink out of
  the root. The version comes from a sibling `libllama.so.X.Y.Z` name when present.
- `engine add <path>` accepts the binary or its directory, runs `--version` under the
  bounded check with standard error captured (the other engines keep standard
  output), and parses version and commit. `build_fingerprint` is `<version>+<commit>`.
- The installation digest is the ADR 0008 manifest over the binary and the
  `lib*.so*` files beside it (or in `<prefix>/lib`).
- The verified set gains `(llamacpp, "0.6.0")` after the live rows; the listing treats
  `0.6.0-dev` with commit `d812350` as 0.6.0.
- `security.deep_park: disabled` is written; `--deep-park enabled` is refused
  `capability_missing`. No toolchain check.
- `add` and every launch refuse when `/etc/llama.cpp/config.ini` exists.

### 2. Launch

```
<bin>/llama-server --host 127.0.0.1 --port <port> --model <checkpoint>/<gguf> \
  --alias <served name> --ctx-size <pad256(context_length) × slots> \
  --parallel <slots> --no-kv-unified --gpu-layers <n_gpu_layers|all> \
  --cache-type-k <t> --cache-type-v <t> --fit off --cache-ram 0 \
  --no-context-shift --metrics --slots --offline --no-webui \
  --cors-origins localhost --no-cors-credentials \
  [--mmproj <checkpoint>/<mmproj_file>] [host-fixed args] [extra args]
```

Environment: the closed engine environment (SPEC §13.3), the placement's
`CUDA_VISIBLE_DEVICES`, `XDG_CONFIG_HOME=<state>/engines/llamacpp/config` and
`LLAMA_CACHE=<state>/engines/llamacpp/cache` (service user, 0700, empty). The engine's
output goes to the launch's redacting log as for every engine; `--log-file` is
reserved.

Typed settings: `context_length` (required), `max_concurrent_requests` (slots,
default 4, shown with its source under `context.streams` as TensorFold's),
`kv_cache_dtype` (llama.cpp's cache types, default `f16`), and a new
`engine_config.llamacpp` block with `n_gpu_layers` (integer or `all`), `gguf_file` and
`mmproj_file` (relative paths inside the checkpoint). The common fields llama.cpp has
no flag for are refused. Reserved, sensitive and ordinary options are ADR 0029 §6's
lists, checked by exact name after `_` → `-`, with every alias and negative form.
`--parallel` in the extra or host-fixed arguments is reserved (unlike TensorFold,
where it wins): the slot count sizes the KV, so it has one source.

### 3. Readiness

`/health` 200; `/v1/models` lists the served name; `/props` `total_slots`,
`endpoint_metrics` and `endpoint_slots`, and `/v1/models` `meta.n_ctx`, match what
was rendered (else `effective_args_mismatch`); then the bounded chat probe (non-empty
`content` or `reasoning_content`). The undeclared Initialize window is ADR 0014 A1
plus the A7 allowance until live rows measure the start.

### 4. Drain, park, wake

The `idle_before_signal` gate reads `/metrics` (`requests_processing` and
`requests_deferred` both 0) with ADR 0023 §6's outcomes. SIGTERM to the owned group,
gone evidence, release. `capyctl park deployment` is refused unsupported. Wake is a
fresh launch of the pinned contract. Engine quiescence after a hang-up reads the same
two gauges (SPEC §10, T17). An SSE `:` ping is not backend progress.

### 5. Sizing

The GGUF header reader (`capyctl-config`, header only, bounded: at most 64 MiB of
metadata and 1 048 576 keys, strings at most 1 MiB, arrays at most 1 048 576 entries)
returns the keys above. With no `resources` and no `memory.request`, the request is
derived as ADR 0029 §9 states; the refusal cases there name the field to declare
instead. The short form `resources: {gpu, ram}` and the phase form work unchanged.
The margin starts at the ADR 0014 §5 placeholders (A18 on unified memory, ADR 0019 §3
on a discrete GPU) and is recomputed from live rows.

### 6. Requests and metrics

`cache_salt` is refused before forwarding (`AdapterError::CacheSaltUnsupported`).
`engine_metrics.rs` gains the `timings` arm: `prefill_ms` ← `prompt_ms`,
`decode_tokens_per_second` ← `predicted_per_second` (positive only), the completion
count ← `predicted_n` when `usage` is absent; `may_carry` also accepts chunks carrying
`"timings"`. Cached tokens come through the existing `usage` path. `ttft_ms` stays the
router's; `queue_ms` is absent.

Load reports: a `llamacpp` family with running ← `llamacpp:requests_processing`,
waiting ← `llamacpp:requests_deferred`, and the KV usage derived from one bounded
`/slots` read per report: the tokens of processing slots (`n_prompt_tokens` plus
`next_token[0].n_decoded`) over the sum of `n_ctx`. No latency histograms are
forwarded, so `LATENCY_ENGINES` does not gain `llamacpp`.

## Errors

- `engine_unsupported` (`engine add`): `/etc/llama.cpp/config.ini` exists; or the
  version output does not parse.
- `engine_config_file` (launch, closed reason): the same file appeared after
  registration.
- `capability_missing`: `deep` or `host_backed` on llama.cpp; `--deep-park enabled`.
- Resolution refusals naming the field: `context_length` missing or above the GGUF's
  training context; several GGUF candidates without `gguf_file`; a derived request
  that cannot be derived (ADR 0029 §9) without `resources` or `memory.kv_cache`;
  `kv_cache_dtype` outside llama.cpp's list; a reserved option or environment name.
- `effective_args_mismatch`: readiness found other slot settings than rendered.
- `cache_salt_unsupported` (request).

## Testing

CPU and Fake-engine tests, tagged T42 (llama.cpp conformance, added to SPEC §20 in
slice L1) and T14, T17, T37, T40 where they apply. They are not qualification.

Live rows (scripts under `scripts/live/matrix/rows/`), first on a maintainer's
discrete-GPU machine, then on GB10 after the owner approves a lab-host build:

- LC1: `engine add` lists llama.cpp 0.6.0 with the digest and commit.
- LC2: deploy a Qwen3 GGUF; plain and streaming chat, tool calls and
  `reasoning_content` through the router; chunk shapes against the relay's delta
  allowlist; peak memory within the reservation (records the measured margin).
- LC3: park releases the memory; a request wakes the deployment and succeeds.
- LC4: a llama.cpp deployment and a vLLM deployment switch under memory pressure.
- LC5: a streaming request cancelled midway; the gauges return to idle within the
  drain bound, then park.
- LC6: per-request figures (`capyctl.metrics`) and load reports during LC2's traffic;
  `--spec-type draft-mtp` on a model with MTP heads (records the MTP context's cache).

## Items that need a live check

1. Start time to `/health` 200 and the chat probe on each host class (sets the
   Initialize window).
2. Peak and steady GPU memory against the derived request (the margin); host RSS on a
   discrete GPU (the `ram` figure); on GB10, whether the default mmap load holds a
   page-cache copy of the weights beside the device copy.
3. The KV formula against llama.cpp's own startup log line for the KV buffer size.
4. SIGTERM to exit time and full memory release.
5. Stream chunk shapes (`role`, `content`, `reasoning_content`, `tool_calls`, the
   final usage chunk with `timings`) against `STREAM_DELTA_FIELDS`.
6. That a client hang-up drops `requests_processing` to 0 promptly, mid-prefill
   included.
7. The `/slots`-derived KV usage under load.
8. The MTP context's cache size with `draft-mtp`, and quantized V cache with flash
   attention `auto`.
9. What a prebuilt `b11429` CUDA binary reports for `--version` (`0.6.0` or
   `0.6.0-dev`).
10. Whether the version check needs CUDA initialisation time on a CUDA build.

## Findings that change the first sketch

The first evaluation's sketch (2026-10-09, read from the release notes and README)
differs from the source in these places:

- `-c <context_length> -np <n>` gives each request `context_length / n` tokens; the
  rendered `-c` must be the window times the slot count, with an explicit KV layout.
- The tag points at `d812350`; `8345f33` is the release's target branch head at
  publication.
- The version is on standard error, may read `0.6.0-dev`, and the build number
  depends on the clone, so the fingerprint uses the commit.
- A binary digest misses the shared libraries the default build loads.
- `config.ini` files and `LLAMA_ARG_*` variables are hidden inputs to close.
- Short aliases need no table (ADR 0014 §6 already refuses short options); every long
  alias, negative form and the `_` spelling do.
- `prompt_tokens_seconds` and `predicted_tokens_seconds` are throughput gauges reset
  by each scrape, not latency series; `n_tokens_max` is the largest sequence seen, not
  cache pressure; `prompt_tokens_cached_total` exists.
- The final usage chunk is implemented; only its documentation is missing.
- `cache_salt` must be refused, as for TensorFold.
- The GGUF key is `<arch>.attention.key_length`, not `<arch>.key_length`, and
  `head_count_kv` may be per layer.
- The parser is not one generic fallback: specialized template parsers, then one
  generated from the template.
- The fattn commit cited for DGX Spark is a HIP fix for the 576-wide head path, not
  a Qwen4Exp kernel.
- The web UI, CORS default, `--tools`/`--agent`, `--reuse-port` and `--props` need
  closing as well.

## Documentation

- SPEC: llama.cpp in §1 R01 and §9.4 (this change); T42 in §20 with slice L1.
- ADR 0029, and an amendment note in ADR 0018.
- With slice L6: the engines guide (install, build flags, prebuilt binaries,
  restart-only, the KV layout), the engine-flags table, configuration settings,
  errors, and the release notes. The site's platform list follows the docs.
