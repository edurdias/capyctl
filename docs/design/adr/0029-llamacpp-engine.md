# ADR 0029 — llama.cpp (`llama-server`) as the fourth engine

**Status:** Accepted (owner decision 2026-10-09: restart-only, single model, for v1).
Sections 2 to 12 are the recommended defaults adopted with that decision; each
records the alternatives considered. Nothing is implemented yet.
**Amends:** `SPEC.md` §1 (R01, supported engines) and §9.4 (later backends), and
ADR 0018 §1 (a bare-binary installation: detection, version check, fingerprint).
SPEC §20 gains T42 (llama.cpp conformance) with the first tests (plan slice L1).
**Related:** ADR 0008 (installations and fingerprints), ADR 0012 (deep parking and its
protections), ADR 0014 (§3 reserved settings, §5 memory request, §6 extra arguments,
§7 checkpoint identity, §8 sensitive options, A1, A6, A7, A18), ADR 0017 (version
skew), ADR 0018 (registration), ADR 0019 (discrete GPUs), ADR 0023 (TensorFold, the
landing shape this follows), ADR 0024 (parsers by model family), ADR 0028 (multi-node
groups). Design: `docs/specs/2026-10-09-llamacpp-engine-design.md`. Plan:
`docs/plans/2026-10-09-llamacpp-engine.md`.

## Context

llama.cpp (MIT) serves GGUF checkpoints through `llama-server`, a single C++ binary
built with CMake, with an OpenAI-compatible API. It moved to semantic releases:
v0.6.0 was published on 2026-10-05 (tag `v0.6.0`, commit `d812350`; ggml v0.26.0).
Home users run it by hand on discrete GPUs and on GB10-class unified-memory machines.

What the v0.6.0 sources say (read at the tag; a CPU build of the tag was used only to
read `--help` and `--version`; nothing ran on a GPU):

- `GET /health` answers 503 `Loading model` until the model is loaded, then 200
  `{"status":"ok"}`, and needs no API key.
- The only unload is `--sleep-idle-seconds` (off by default), an engine-local idle
  timer: the model and KV leave memory after N idle seconds and the next task reloads
  them (a failed reload aborts the process). `/health`, `/props`, `/models` and
  `/metrics` neither wake it nor reset the timer; `/slots` wakes it. A single-model
  server has no on-demand sleep or wake. Router mode (`llama-server` started without a
  model) can unload a model on request, by running each model as a child server.
- Unset options are filled from `/etc/llama.cpp/config.ini`, then
  `$XDG_CONFIG_HOME/llama.cpp/config.ini` (default `~/.config/llama.cpp`), then
  `LLAMA_ARG_*` environment variables (and `LLAMA_API_KEY`), before the command line.
- The parser takes exact names only (no abbreviation), replaces `_` with `-` in `--`
  options, refuses `--name=value`, and keeps the last of a repeated option with a
  warning. Every option has a `--` long form; several have more than one.
- `-c` is the KV pool of all slots together: without a unified KV cache each of the
  `-np` slots gets `-c / -np` tokens, rounded up to 256. Left unset, `-c` is the
  model's training context, `-np` is `auto` (4 slots, unified KV), `--fit` (on by
  default) shrinks unset settings to the free device memory, `-ngl` is `auto`, and
  `--cache-ram` keeps up to 8192 MiB of idle prompt caches in host RAM.
- `--metrics` is off by default; `/slots` is on; the web UI is on; CORS allows any
  origin with credentials.
- Chat responses carry `usage` (with `prompt_tokens_details.cached_tokens`) and a
  `timings` object (`prompt_ms`, `predicted_per_second`, `cache_n`, ...); a stream
  with `stream_options.include_usage` ends with an empty-`choices` usage chunk that
  also carries `timings`.
- `cache_salt` is not read.
- A Linux build links `libllama`, `libggml*`, `libmtmd`, `libllama-common` and
  `libllama-server-impl` as shared libraries through its RUNPATH. `--version` prints
  `version: 0.6.0 (build N, commit H)` on standard error; a build configured without
  `-DLLAMA_BUILD_IS_DEV=OFF` reports `0.6.0-dev`, and N counts the commits in the
  clone (a shallow clone reports 1).

## Decision

### 1. Scope: one model, restart-only

`llamacpp` joins `vllm`, `sglang` and `tensorfold` in every closed engine set. A
deployment runs one `llama-server` process serving one GGUF model. Its residency is
`restart_only`; `deep` or `host_backed` fails resolution with `capability_missing`.
Parking is the restart-only release and waking is a fresh launch of the same pinned
effective contract (§10). Multi-rank groups (ADR 0028) are out of scope.

*Alternatives:* hold llama.cpp until it can deep-park (nothing to adopt upstream
today, see the outlook); adopt router mode for its on-demand unload (rejected in §7).
SPEC §9.4 lets a later backend begin with an approved restart-only profile, and ADR
0023 shows the shape.

### 2. Registration: a bare binary

The installation is a `llama-server` executable, not a Python environment.

- **Detect** finds an executable regular file named `llama-server` in `PATH`
  entries, the build directories `~/llama.cpp/build/bin` and
  `~/llama.cpp/build*/bin`, `/opt/*/bin`, `/usr/local/bin` and every `--path`, with
  ADR 0018 §1's bounds; it follows no symlink out of the root and executes nothing.
  It reads the version from a sibling `libllama.so.<major>.<minor>.<patch>` file name
  when there is one (a shared build), and otherwise lists the version as unknown
  until `add`.
- **Add** takes the binary or a directory holding it, resolves it lexically, and runs
  the bounded version check (ADR 0018 §1: 60 s, 4 KiB, cleared environment, own
  process group) reading **standard error**, where `llama-server --version` writes.
  It parses `version: <v> (build <n>, commit <h>)`. The profile's
  `build_fingerprint` is `<v>+<h>` (for example `0.6.0+d812350`); the build number is
  not part of it, since it counts the commits in the clone.
- **Fingerprint** (ADR 0008): the installation digest is the canonical manifest over
  the binary and every `lib*.so*` regular file in its directory, and in `<prefix>/lib`
  when the binary is `<prefix>/bin/llama-server` (the CMake install layout). That
  covers llama.cpp's own libraries, the backends a `GGML_BACKEND_DL` build loads from
  its directory, and CUDA runtime libraries unpacked beside a prebuilt binary. System
  libraries outside the installation are not covered, as for the other engines. A
  digest of the binary alone would miss a rebuilt `libggml-cuda`.
- **Verified set:** `(llamacpp, "0.6.0")` once the live rows pass (§3). A build counts
  as 0.6.0 when it reports `0.6.0`, or `0.6.0-dev` with the tag's commit `d812350`
  (a build of the tag configured without `-DLLAMA_BUILD_IS_DEV=OFF`). Anything else
  lists as `custom` (ADR 0017).
- **Deep park:** llama.cpp has no deep-park capability, so the profile is written
  `security.deep_park: disabled` and `--deep-park enabled` is refused
  `capability_missing`. Nothing compiles at run time, so there is no toolchain check.
- **Hidden configuration:** `add` refuses an installation on a machine that has
  `/etc/llama.cpp/config.ini` (`engine_unsupported`, naming the file and saying it
  would set options CapyCTL cannot see), and every launch checks again (§6).
- **The role's own engine:** `local_engine.llamacpp`, `--llamacpp-bin`,
  `CAPYCTL_LLAMACPP_BIN`, with the shared precedence; the profile is `local`, or
  `local-llamacpp` beside another engine. The default `engine add` profile name is
  `llamacpp`.

*Alternatives:* a digest over the binary plus its build info (misses the shared
libraries, and the build info is self-reported); requiring a static build
(`-DBUILD_SHARED_LIBS=OFF`), which would refuse llama.cpp's default build; executing
`llama-server` during detection (ADR 0018 forbids it).

### 3. Qualification order

Live qualification runs first on a maintainer's discrete-GPU machine, with a v0.6.0
build in the home directory (AGENTS.md allows engine builds there, with no driver,
CUDA or system-package change). A build on a lab host follows only after the owner
approves it, and is then added to the AGENTS.md list of approved engine environments.
The discrete-GPU rows qualify function; a GB10 row qualifies unified-memory sizing.
Until a row passes on a host class, that class runs llama.cpp unqualified and the
engines guide says so. CPU and Fake-engine tests are not qualification.

*Alternatives:* build on a lab host first (needs an approval that is not yet given and
risks the shared hosts for a first contact); qualify on one host class only (GB10's
memory behavior differs from a discrete GPU's). The binary may be a source build of
the tag or llama.cpp's prebuilt Ubuntu CUDA 13.4 build of the same commit (nightly
`b11429`, x86-64 and arm64): it needs no compiler, and on a lab host it needs the same
approval. The live rows record which one ran.

### 4. Listener and key

The engine listens on `127.0.0.1` at its private port and has no engine key, as
TensorFold. CapyCTL owns authentication at its ingress, and the routed path is the
only network path to the engine. `--api-key` and `--api-key-file` are reserved and the
closed environment never carries `LLAMA_API_KEY`. CapyCTL renders
`--cors-origins localhost` and `--no-cors-credentials`, so a web page in a browser on
the same machine cannot read the engine's answers (llama.cpp's default echoes any
origin with credentials), and `--no-webui`.

*Alternatives:* a per-launch `--api-key` (defense in depth, but every route except
`/health` would then need the key, including the `/metrics` and `/slots` reads the
host makes, and no deep-park control path requires the ADR 0012 key guard). This is
revisited with any park facility, when the ADR 0012 protections become mandatory.

### 5. Context, slots and memory settings CapyCTL renders

`context_length` is required: it is each request's window. `max_concurrent_requests`
is the slot count, 4 when undeclared (`LLAMACPP_DEFAULT_PARALLEL`, llama-server's own
automatic count). CapyCTL renders

```
-c <pad256(context_length) × slots> -np <slots> --no-kv-unified
```

so each slot gets exactly the declared window (llama.cpp rounds it up to a multiple of
256) and the whole KV cache is allocated at start. A `context_length` above the GGUF's
`<arch>.context_length` is refused at resolution: llama.cpp caps each slot at the
training context while still allocating the larger cache. CapyCTL also renders
`-ngl <n>` (`engine_config.llamacpp.n_gpu_layers`, an integer or `all`, default
`all`), `-ctk <t> -ctv <t>` (`kv_cache_dtype`, one of llama.cpp's cache types `f32`,
`f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `iq4_nl`, `q5_0`, `q5_1`, default `f16`),
`--fit off`, `--cache-ram 0` (SPEC §12: no engine-owned host cache until designed),
`--no-context-shift`, `--metrics`, `--slots` and `--offline`. The common fields
llama.cpp has no flag for (`dtype`, `quantization`, `cuda_graphs`,
`trust_remote_code`, `language_model_only`) are refused on a llama.cpp profile;
quantization is in the GGUF file.

*Alternatives:* `-c <context_length>` alone (what a hand-written command usually
does: each of N slots would get 1/N of the window); a unified pool of
`context_length` shared by all slots (smaller, but concurrent long requests compete
for it and an admitted request can fail when it fills, with context shift off); a
unified pool of `context_length × slots` capped per slot by `--kv-unified-per-slot`
(same memory as the chosen layout, more prefix sharing; a candidate once a live row
compares the two). For the default slot count: 1 (least memory, no concurrency), 8
(TensorFold's; here every slot's cache is allocated at start, so 8 doubles the KV of
4), `auto` (it also switches the KV layout).

### 6. Reserved, sensitive and ordinary options

Reserved options are refused in `extra_args` and host-fixed arguments at deploy time
and again when the command is rendered. The check compares exact names after
replacing `_` with `-`, as llama.cpp's parser does, and covers every long alias and
negative form (for example `--gpu-layers` and `--n-gpu-layers`, `--ui` and `--webui`,
`--slots` and `--no-slots`). Short options are already refused in extra and
host-fixed arguments (ADR 0014 §6), and for llama.cpp so is a `--name=value`
spelling, which llama.cpp itself rejects.

- **Reserved, rendered by CapyCTL:** `--host`, `--port`, `--model`, `--alias`,
  `--ctx-size`, `--parallel`, `--kv-unified`/`--no-kv-unified`,
  `--kv-unified-per-slot`, `--gpu-layers` (all spellings), `--cache-type-k`,
  `--cache-type-v`, `--fit`, `--fit-target`, `--fit-ctx`, `--cache-ram`,
  `--context-shift`/`--no-context-shift`, `--metrics`, `--slots`/`--no-slots`,
  `--offline`, the web UI switches, `--cors-origins`, `--cors-methods`,
  `--cors-headers`, `--cors-credentials`/`--no-cors-credentials`, `--mmproj`.
- **Reserved, never rendered:** `--api-key`, `--api-key-file`,
  `--sleep-idle-seconds`, `--models-dir`, `--models-preset`, `--models-max`,
  `--models-autoload`/`--no-models-autoload`, `--rpc`, `--slot-save-path`,
  `--cache-idle-slots`, `--props`, `--reuse-port`, `--path`, `--api-prefix`,
  `--ssl-key-file`, `--ssl-cert-file`, `--log-file`, `--log-prompts-dir`, the
  verbosity options (request logging stays off, SPEC §13.3), the model sources
  `--hf-repo`, `--hf-file`, `--hf-token`, `--model-url`, `--docker-repo`,
  `--mmproj-url`, `--spec-draft-hf` and the `--*-default` presets (CapyCTL
  materializes checkpoints, ADR 0008), the built-in agent surface `--tools`,
  `--tools-runtime`, `--agent`, `--mcp-servers-config`, `--mcp-servers-json`,
  `--ui-mcp-proxy`, the device choice `--device`, `--split-mode`, `--tensor-split`,
  `--main-gpu`, `--spec-draft-device`, `--mmproj-device` (one GPU per model, chosen
  by CapyCTL, ADR 0019), `--embedding`, `--rerank` and `--pooling` (v1 serves chat),
  and the options that print and exit (`--help`, `--version`, `--list-devices`,
  `--cache-list`, `--completion-bash`).
- **Sensitive, host approval by name (ADR 0014 §8):** paths `--model-draft` (also
  spelled `--spec-draft-model`),
  `--lora`, `--lora-scaled`, `--control-vector`, `--control-vector-scaled`,
  `--chat-template-file`, `--grammar-file`, `--json-schema-file`,
  `--lookup-cache-static`, `--lookup-cache-dynamic`, `--media-path`, with values
  inside `security.approved_paths`; code `--video-ffmpeg-dir`.
- **Ordinary:** everything else, including `--spec-type`, `--load-mode`,
  `--override-tensor`, `--cpu-moe`, `--n-cpu-moe`, `--flash-attn`,
  `--reasoning-format`, `--reasoning`, `--chat-template` and batch sizes.

The hidden inputs are closed too. The launch environment is closed (SPEC §13.3) and
points `XDG_CONFIG_HOME` and `LLAMA_CACHE` at empty CapyCTL-owned directories
(`<state>/engines/llamacpp/{config,cache}`, mode 0700), so no user-level
`config.ini` applies. A profile or deployment environment may not name a variable
llama-server reads in place of a reserved option, its listener, its device or its
directories, even when `approved_env` lists it (every `getenv` of the v0.6.0 tag
read): `LLAMA_ARG_*`; `LLAMA_SERVER_*` (router-child mode and slot debugging);
`AIP_*` (`AIP_MODE=PREDICTION` with `AIP_HTTP_PORT` replaces the port);
`LLAMA_API_KEY`, `MTMD_BACKEND_DEVICE` and `HF_TOKEN` (the aliases of `--api-key`,
`--mmproj-device` and `--hf-token`); the ggml variables that pick or multiply
backend devices, load another backend or move device allocations into host memory
(`GGML_CUDA_DEVICES`, `GGML_CUDA_ENABLE_UNIFIED_MEMORY`, `GGML_VK_VISIBLE_DEVICES`,
`GGML_VK_PREFER_HOST_MEMORY`, `GGML_VK_ALLOW_SYSMEM_FALLBACK`, `GGML_METAL_DEVICES`,
`GGML_OPENCL_PLATFORM`, `GGML_OPENCL_DEVICE`, `GGML_HEXAGON_DEVICES`,
`ONEAPI_DEVICE_SELECTOR`, `GGML_BACKEND_PATH`); and `LLAMA_CACHE`,
`XDG_CONFIG_HOME` and `HOME`. The launch environment drops them again. A launch on a
host where `/etc/llama.cpp/config.ini` exists is refused before anything starts, with
the closed reason `engine_config_file`.

*Alternatives:* TensorFold's prefix rule (llama.cpp does not abbreviate, so it would
flag ordinary options such as `--cache-reuse` beside a reserved `--cache-ram`);
overriding a system `config.ini` by rendering every option (impossible for
`--api-key` or `--tools`, which have no negative form); listing `--tools` and
`--agent` as sensitive (they give the model the host's files; no approval path in
v1); leaving `--fit` ordinary because it only adjusts unset values (a later
heuristic change could still move a granted reservation).

### 7. Router mode unused

CapyCTL is the router. Each deployment instance is one single-model `llama-server`;
CapyCTL always renders `--model`, so llama-server never starts in router mode, and
the router-mode options are reserved (§6). The router's routes (`/models/load`,
`/models/unload`, `/models/sse`) are never used.

*Alternatives:* adopting router mode for its on-demand `/models/unload`, the only
explicit unload in v0.6.0. Rejected: it is a second controller that spawns its own
children, keeps its own model set and idle policy, and loads models on demand, which
SPEC §1.2 forbids ("one lifecycle owner").

### 8. Speculative decoding

Speculative decoding is configured with extra arguments. `--spec-type` and its tuning
options are ordinary, including the weight-free `ngram-*` types and `draft-mtp`
(MTP heads in the checkpoint). A draft model is `--model-draft`, a path option the
host approves by name, inside `security.approved_paths`, checked lexically at deploy
time and through symlinks before launch, never fetched and never digested (the
TensorFold drafter rule, ADR 0023 §5). Its GGUF size, every shard of a split set
named by its first shard, counts with the checkpoint's weights (ADR 0014 A6),
whether the host-fixed or the extra arguments name it; a draft that cannot be
counted (outside the approved paths, a shard missing) is refused, never left out.

*Alternatives:* reserving speculative decoding for v1 (loses llama.cpp's main
throughput lever on GB10, MTP for Qwen4Exp); a typed field (one more schema block for
options llama.cpp already validates).

### 9. Sizing from the GGUF header

A llama.cpp deployment may state `resources` (the short form, ADR 0023 amendment of
2026-10-04, or phases) or let CapyCTL derive the request (ADR 0014 §5) from the GGUF
header. CapyCTL reads the header only (magic, version, metadata), never tensor data,
of the GGUF it renders: the single `.gguf` in the checkpoint that is not an `mmproj`
file, the first shard of a single `-00001-of-0000N.gguf` set, or the file named by
`engine_config.llamacpp.gguf_file` (required when the checkpoint holds more than one
candidate, a relative path inside it). `engine_config.llamacpp.mmproj_file` names a
multimodal projector the same way and renders `--mmproj`.

- Weights: the rendered GGUF (all shards), the projector and the draft model (A6).
- KV: `slots × pad256(context_length) × Σ_layers head_count_kv × (key_length ×
  bytes(K) + value_length × bytes(V))`, from `<arch>.block_count`,
  `<arch>.attention.head_count_kv` (a number or one per layer),
  `<arch>.attention.key_length` and `.value_length` (else `embedding_length /
  head_count`), with the cache types' block sizes (`q8_0` is 34 bytes per 32
  values).
- Margin: a placeholder until live rows measure peak minus weights minus KV (the
  ADR 0014 §5 / A18 / ADR 0019 §3 family rules), then recomputed from the rows.

The derivation is refused, and the deployment must state `memory.kv_cache` or
`resources`, when the header shows sliding-window, recurrent (`ssm.*`), hybrid
(`full_attention_interval`) or MLA (`attention.kv_lora_rank`) attention, when
`--spec-type draft-mtp` is passed (its MTP context's cache is not yet measured), and
when the arguments move weights or KV off the GPU (`n_gpu_layers` other than `all`,
`--override-tensor`, `--cpu-moe`, `--n-cpu-moe`, `--n-cpu-ffn`, `--no-kv-offload`,
an `mlock` load mode). With `--fit off` llama-server allocates the whole cache its
context and slots fix, so where the header makes that cache calculable (full
attention, no cache layers off the GPU) a declared `memory.kv_cache`,
`memory.request` or `resources` that leaves less for it is refused; other layouts
keep the declared estimate. Checkpoint identity stays ADR 0014 §7's digest over the
whole directory; the draft model lies outside it, so every host, the embedded one
included, compares the weights and GGUF facts it re-measures before a launch with
the revision's. Undeclared `timeouts.initialize` follows ADR 0014 A1 and A7 until
live rows measure llama.cpp's start.

*Alternatives:* `resources` always required (TensorFold's rule; llama.cpp's cache
is fully allocated at start and derivable for common models); llama.cpp's own
`llama-fit-params` estimator (runs engine code during resolution and reads free
device memory); Σ of every file in the checkpoint (GGUF repositories often hold
several quantizations).

### 10. Readiness, drain, park and wake

Ready is `/health` 200, then `/v1/models` listing the served name, then `/props`
reporting `total_slots` equal to the rendered slot count and `endpoint_metrics` and
`endpoint_slots` true, and `/v1/models` `meta.n_ctx` equal to the rendered slot
window capped at the training context; a difference is `effective_args_mismatch`
(SPEC §8.2: reserved values verified after the engine's parser). Then the bounded chat
probe, where a non-empty `content` or `reasoning_content` is an answer.

The release before a stop signal reads `/metrics`: `llamacpp:requests_processing` and
`llamacpp:requests_deferred` both 0 is idle. An engine whose group is gone, that does
not listen or that answers 503 is signalled at once; one whose `/metrics` does not
answer is signalled when the bound ends (30 s or the command's remaining time); one
still busy at the bound is not signalled and the cleanup is uncertain (ADR 0023 §6).
Then SIGTERM to the owned group (llama-server shuts down on the first SIGTERM),
verified exit, then release. `capyctl park deployment` is refused as unsupported.
After a client hangs up, the same two gauges at 0 report engine quiescence (SPEC §10):
llama-server cancels the task when its connection closes. CapyCTL never forwards
client headers, so `X-Conversation-Id`, which makes a stream survive a disconnect,
never reaches the engine.

### 11. Requests and metrics

Routing selects the deployment and the forwarded request carries the served name.
Responses pass through unchanged, `timings` included. A request with `cache_salt` is
refused `cache_salt_unsupported` (llama-server ignores it, as TensorFold does).

Per-request figures (`capyctl.metrics`, SPEC §17): `prefill_ms` from
`timings.prompt_ms`, `decode_tokens_per_second` from `timings.predicted_per_second`
(positive only), `cached_tokens` from `usage.prompt_tokens_details.cached_tokens`,
the completion count from `usage` or else `timings.predicted_n`. llama.cpp reports
no time to first token and no queue time (`prompt_ms` excludes the wait for a slot),
so `ttft_ms` is the router's and `queue_ms` is absent.

Load reports: running is `llamacpp:requests_processing` and waiting
`llamacpp:requests_deferred`. llama.cpp exports no KV usage ratio, so the host
derives one from `/slots`: the tokens held by processing slots over the sum of the
slots' `n_ctx`. llama.cpp exports no latency histograms (its `*_tokens_seconds`
gauges are throughputs averaged between two scrapes, and each scrape resets them),
so no latency series is forwarded.

### 12. Tool calls and reasoning

llama-server parses tool calls and reasoning from the GGUF's chat template (`--jinja`
is on by default): a handful of templates have dedicated parsers and every other one
gets a parser generated from the template. There are no parser names, so
`engine_config.llamacpp` has no `tool_call_parser` or `reasoning_parser`, and ADR
0024's family detection and its status sources do not apply, as for TensorFold.
`--reasoning-format`, `--reasoning`, `--reasoning-budget`, `--chat-template` and
`--chat-template-kwargs` are ordinary extra arguments.

*Alternatives:* family detection from GGUF `general.architecture` (nothing to pass
the result to); typed fields for the reasoning options (more schema for options the
engine validates).

## Deep-park outlook

A `deep` tier for llama.cpp needs, upstream, an explicit single-model sleep and wake:
a request that unloads the model and KV and answers only once the memory is free, a
wake that reloads and answers once the model serves (and fails without aborting the
process), both behind the API key, with `/props` `is_sleeping` as the state. With
that, the ADR 0012 protections apply (loopback listener, per-launch key, key guard,
no control path through ingress), `--sleep-idle-seconds` stays reserved because the
idle policy is CapyCTL's (SPEC §6.5), and readiness must read `is_sleeping`, since
`/health` answers 200 while asleep. Wake reloads from disk, so it re-verifies the
checkpoint (ADR 0014 §7). We will file that request upstream later and write the
deep-park ADR only once such an API exists in a release.

## Consequences

- A fourth engine kind in every closed set, a bare-binary installation path beside the
  dist-info one, and a GGUF header reader in `capyctl-config`.
- The version check learns to read standard error for one engine.
- llama.cpp deployments allocate their whole KV cache at start: memory grows with
  `max_concurrent_requests`, and the status shows the slot count and its source.
- A machine-wide `/etc/llama.cpp/config.ini` blocks llama.cpp on that machine until
  removed.
- Out of scope: deep and host-backed residency, router mode, multi-rank and `--rpc`,
  embeddings and reranking deployments, and backends other than CUDA (Vulkan, Metal,
  ROCm and the rest run unqualified if they register).

## Verification

CPU and Fake-engine tests tagged T42 (with T14, T17, T37 and T40 where they apply)
cover registration, rendering, reserved options, sizing, readiness, the idle gate,
park refusal and the metrics mapping. They are not qualification. Live rows LC1–LC6
(plan slice L6) on a maintainer's discrete-GPU machine qualify it, and a GB10 row
after the owner approves a lab-host build.
