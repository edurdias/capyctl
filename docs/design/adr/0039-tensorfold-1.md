# ADR 0039 — TensorFold 1.0: the native binary beside the 0.6 line

**Status:** Proposed (2026-10-25), for the owner to decide. Target release 0.2.0 (retargeted from 0.1.9 on 2026-10-27, after 0.1.9 shipped with ADR 0035 only). Nothing
is implemented; the SPEC, guide and operations texts this ADR lists change with the
implementation, as for ADRs 0030 and 0033. The section "Open questions for the owner"
lists what the owner has to decide, each with options and a recommendation.
**Amends (with the implementation):** `SPEC.md` §9.3 (a second TensorFold line), §10 (the
idle reading of a TensorFold 1.0 engine), §20 (T41); ADR 0023 §1 to §8 (each gains the
native line beside the Python one); ADR 0018 §1 (a third installation form: a release
archive holding one native binary); ADR 0025 (the cap on TensorFold 1.0); ADR 0028 §10
(TensorFold 1.0 serves no group).
**Related:** ADR 0008 (installations and fingerprints), ADR 0012 (deep parking and its
protections), ADR 0014 (A1, A7 and A23: start and wake bounds), ADR 0017 (version skew and
capability gating), ADR 0024 (parsers), ADR 0029 (llama.cpp, the bare-binary shape this
follows), ADR 0030 (decisions), ADR 0031 (custom engines and the listener check), ADR 0032
(cancelled-request settlement), ADR 0033 (LMCache), ADR 0035 (several GPUs on one host),
ADR 0036 (embeddings, proposed), ADR 0037 (trace context, proposed), ADR 0038 (TensorRT-LLM,
proposed).

## Context

Owner decisions of 2026-10-25 for CapyCTL 0.2.0: TensorFold 1.0.0, a native executable
written in Zig and not a pip upgrade, is installed at `~/tensorfold-1.0.0` on host A and is
to be qualified `restart_only` with Nemotron 3.5 Lightning 30B-A3B. TensorFold 0.6.x stays
supported. No lab host is reachable now, so this ADR is a design from the engine's
published source, its release archive and its checkpoint; every claim about the running
engine is marked as read or as inference, and the TF rows below are the live check.

TensorFold 1.0.0 was published on 2026-10-07. It is no longer a Python package: a `pip
install` of the tag stops with directions to the binary, and the Python engine stays on
the 0.6 line (`setup.py:1-6`, `RELEASE-NOTES-1.0.0.md:43-45`). On CUDA it serves one
model family, Nemotron 3.5 Lightning on a GB10 (`README.md:33-54`). Newer native releases
already exist (1.1.4 on 2026-10-24, 1.1.5 on 2026-10-25) and change the interface this ADR
relies on (Evidence §9).

### What CapyCTL does for TensorFold 0.6.x today

| Concern | 0.6.x behaviour in CapyCTL | Where |
|---|---|---|
| Detection | `tensorfold-*.dist-info` in a Python environment; executes nothing | ADR 0023 §1, `crates/capyctl-agent/src/engines.rs:54-70` |
| Entry point and version | `<env>/bin/tensorfold`, `--version` | `crates/capyctl-agent/src/engines/resolve.rs:63`, `:343` |
| Registration checks | `ninja`, `nvcc`, `c++` on the closed launch PATH or `toolchain_missing` (exit 26); the Python capability probe (`core`, `deep_park`, `metrics`) | ADR 0023 §2, `crates/capyctl-agent/src/installation.rs:66-67`, `runtime_integrity.rs:59-60` |
| Launch | `serve <dir> --name --host 127.0.0.1 --port --no-update-check --backend cuda --snapshot-dir none --context <n> [--parallel <n>] [--kv-dtype] [--max-tokens] [--thinking/--no-thinking] [--drafter none]` | `crates/capyctl-adapters/src/tensorfold/args.rs:192-209`, `:278-335` |
| Environment | `TENSORFOLD_NO_UPDATE_CHECK=1`, `HF_HUB_OFFLINE=1`, `TRANSFORMERS_OFFLINE=1`, `TORCH_EXTENSIONS_DIR`, `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` (the declared Ready allocation), `TF_NCCL_LIB` for a group member, `DO_NOT_TRACK=1` | `args.rs:340-365`, `:401-419` |
| Reserved options | `--host --port --name --alias --backend --context --tp --rank --master --master-port --snapshot-dir --no-update-check --api-key --api-key-file --metrics-open`, checked with the abbreviation rule | `crates/capyctl-config/src/engine_policy.rs:239-255` |
| Readiness | `/health` `"ok": true`, the served name in `/v1/models`, then the chat probe | `crates/capyctl-adapters/src/tensorfold/adapter.rs:138-151` |
| Idle signal (drain, stop, cancelled requests) | `/health` `busy: false` and `requests_running: 0`; counters that disagree are busy; a `/health` without either field is unanswered | `tensorfold/http.rs:18-27`, `:85-98`, `adapter.rs:198-207` |
| Abort | the router closes the engine connection; `cancel_work` answers `Uncertain` | `adapter.rs:183-191`, ADR 0032 item 6 |
| Load reports | `tensorfold:requests_running`, `requests_waiting`, `kv_cache_usage_ratio` from `/metrics`, forced to at least one running request unless `/health` reads idle | `crates/capyctl-agent/src/load.rs:159-163`, `:482-494` |
| Memory | `resources` required; the declared Ready allocation is TensorFold's cap; streams' caches grow by use inside it | ADR 0023 §4, ADR 0025 |
| Start bound | 1800 s for a first kernel build; request deadline at least 1800 s | `crates/capyctl-config/src/effective/timeouts.rs:40`, `:216-217` |
| Release check | off on every launch (`--no-update-check`, `TENSORFOLD_NO_UPDATE_CHECK=1`) | `crates/capyctl-adapters/src/engine_telemetry.rs:24-26` |
| Parking | `restart_only`; `deep` and `host_backed` refused `capability_missing` | ADR 0023 §4, §6 |
| LMCache | off: `engine_unsupported`, "TensorFold has no LMCache integration" | ADR 0033 §2 |

### What was read

Read on 2026-10-25; no engine was run and no lab host was used.

- TensorFold [v1.0.0](https://github.com/ashhart/TensorFold/tree/v1.0.0), an annotated tag
  on commit `8b003018a090`. Paths below are under `zig/src/` at that commit unless they name
  a top-level file. Line references are to that commit.
- The release asset `tensorfold-1.0.0-linux-aarch64.tar.gz` (1,811,749 bytes, SHA-256
  `d33a249fa669558c5227f4977759b1fa9e0af407f7f05cae088333d6cc909b61`, matching the
  release's own `.sha256` file and its asset digest), unpacked in a cache directory of the
  maintainer's workstation and read only: `file`, `readelf -d`, `sha256sum -c SHA256SUMS`
  (all 51 lines pass). The aarch64 binary was not executed.
- The checkpoint
  [`TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit`](https://huggingface.co/TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit)
  at revision `d9d758fb83953437f7263256b0d96157e2a348b8`: the Hub's file list with sizes,
  `config.json` and the model card.
- For drift only: `zig/src/server/status_routes.zig` and `zig/src/server/cli.zig` at
  [v1.1.5](https://github.com/ashhart/TensorFold/tree/v1.1.5).

## Evidence

### 1. Distribution and identity

- The Linux archive holds `bin/tensorfold-native` (3,177,672 bytes, an aarch64 ELF whose only
  `NEEDED` entries are `libc.so.6`, `libpthread.so.0` and `libdl.so.2`; `libcuda.so.1`
  comes from the driver at run time), an empty `lib/` "reserved for future adjacent runtime
  libraries", `share/tensorfold/cuda/sm121/aot.json` and 46 `cubins/*.cubin`, `VERSION`
  (`1.0.0`), `RUNTIME.md`, `SHA256SUMS` over every other file, and license notices. CUDA
  fatbins are embedded in the binary; Nemotron also needs the `aot.json` and `cubins`
  directory, found at `share/tensorfold/cuda/sm<capability>/` beside the binary unless
  `TENSORFOLD_CUDA_KERNELS` names another (`RUNTIME.md:9-14`,
  `native/cuda.zig:78-83`, `:330-334`). The kernel set is code the engine loads, so it is
  part of the installation's identity.
- `tensorfold-native --version` prints `tensorfold-native 1.0.0` on standard output and
  needs no model, driver or GPU (`server/main.zig:18-22`). `tensorfold-native
  capabilities --json` prints a document with `schema: 1`, `engine: "zig"`, the version,
  the chip, the backends, the families with their weight formats, every flag the binary
  serves with its allowed values, and the environment variables it honours
  (`server/main.zig:34-39`, `server/cli.zig:260-288`). Reading the chip opens the CUDA
  driver and asks the device's compute capability; it creates no context and answers
  `null` without a driver (`native/cuda.zig:24-35`).
- The runbook installs by archive plus checksum and says to keep `bin`, `lib` and `share`
  together (`RUNBOOK.md:59-88`). Upgrading is unpacking a new archive and restarting from
  it (`RUNBOOK.md:129-131`).

### 2. Command line

- `serve MODEL [flags]` parses one flag table (`server/cli.zig:30-92`). Names are matched
  **exactly**; `--name=value` is split at `=`; there is no abbreviation and no `_` to `-`
  folding (`server/cli.zig:136-139`, `:160-162`). 0.6's argparse accepted abbreviations,
  which is why ADR 0023 checks options with the prefix rule.
- A flag the native engine does not serve is refused at start with exit 2 and "`<flag>` is
  not served by the native engine yet; serve this command with --engine python"
  (`server/cli.zig:174-177`). Among them: `--drafter`, `--kv-dtype`, `--tp`, `--rank`,
  `--master`, `--master-port`, `--vision`, `--vision-urls`, `--lane-kernels`,
  `--precision`, `--prefill-fp8` (`server/cli.zig:39-43`, `:61-65`, `:81-88`). So
  CapyCTL's 0.6 command, which renders `--drafter none` unless drafts are named or turned
  off (`crates/capyctl-adapters/src/tensorfold/args.rs:332-334`), fails on 1.0 before it
  loads anything.
- Served flags CapyCTL cares about: `--host`, `--port`, `--name`, `--alias`, `--api-key`,
  `--api-key-file`, `--metrics-open`, `--dashboard`, `--context`, `--speed-up` (two-Mac
  Metal pairing), `--max-tokens`, sampling defaults, `--thinking`/`--no-thinking`,
  `--reasoning-effort`, `--thinking-budget`, `--loop-guard`, `--no-drafts`, `--keep-warm`
  (Metal only, `README.md:110`), `--compact-at`, `--compact-keep`, `--compact-memory`,
  `--prompt-cache-gib` and `--prompt-cache-over-cap` (Flash Next on Metal), `--snapshot-dir
  none` and `--max-snapshots 0` (only those values), `--parallel`, `--no-update-check` ("an
  accepted compatibility switch", `README.md:119`), `--backend` (`auto` or `cuda` on
  Linux), and on the CUDA build `--device` (a GPU ordinal) and `--segments`
  (`server/cli.zig:24-27`, `:31-91`).
- `--parallel auto` is 8; a number is a fixed count, and an engine whose memory fits fewer
  streams refuses rather than serving fewer (`server/cli.zig:237-248`,
  `native/cuda_memory.zig:73-79`). CapyCTL renders a number, so 1.0 fails closed where 0.6
  grew into its budget.
- `--context` bounds prompt plus reply. Unset, Nemotron's CUDA default is 16,384; `0` is the
  model's own window; a value above the window is refused (`families/nemotron/cuda_engine.zig:18`,
  `native/cuda.zig:69-76`, `:294-302`).
- The model is a directory, or a repository id already in the Hugging Face cache. `serve`
  never downloads (`server/hub.zig:22-50`); only `tensorfold pull` does.
- Variables honoured: `TENSORFOLD_API_KEY`, `TENSORFOLD_NO_LIVE`, `TENSORFOLD_SEED_SALT`,
  `TENSORFOLD_REQUEST_LOG`, `TENSORFOLD_NO_UPDATE_CHECK`, `HF_HOME`, `HF_HUB_CACHE`,
  `HF_HUB_OFFLINE`, and on CUDA `TF_CUDA_DEVICE`, `TF_CUDA_SEGMENTS`,
  `TENSORFOLD_CUDA_KERNELS`, `TENSORFOLD_MEMORY_RESERVE_GIB`, `TENSORFOLD_CUDA_MEMORY_LIMIT_GB`
  (`server/cli.zig:94-96`). `TENSORFOLD_REQUEST_LOG` appends every chat and completion body
  to a file "for replays" (`server/request_log.zig:1`).

### 3. Readiness surfaces

- The listener opens only after the engine has loaded the checkpoint, built its CUDA graphs
  and measured its rounds: `main` opens the engine (`server/main.zig:53-59`) before
  `serve.run` binds the socket (`server/serve.zig:132-142`). Until then a connection is
  refused; there is no 503 loading phase on 1.0.
- `GET /health` answers 200 `{"status": "ok", "model": <served name>, "model_ids": [...],
  "max_batch_size": <streams admitted>, "warming": false, "memory": {"active", "cache",
  "peak"}, "live": {"connections", "waiting", "decode_tokens_per_second",
  "prefill_tokens_per_second"}}` (`server/status_routes.zig:13-34`). It has **no `ok`, no
  `busy` and no `requests_running`**. With an API key configured it answers only
  `{"status": "ok"}` (`:14`). `live.connections` is running plus waiting requests, not open
  HTTP connections (`server/live.zig:15-24`). `warming` is a field of the status structure
  that the CUDA lane host never sets (`core/engine_api.zig:169`,
  `core/lane_host.zig:154-174`).
- So CapyCTL's 0.6 reads fail on 1.0: readiness waits for `"ok": true` forever
  (`crates/capyctl-adapters/src/tensorfold/adapter.rs:140-143`), and the idle read reports
  "health has no busy" as an unanswered engine (`tensorfold/http.rs:89-92`,
  `adapter.rs:205`), which the stop path treats as safe to signal at the bound.
- `GET /v1/models` lists the served name and aliases (`server/status_routes.zig:36-51`).

### 4. Work counters, cancellation and stop

- The lane host's status, read under its one mutex, gives `running` (admitted streams) and
  `waiting` (queued jobs) (`core/lane_host.zig:154-174`). `/metrics` renders both from one
  status read as `tensorfold:requests_running` and `tensorfold:requests_waiting`
  (`server/metrics.zig:90-93`, `:112-113`), the same names 0.6 used. It also renders
  `tensorfold:kv_cache_usage_ratio{pool="i"}` per live stream, the
  `request_latency_seconds` and `time_to_first_token_seconds` histograms CapyCTL forwards
  today, `request_time_per_output_token_seconds`, `device_memory_bytes` and
  `device_memory_peak_bytes` on CUDA, `client_disconnections_total`, and vLLM-named mirrors
  (`server/metrics.zig:112-143`). CapyCTL's load parser already prefers the `tensorfold:`
  families over the mirrors (`crates/capyctl-agent/src/load.rs:213-217`).
- The submit queue is unbounded; `Busy` (HTTP 503) comes only from an allocation failure
  (`core/lane_host.zig:124-144`, `server/chat.zig:226-229`, `server/errors.zig:48`).
- A request in its HTTP handler, before it is submitted (template, tokenization), is in
  neither count. That is the in-transit case ADR 0032 names for every engine; SPEC §10 as
  amended by owner decision 6 of 2026-10-21 already settles on an idle reading only a
  request the engine answered.
- Client disconnect: the handler polls its connection every 50 ms and then cancels the
  request (`server/chat.zig:424-452`, the wait at `:438`, the check at `:445`). The engine
  ends a cancelled request between rounds and still reports it finished
  (`core/engine_api.zig:185-186`); the handler waits for that end before it returns
  (`server/chat.zig:408-421`). No request ID that CapyCTL assigns reaches the engine or
  comes back, and there is no abort route (`server/routes.zig:38-66`), so ADR 0032's
  per-request proof is not available. `client_disconnections_total` is a count, which ADR
  0032 §4 rejects as evidence.
- SIGTERM and SIGINT stop accepting; the server then waits at most 2 s for open
  connections and exits 0, even with replies still open (`server/serve.zig:21-29`,
  `:158-165`). CapyCTL drains before it signals, so nothing of its own is open then.

### 5. Memory admission

- On a unified device (GB10) the pool is `/proc/meminfo`'s `MemTotal` and `MemAvailable`;
  on a discrete device, CUDA's counts (`native/cuda.zig:108-128`,
  `native/cuda_memory.zig:81-85`).
- The engine's room is `MemAvailable` less a reserve, and with
  `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` set, at most the cap less what the process already
  holds (`native/cuda_memory.zig:33-46`). The reserve is `TENSORFOLD_MEMORY_RESERVE_GIB`
  (at least 2 GiB), else a tenth of the pool and at least 4 GiB (`:48-56`): about 12 GiB on
  a GB10 [INFERENCE from its memory size; not read on host A].
- Before loading, the checkpoint's `.safetensors` bytes must fit the room; after loading,
  the room left must fit `--parallel` streams of `stream_bytes` each, each sized for the
  full window (`native/cuda.zig:335-356`). Each admitted stream allocates its whole buffer
  in one piece when it starts (`families/nemotron/cuda_state.zig:84-88`). So the peak is the
  model plus `--parallel` full-window streams; nothing grows past what the start admitted.
  This removes the growth ADR 0025 found on 0.6, where the streams' caches grew into the
  budget by use.
- The startup line prints the device, the model's measured bytes, the stream count, each
  stream's bytes at the window and the reserve (`native/cuda.zig:360-362`).
- Per stream, from `families/nemotron/cuda_state.zig:30-37` and
  `families/nemotron/cuda_mtp.zig:109-121` with this checkpoint's `config.json`: the
  window is rounded up to 512 keys after adding 16 verify rows
  (`families/nemotron/cuda_engine.zig:96-97`); each key costs 6,144 bytes in the six
  attention layers (2 KV heads of 128, BF16, keys and values) and 1,024 bytes in the MTP
  head's attention; the 23 Mamba-2 layers' state, convolution and verify buffers are fixed
  at about 64 MiB. Computed (not measured):

  | `--context` | One stream | 8 streams |
  |---|---|---|
  | 16,384 (the engine's default) | 0.18 GiB | 1.40 GiB |
  | 32,768 | 0.29 GiB | 2.28 GiB |
  | 131,072 | 0.94 GiB | 7.53 GiB |
  | 262,144 (the model's window) | 1.82 GiB | 14.53 GiB |

  Scratch buffers, CUDA graphs and the CUDA context come on top and were not computed.

### 6. Models

- The CUDA build registers one family, `nemotron_h`, reading one weight format,
  `mlx-q4g64` (MLX affine 4-bit, group 64) (`native/cuda.zig:11-20`,
  `families/nemotron/cuda_native.zig:11-12`). Any other `model_type` is refused at start:
  "the native CUDA engine has no backend for X checkpoints yet" (`native/cuda.zig:284-291`).
  The format name comes from `config.json` `quantization.bits` and `group_size`
  (`cli/hub.zig:117-128`). Qwen3.8-27B, Bonsai, Gemma 4, Qwen3.6 and DeepSeek-V4 stay on
  the Python line (`RELEASE-NOTES-1.0.0.md:17`).
- Qualified on CUDA: "Nemotron 3.5 Lightning 30B-A3B, MLX affine 4-bit, group 64, included
  MTP head", checkpoint `TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit`
  (`README.md:38-45`). The README and release notes say greedy and sampled
  (`README.md:54`, `RELEASE-NOTES-1.0.0.md:39-41`); the runbook says greedy and lists
  sampling gaps (`RUNBOOK.md:63`, `:87`). The two disagree; the TF rows run both.
- The checkpoint at `d9d758fb83953437f7263256b0d96157e2a348b8` (OpenMDW 1.1, not gated):
  four weight shards and `mtp-4bit.safetensors` (751,638,984 bytes), 18,527,064,363 bytes of
  safetensors (17.25 GiB) and 18,544,234,457 bytes in all. `config.json`: `model_type
  nemotron_h`, 52 layers (23 Mamba-2, 23 MoE, 6 attention), 2 KV heads of 128,
  `max_position_embeddings` 262,144, `quantization` affine 4-bit group 64. It is the same
  revision CapyCTL already runs on TensorFold 0.6.6 under its earlier repository id
  (`docs/examples/deployment-tensorfold.yaml`, `scripts/live/quick-check/remote.sh:34`);
  the earlier id answers the Hub with a redirect to this one.
- Concurrency: on CUDA, Nemotron's concurrent requests share each round
  (`README.md:54`). On 0.6.3 to 0.6.5, Nemotron-H ran one request at a time on CUDA
  whatever `--parallel` said (ADR 0023 §4 amendment).
- Replies carry the `tensorfold` object with `time_to_first_token`, `prefill_seconds`,
  `tokens_per_second`, `token_sha` and drafting counts (`server/chat.zig:501-530`); the
  router's per-request figures already read the first two
  (`crates/capyctl-router/src/engine_metrics.rs:6-8`). Completions return no token ids
  (no `return_token_ids` handling outside the Responses translation), so the completion
  probe answers with text, which a single launch's wake canary accepts (owner decision
  2026-10-09). `cache_salt` is not read anywhere in the server. Unknown request fields are
  ignored [INFERENCE: `server/fields.zig` refuses only named fields such as `n` above 1,
  `logprobs` and media parts; TF3 confirms with the completion probe's fields].

### 7. Parking

The native route table has no sleep, release or unload route (`server/routes.zig:38-66`).
`--keep-warm` keeps a Metal GPU busy while idle; on CUDA the engine has no keepalive target
and the flag does nothing (`server/server.zig:76-86`, `core/engine_api.zig` `keepalive`
"null when the backend is not Metal"). 1.0 is `restart_only` as 0.6 is.

### 8. Network, release check and telemetry

- `serve` makes no outbound connection: the only HTTP client is in `cli/pull.zig`. There is
  no release check in the 1.0 server; `--no-update-check` and `TENSORFOLD_NO_UPDATE_CHECK`
  are accepted and do nothing (`README.md:119`, `server/cli.zig:79`, `:95`).
- No usage reporting, no W3C trace context and no OTLP export: no `traceparent`, OTLP or
  OpenTelemetry string appears in the source.
- A listener on a non-loopback address without a key only warns (`server/serve.zig:91`).

### 9. Later releases

At v1.1.5, `/health` answers 503 `{"status": "unavailable"}` when the engine is not healthy
and a `draining` body while it stops (`server/status_routes.zig:15-21` at v1.1.5), and
`--idle-unload SECONDS` closes the engine after an idle period, the next request reloading
it (`server/cli.zig:95`, `:205` at v1.1.5), beside new listener flags (`--allowed-host`,
`--allowed-origin`, `--allowed-controls`). An engine that unloads itself on its own timer
contradicts SPEC §6.5 (the idle policy is CapyCTL's) and would change what a charge means.
A version number above 1.0 is therefore not the same contract.

### Summary

| | TensorFold 0.6.x | TensorFold 1.0.0 | CapyCTL consequence |
|---|---|---|---|
| Form | Python environment, `bin/tensorfold`, dist-info | archive with `bin/tensorfold-native`, `VERSION`, `SHA256SUMS`, kernel set | a second registration form |
| Build at start | torch extensions, compiler on PATH | none | no toolchain check, no extensions directory, no 1800 s bound |
| Parser | argparse, abbreviations | exact names; unserved flags refused | exact-name policy; `--drafter none` and `--kv-dtype` never rendered |
| Readiness | `/health` `ok`, 503 while loading | no listener until loaded; `/health` `status: ok`, `model_ids` | native readiness read |
| Idle signal | `/health` `busy`, `requests_running` | `/metrics` `requests_running`, `requests_waiting`, one snapshot | native idle read; no `/health` fold in load reports |
| Memory | grows by use inside the cap | admitted in full at start; cap and reserve | charge = model + streams × full window |
| Models on CUDA | many families | `nemotron_h`, `mlx-q4g64` only | refusal from the capabilities document |
| Groups | `--tp 2` across two hosts | none on CUDA | `group_unsupported` |
| Parking | none | none | `restart_only` |
| Release check | on unless turned off | none | the switch stays rendered |

## Decision

### 1. One engine kind, two lines

TensorFold 1.0 stays the engine kind `tensorfold` in every closed set, with one adapter
module, `crates/capyctl-adapters/src/tensorfold/`, that serves two **lines**: `python`
(0.6.x, ADR 0023 as it stands) and `native` (1.0). The line is a property of the
installation, fixed at registration from its form (a Python environment with a
`tensorfold-*.dist-info`, or a release root holding `bin/tensorfold-native`), never from
comparing version strings. It is written to the profile as `line: native` (absent means
`python`, so every existing `engines.yaml` keeps its meaning), carried into the effective
configuration and the frozen revision, and so pinned per deployment: a profile that changes
line changes its build fingerprint, which is drift (ADR 0008).

What the two lines share: the engine kind, restart-only residency, no engine key, the
forwarder (`cache_salt` refused, responses passed through with the `tensorfold` object),
the shared wait before a stop signal (`tensorfold::wait_idle`), the Initialize skeleton
(spawn through the director's tool, watch the process, probe once), the `resources`
requirement and the short form, `--parallel` handling and its default of 8, and the load
report's metric names. What differs, and only there, branches on the line: registration,
rendering and environment, the reserved and sensitive option tables, resolution refusals,
the readiness read, the idle read, the load report's `/health` fold and the start bounds.

*Alternatives:* a new engine kind (`tensorfold-native`), rejected in "Alternatives
considered"; branching on the version number, rejected because 1.1 changes the contract
(Evidence §9) and a 0.6-line build numbered 1.x cannot be ruled out.

### 2. Registration of a binary

The native installation is the archive's root directory, the directory above
`bin/tensorfold-native` (`~/tensorfold-1.0.0` on host A).

- **Detect** finds an executable regular file named `tensorfold-native` in `PATH` entries,
  in `bin/` of each directory directly in the home directory (`~/*/bin`, which covers
  `~/tensorfold-1.0.0`), in `/opt/*/bin`, `/usr/local/bin` and every `--path`, with ADR
  0018 §1's bounds, following no symlink out of its root and executing nothing. The version
  is the root's `VERSION` file (one line, at most 32 bytes, a `MAJOR.MINOR.PATCH`), else
  `unknown` until `add`. The line is `native`.
- **Add** takes the binary, its `bin` directory or the root, resolves lexically, and then,
  because the operator named it:
  1. runs the bounded version check (ADR 0018 §1: 60 s, 4 KiB, cleared environment, own
     process group) reading **standard output** for `tensorfold-native <version>`; a
     `VERSION` file that disagrees is `engine_version_failed`;
  2. when the root holds `SHA256SUMS`, checks every listed file against it (each path
     relative and inside the root, no symlink followed out); a missing or different file
     refuses the installation `engine_unsupported`, naming the first file and saying the
     archive was changed after it was unpacked; a root without `SHA256SUMS` registers and
     `engine list` says the publisher's list was absent;
  3. runs `capabilities --json` under the same bounds (64 KiB) and requires `schema` 1,
     `engine` `"zig"`, `version` equal to the checked version, `cuda` among `backends`, and
     among `serve.flags` every flag CapyCTL renders (`--host`, `--port`, `--name`,
     `--context`, `--parallel`, `--backend` with value `cuda`, `--snapshot-dir` with value
     `none`, `--no-update-check`, `--max-tokens`, `--thinking`, `--no-thinking`,
     `--no-drafts`) and `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` among `env`; anything missing is
     `engine_unsupported`, naming it. The document's `families` are recorded on the profile
     (§7). This replaces the Python capability probe for the native line, which has nothing
     to import (`runtime_integrity::required_files` is empty for it);
  4. measures the ADR 0008 fingerprint (below) and writes the profile with
     `security.deep_park: disabled`; `--deep-park enabled` is refused `capability_missing`,
     as for 0.6.
- **No toolchain check.** Nothing compiles at run time, so `toolchain_missing` never
  applies to the native line and `--cuda-home` is not asked for.
- **Fingerprint** (ADR 0008): the canonical manifest over every regular file under the
  root (the binary, `share/tensorfold/cuda/**`, `lib/**`, `VERSION`, `SHA256SUMS`, the
  notices), bounded as ADR 0008's walk. The kernel set is loaded code and must be covered;
  a digest of the binary alone would miss a replaced cubin. `build_fingerprint` is the
  checked version, `1.0.0`.
- **Profile name.** `engine add` defaults the native line's profile name to
  `tensorfold-native`, so it registers beside a 0.6 profile named `tensorfold` without
  `--name` (open question 6).
- **The role's own engine.** `local_engine.tensorfold`, `--tensorfold-bin` and
  `CAPYCTL_TENSORFOLD_BIN` also take a `tensorfold-native` path; the line follows from the
  file name, and the toolchain check runs only for the Python line.
- **Kind detection on add.** A path ending in `bin/tensorfold` resolves as today (a Python
  environment, or refused when there is none); a file named `tensorfold-native`, or a
  directory holding `bin/tensorfold-native`, is the native line. A `tensorfold` file
  outside a Python environment is refused `engine_unsupported`, saying the native release
  is registered by its `tensorfold-native` binary.

### 3. Accepted versions

The verified set gains `(tensorfold, "1.0.0")` for the native line once TF1 to TF10 pass.
Recommended (open question 1): another 1.0.x patch release registers as `custom`
(unqualified) when its capabilities document passes §2; any native release from 1.1.0 on is
refused at `add` with `engine_unsupported`, naming this ADR, until an amendment reads it.
The Python line keeps its verified set (0.6.0, 0.6.1, 0.6.2, 0.6.3, 0.6.5, 0.6.6).

### 4. Launch

```text
<root>/bin/tensorfold-native serve <model dir> --name <served> --host 127.0.0.1
  --port <port> --no-update-check --backend cuda --snapshot-dir none
  --context <n> --parallel <n> [--max-tokens <n>] [--thinking | --no-thinking]
  [host args] [extra args]
```

- `--context` is always rendered from the required `context_length`; `--parallel` follows
  ADR 0023 §4's amendment unchanged (declared, else 8, else the host's or extra arguments'
  own `--parallel`, never both). A number makes 1.0 refuse rather than serve fewer streams
  (Evidence §2), which keeps the charge honest.
- Nothing is rendered for drafts: the native engine drafts with the checkpoint's own MTP
  head and has no `--drafter`. `--no-drafts` is an ordinary extra argument.
- `--no-update-check` stays rendered, and the environment keeps
  `TENSORFOLD_NO_UPDATE_CHECK=1`, although the 1.0 server has no release check (Evidence
  §8): the switch costs nothing and stays effective if a later 1.0.x adds one.
- **Reserved** on the native line (refused in host and extra arguments, checked by exact
  name, with `--name=value` split at `=`): `--host`, `--port`, `--name`, `--alias`,
  `--backend`, `--context`, `--api-key`, `--api-key-file`, `--metrics-open`,
  `--snapshot-dir`, `--max-snapshots`, `--no-update-check`, `--device` (CapyCTL pins the
  GPU with `CUDA_VISIBLE_DEVICES`, and the engine's default ordinal 0 is the pinned
  device), `--speed-up` (two-Mac Metal pairing, a topology CapyCTL does not run),
  `--dashboard` (an extra page and `/stats` route on the engine's port), and
  `--compact-memory` (it writes each conversation's memory note to a directory, against
  zero data retention, `docs/operations/data-retention.md`).
- **Host approval by name** (sensitive): `--compact-at` and `--compact-keep`. Compaction
  rewrites a conversation's older turns into a note before the model sees them, so a host
  administrator decides whether a deployment may turn it on (open question 4).
- **Typed:** `--max-tokens` (`engine_config.tensorfold.max_tokens`) and `--thinking` /
  `--no-thinking` (`engine_config.tensorfold.thinking`) as on 0.6. `kv_cache_dtype` has no
  native flag (§6).
- **Ordinary:** every other served flag, among them sampling defaults,
  `--reasoning-effort`, `--thinking-budget`, `--loop-guard`, `--no-drafts`, `--segments`,
  and the Metal-only `--keep-warm`, `--prompt-cache-gib` and `--prompt-cache-over-cap`,
  which do nothing on CUDA. A flag the binary does not serve is refused by the binary at
  start; the launch failure names it and says that TensorFold 1.0 does not serve it and a
  0.6 installation may, replacing the engine's "serve this command with --engine python",
  which means nothing to a CapyCTL user (`crates/capyctl-adapters/src/launch_failure.rs`
  learns the native message).
- CapyCTL never renders the option with the prefix rule on this line: the parser cannot
  expand an abbreviation, so exact names are the whole check, and the binary refuses the
  rest.

### 5. Environment

The native line's closed environment (SPEC §13.3) is: `PATH` (the closed system PATH, no
CUDA or installation `bin`, since the binary runs nothing), `HOME`, `CUDA_VISIBLE_DEVICES`,
`CUDA_DEVICE_ORDER`, `HF_HUB_OFFLINE=1`, `TENSORFOLD_NO_UPDATE_CHECK=1`,
`TENSORFOLD_CUDA_MEMORY_LIMIT_GB` (§8), `TENSORFOLD_MEMORY_RESERVE_GIB` if open question 3
is decided that way, `CAPYCTL_ENGINE_LOG`, `DO_NOT_TRACK=1` under the host's
`engine_telemetry: off` (`engine_telemetry.rs`), and the resolved engine environment's
approved names. Dropped from the 0.6 list for this line: `TORCH_EXTENSIONS_DIR`, `CUDA_HOME`,
`MAX_JOBS`, `FLASHINFER_NVCC_THREADS`, `TRANSFORMERS_OFFLINE` and `TF_NCCL_LIB`. Never
carried, and refused as `--approve-env` names on a native profile: `TENSORFOLD_API_KEY`
(CapyCTL owns authentication, ADR 0023 §3), `TENSORFOLD_REQUEST_LOG` (writes every request
body to a file), `TENSORFOLD_CUDA_KERNELS` (would load kernels from outside the measured
installation), `TF_CUDA_DEVICE` (the pinned device is ordinal 0) and `TF_CUDA_SEGMENTS`
(`--segments` is the ordinary spelling).

### 6. Deployment configuration and what is refused

A native-line deployment resolves as a 0.6 one (ADR 0023 §4: `context_length` required,
`resources` required, the short form, `max_concurrent_requests` as `--parallel`), with these
refusals at resolution, each naming TensorFold 1.0 and the fix:

- `kv_cache_dtype`: `capability_missing`, "TensorFold 1.0 has no KV cache type option; its
  KV cache is BF16".
- `model.draft`, and `--drafter` in any arguments: `capability_missing`, "TensorFold 1.0
  drafts with the checkpoint's own MTP head".
- a `topology` (groups, ADR 0028; several GPUs, ADR 0035): `group_unsupported`.
- `api: decisions`: `capability_missing:decisions`, as today (ADR 0030); 1.0's
  `/v1/decisions` checks requests but scores no labels yet (`RELEASE-NOTES-1.0.0.md:23`).
- `api: embeddings` if ADR 0036 lands: refused; 1.0 has no embeddings route.
- `residency` `deep` or `host_backed`: `capability_missing`, as today.
- the checkpoint (§7).

The derived start bound is ADR 0014 A1 and A7's (weights-based), not
`TENSORFOLD_FIRST_BUILD_MS`; the request deadline gets no 1800 s floor; a `restart_only`
wake gets the Initialize window (A23). TF2 measures the cold and warm starts.

### 7. Models: what is refused

At `engine add` the profile records the capabilities document's `families` (for 1.0.0 on
Linux: `nemotron_h` reading `mlx-q4g64`). Resolution reads the checkpoint's `config.json`
`model_type` and `quantization` (`bits`, `group_size`, `mode`) and:

- refuses a checkpoint whose family or format the installation does not list,
  `capability_missing:model_family`, naming both and saying that the 0.6 line serves other
  families; the binary would refuse it at start anyway, after CapyCTL had reserved memory
  and spent a launch;
- accepts the qualified checkpoint,
  `TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit@d9d758fb83953437f7263256b0d96157e2a348b8`
  (its earlier repository id, which redirects, resolves to the same revision and is the
  same checkpoint), as qualified once TF2 passes;
- accepts any other checkpoint of a listed family and format (a fine-tune, a later revision)
  as **unqualified**: it runs, and `capyctl status deployment` and `validate config` say
  "TensorFold 1.0 qualifies only Nemotron 3.5 Lightning at revision d9d758f on a GB10"
  (open question 2).

On a discrete GPU, or a GB10 other than host A's class, a 1.0 deployment runs unqualified,
as ADR 0023's Consequences say for 0.6.

### 8. Memory: the charge, the cap and the reserve

- **The charge** stays the operator's `resources` (ADR 0023 §4, ADR 0025). Because 1.0
  admits every stream at its full window at start (Evidence §5), the figure to declare is
  the model plus `--parallel` streams at `context_length`, plus scratch, graphs and the CUDA
  context, plus host RAM. For the qualified checkpoint at `context_length: 32768` and 8
  streams the computed part is 17.25 GiB of weights and 2.28 GiB of streams
  [INFERENCE: the rest is measured by TF2]. The TF rows run with the example's
  `resources: {gpu: 30GiB, ram: 2GiB}`, the figure CapyCTL already declares for this
  checkpoint on 0.6.6, and the recipe and guide figure for 1.0 is set from TF2's measured
  peak with the margin TF2 shows, never below it.
- **The cap**: `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` is rendered as on 0.6.3 and later: the
  declared Ready allocation on the GPU's memory domain (on a GB10, the sum of `gpu` and
  `ram`), in GiB. On 1.0 it bounds the room to the cap less what the process holds, and a
  launch whose model and streams do not fit fails at start with TensorFold's message naming
  `--parallel` or `--context`, which CapyCTL's launch failure maps to "lower
  `max_concurrent_requests` or `context_length`, or raise `resources`" (ADR 0023 §4).
  The CUDA context and driver allocations lie outside what the engine counts; TF2 compares
  `device_memory_peak_bytes` with CapyCTL's per-process reading of the engine to measure
  that difference, and if it is not small the rendered cap is the declared allocation less
  the measured difference.
- **The reserve**: the engine also holds back its own reserve from `MemAvailable`, about
  12 GiB on a GB10 by default (Evidence §5). CapyCTL already holds the host's reserve in
  its ledger, so the default can refuse a launch CapyCTL admitted when the pool is nearly
  full. Recommended (open question 3): render `TENSORFOLD_MEMORY_RESERVE_GIB=2`, the
  smallest value the engine takes, after TF4 confirms both behaviours.

### 9. Readiness and start

Ready on the native line is: a connection accepted (refused means not yet: the engine is
still loading), `GET /health` 200 with `status` `"ok"` and the served name in `model_ids`,
the served name in `/v1/models`, then the chat probe, where a non-empty `content` or
`reasoning_content` is an answer. SPEC §8.2 (reserved values verified after the engine's
parser): `/health` `max_batch_size` must equal the rendered `--parallel`; a difference is
`effective_args_mismatch` and the coordinator stops the launch on its recorded process.
The startup line (Evidence §5) is kept in the engine log, and `capyctl status deployment`
shows its stream figures when the log holds them.

### 10. Idle, drain, stop and cancelled requests

- **The idle signal** on the native line is one `/metrics` read with
  `tensorfold:requests_running` and `tensorfold:requests_waiting` both 0, as llama.cpp's
  (ADR 0029 §10). Both come from one status snapshot, so there is no disagreement rule to
  apply. A connection refused (the engine exited or is still loading) is not listening and
  may be signalled at once; a `/metrics` that does not answer may be signalled when the
  bound ends (30 s or the command's remaining time); an engine that still reports work at
  the bound is not signalled and the cleanup is uncertain (ADR 0023 §6, unchanged).
- **Stop** is the restart-only release: drain, the idle read above, SIGTERM to the owned
  process, verified exit, then release. 1.0 exits within about 2 s of SIGTERM
  (Evidence §4); TF5 measures it.
- **Cancelled requests** keep today's rule (ADR 0032 item 6): the router closes the engine
  connection, `cancel_work` answers `Uncertain`, and a `cancelling` request settles on an
  idle reading taken after the hang-up only if the engine answered it (owner decision 6 of
  2026-10-21), else at the instance's next verified stop; requests held until the next stop
  that fill the deployment's 32 places restart the instance (ADR 0032 item 7). 1.0 offers no
  per-request evidence: no request ID of CapyCTL's reaches it, no abort route, no
  acknowledgement, and its disconnection counter is a count (Evidence §4).
- The guide's abort table (`docs/guide/requests.md`, "How quickly each engine stops")
  gains a TensorFold 1.0 row measured by TF6, with "`/metrics`:
  `tensorfold:requests_running`, `tensorfold:requests_waiting`" as the count read.

### 11. Load reports and per-request figures

The host scrapes the same `tensorfold:` families as on 0.6 (`requests_running`,
`requests_waiting`, `kv_cache_usage_ratio` per pool) and forwards the same two histograms.
On the native line the sample is **not** folded with `/health`
(`crates/capyctl-agent/src/load.rs:482-494` applies to the Python line only): 1.0's `/health`
carries no `busy`, so the fold would keep every sample busy and no remote cancelled request
would ever settle on an idle reading. `device_memory_bytes` and `device_memory_peak_bytes`
are read by the TF rows, not used for accounting. Per-request figures (SPEC §17) come from
the `tensorfold` object as today (`time_to_first_token`, `prefill_seconds`); TF7 checks
them on 1.0.

### 12. Parking

`restart_only`, as 0.6: `deep` and `host_backed` are refused `capability_missing`,
`capyctl park deployment` is refused as unsupported, parking is the restart-only release and
waking is a fresh launch of the same pinned effective contract. `--keep-warm` does nothing
on CUDA. Should a later release offer an unload and reload that answers when the memory is
free, a deep-park ADR reads it then, with ADR 0012's protections; 1.1's `--idle-unload` is an
engine-side timer and is not that (Evidence §9).

### 13. LMCache, groups, several GPUs, trace context

- **LMCache stays off** for TensorFold 1.0: ADR 0033 §2's row ("TensorFold",
  `engine_unsupported`, "TensorFold has no LMCache integration") covers both lines. 1.0 has
  no KV connector, and nothing changes unless a later ADR qualifies one.
- **Groups and several GPUs**: none. 1.0 has no `--tp` on CUDA (Evidence §2), so a `topology`
  is refused (§6); ADR 0028 §10 applies to the Python line only. ADR 0035's question about
  TensorFold 1.0 is answered: one GPU.
- **Trace context**: 1.0 reads no `traceparent` and exports nothing; CapyCTL's own span is
  all there is, as ADR 0037 proposes for TensorFold.

### 14. Listener check

The native line runs ADR 0031 §7's loopback check after the probe passes and whenever a
host re-proves the launch: any TCP listener of the owned process on an address other than
`127.0.0.1` or `::1` stops the launch with gone evidence and fails it, reusing the custom
engine's code under a TensorFold reason (`tensorfold_listener_exposed`). CapyCTL renders the
listener itself, so this is defence in depth for a new binary whose other network paths
(`--speed-up` links, the dashboard) CapyCTL has not run (open question 5).

### 15. Host capability

A host that runs native-line launches declares `tensorfold_native` in its session (ADR
0017). Placement refuses a native-line deployment on a host without it
(`host_capability_missing:tensorfold_native`), so an older agent never renders a 1.0
installation with the 0.6 command.

### 16. Codes

No new exit code. Registration: `engine_unsupported` (17) for a changed archive, a
capabilities document missing what CapyCTL renders, a native release from 1.1.0 on, or a
`tensorfold` file outside a Python environment; `engine_version_failed` (18) for a version
line that cannot be read or disagrees with `VERSION`. Resolution: `capability_missing`
(`kv_cache_dtype`, `model.draft`, parking), `capability_missing:model_family`,
`capability_missing:decisions`, `group_unsupported`. Placement:
`host_capability_missing:tensorfold_native`. Launch: `effective_args_mismatch`
(`max_batch_size`), `tensorfold_listener_exposed`.

### 17. Texts that change with the implementation

- SPEC §9.3, after its last paragraph: "*Amended by ADR 0039:* TensorFold 1.0 is a native
  binary registered from its release directory (`bin/tensorfold-native`), without a build
  toolchain. It serves only the families its capabilities document lists (on CUDA,
  Nemotron-H in MLX 4-bit, group 64), one GPU and no group, `restart_only`. It is ready on
  `/health` `status: ok` with the served name, and idle when `/metrics`
  `tensorfold:requests_running` and `tensorfold:requests_waiting` are both 0. It admits
  every stream at its full window at start, so its declared resources cover the model and
  `max_concurrent_requests` full-length streams."
- SPEC §10, the amendment of 2026-10-01: "TensorFold from `/health` `requests_running: 0`
  and `busy: false`" becomes "TensorFold 0.6 from `/health` `requests_running: 0` and `busy:
  false`, TensorFold 1.0 from `/metrics` `requests_running` and `requests_waiting` both 0".
- SPEC §20, T41 gains: "the native line: registration from a release directory (version on
  standard output, `SHA256SUMS`, the capabilities document, the digest over the whole
  root), exact-name reserved options, refusals of `kv_cache_dtype`, drafters, topologies and
  unlisted families, readiness from `/health` `status` and `model_ids` with
  `max_batch_size` verified, the `/metrics` idle signal, no `/health` fold in load reports,
  the loopback check (ADR 0039)".
- ADR 0023 gains a pointer to this ADR in each of §1 to §8.
- Guide: `docs/guide/engines.md` ("TensorFold": the two lines, registering 1.0, what 1.0
  refuses, the memory figure; "First model on each engine" gains TensorFold 1.0.0 with
  Nemotron 3.5 Lightning), `docs/guide/requests.md` (the field table's TensorFold column
  read at 1.0.0, the abort table's row), `docs/guide/engine-flags.md` (the native line's
  reserved and sensitive options), `docs/guide/errors.md` (the codes of §16),
  `docs/operations/configuration.md` (`line` in a profile), a new
  `docs/examples/deployment-tensorfold-native.yaml`.
- `AGENTS.md`: the approved environments list gains TensorFold 1.0.0 at `~/tensorfold-1.0.0`
  on host A (owner decision 2026-10-25), and the current-phase line names the native line
  once TF rows pass.

## Consequences

- TensorFold deployments keep `engine: tensorfold` in their files whichever line the
  profile has; switching a deployment from 0.6.6 to 1.0 is pointing it at the native
  profile, a new revision.
- A second installation form joins the dist-info one and the bare llama.cpp binary: a
  release root with a version file, a publisher's checksum list and a capabilities
  document, executed twice at `add` (version, capabilities) and never during detection.
- The native line needs no compiler, no extensions directory and no lock cleanup; its
  first start is not a build, so its start bound is the ordinary one.
- 1.0 fails closed on memory: a deployment whose declared resources cannot hold the model
  and its full-length streams refuses to start instead of stopping requests later.
- Nemotron 3.5 Lightning on a GB10 is the only qualified 1.0 model; everything else
  TensorFold serves stays on 0.6.6, and 0.6.x keeps every behaviour it has.
- Load reports from a 1.0 engine can read idle, which lets cancelled requests settle on a
  host as they do on the router's machine.
- Out of scope: native releases from 1.1.0 on, Metal and macOS, two-Mac pairing
  (`--speed-up`), context compaction by default, embeddings, decisions, groups and several
  GPUs, LMCache, discrete-GPU qualification.

## Alternatives considered

- **A new engine kind, `tensorfold-native`.** Clean separation, but every closed set
  (configuration, protocol, store, groups, refusals, CLI) gains a member, deployments and
  recipes change their `engine`, and the request path, residency and load-report code are
  the same. The differences are confined to a handful of functions that branch on a
  recorded line.
- **One adapter that branches on the version number** (`>= 1.0`). Simpler to write, but 1.1
  changes the contract (health 503 states, engine-side unloading), and the form of the
  installation, not its number, says how to start and read it.
- **Detect 1.0 from a dist-info.** There is none: a pip install of 1.0 stops by design.
- **Keep the 0.6 adapter and adapt the engine** (a wrapper serving `ok`/`busy`). A shim
  CapyCTL would have to maintain against an engine that already reports the same counters
  under `/metrics`.
- **Let the binary refuse unlisted families.** It does, but only after CapyCTL reserved
  memory and spent a launch; the capabilities document lets resolution refuse first.
- **Refuse every checkpoint but the pinned revision.** Safest, but it blocks a fine-tune
  of the same architecture and format that the engine serves; the unqualified note says the
  same thing without blocking it (open question 2).
- **Idle from `/health` `live.connections`** (running plus waiting). Equivalent in value,
  but its name says connections and the route also carries readiness; `/metrics` names the
  two counters and is what load reports already read.
- **Accept every native 1.x as `custom`.** Matches the general rule for unverified
  versions, but 1.1's `--idle-unload` would let an extra argument make the engine drop its
  model behind CapyCTL's accounting.

## Implementation plan (target 0.2.0)

1. **Configuration.** `TensorfoldLine` (`python`, `native`) and the profile field `line`
   (`crates/capyctl-config/src/registration.rs`, profile parsing and publication); the
   native reserved, sensitive and typed tables with exact-name matching
   (`engine_policy.rs`); the native refusals of §6 and the family check of §7
   (`effective/engine_config.rs`, `effective/core.rs`); the start bound and request
   deadline without the TensorFold first-build terms for the native line
   (`effective/timeouts.rs:216-217`, `effective.rs:1896-1899`); the verified set gains
   `(tensorfold, "1.0.0")` once the rows pass, and the 1.1 refusal.
2. **Registration.** `tensorfold-native` scanning and the `VERSION` read
   (`crates/capyctl-agent/src/engines/detect.rs`); resolution of a binary, its `bin` or its
   root, the standard-output version check (`engines/resolve.rs`); `SHA256SUMS`
   verification, the capabilities document and the root digest
   (`installation.rs`); `required_files` empty for the native line
   (`runtime_integrity.rs`); no toolchain check, the default profile name and the kind
   checks (`crates/capyctl-cli/src/engine.rs`, `roles.rs`, `grammar.rs` help text).
3. **Adapter.** `tensorfold/native.rs` (render and environment for the native line);
   `http.rs` gains the native `/health` read (`status`, `model_ids`, `max_batch_size`) and
   the `/metrics` gauges; `adapter.rs` branches readiness and `idle_before_signal` on the
   line; `initialize.rs` skips the extensions directory and verifies `max_batch_size`;
   `launch_failure.rs` reads the native refusal and memory messages; the completion probe
   accepts a text-only answer (it already does).
4. **Agent.** `load.rs` folds `/health` for the Python line only; the native launch skips
   the `TORCH_EXTENSIONS_DIR` lock cleanup (`native_execution.rs:1386-1390`); the listener
   check reused from the custom engine (`native_execution`), with its reason.
5. **Protocol.** The host capability `tensorfold_native` and its placement refusal
   (`crates/capyctl-protocol/src/capabilities.rs`).
6. **Fake engine.** A native mode in `crates/capyctl-testkit/src/fake_engine.rs`: no
   listener until "loaded", the 1.0 `/health` and `/metrics` shapes, exact-name parsing
   that refuses unserved flags.
7. **Docs.** The texts of §17; one change file with the release note.

Tests, tagged **T41** (with T17, T21, T29 and T37 where they apply): detection lists a
`tensorfold-native` with its `VERSION` and executes nothing; `add` reads the version from
standard output, refuses a `VERSION` mismatch, a changed file under `SHA256SUMS`, a
capabilities document missing a rendered flag or the cap variable, a 1.1.0 binary and a
stray `bin/tensorfold`; the digest changes with a cubin; no toolchain is asked; the 0.6
profile and the native one coexist; the native command renders exactly as §4, never
`--drafter`, `--kv-dtype` or the prefix rule, and refuses each reserved option by exact
name and with `=value`; the closed environment holds exactly §5's names; `kv_cache_dtype`,
`model.draft`, a `topology`, `api: decisions`, a parking residency and an unlisted family or
format are refused with their codes; an unlisted checkpoint of a listed family runs with the
unqualified note; readiness waits through refused connections and needs `status` `ok` and
the served name; a different `max_batch_size` is `effective_args_mismatch`; the idle read
signals on 0 and 0, holds back on work at the bound, signals a refused connection at once
and an unanswered `/metrics` at the bound; load reports from a native engine read idle
without `/health`; a 0.6 engine still folds `/health`; an old host never receives a native
launch. CPU and Fake-engine tests are not qualification.

## Live qualification (host A, TensorFold 1.0.0)

Run in a live session the owner schedules (one live session on the hosts at a time), on
host A, with the installation at `~/tensorfold-1.0.0` as it is and the approved TensorFold
0.6.6 environment for the comparison rows, and the checkpoint
`TensorFold/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-MLX-4bit@d9d758fb83953437f7263256b0d96157e2a348b8`
(already in CapyCTL's model store from the 0.6.6 rows), `context_length: 32768`, the default
8 streams and `resources: {gpu: 30GiB, ram: 2GiB}` unless a row says otherwise. Each row
records the CapyCTL commit, the installation digest and the `/health` and startup-line
figures.

| Row | What it shows |
|---|---|
| TF1 | Registration: `engine detect` lists `~/tensorfold-1.0.0/bin/tensorfold-native` at 1.0.0 from `VERSION`; `engine add` checks the version on standard output, every `SHA256SUMS` line and the capabilities document, writes `line: native`, deep park disabled, no toolchain asked, profile `tensorfold-native`, and the 0.6.6 profile stays listed beside it; `engine list` shows the digest, `verified` once the rows pass, and "LMCache off: TensorFold has no LMCache integration". |
| TF2 | Start and memory: the rendered command is exactly §4's; ready after the connection opens, `/health` `status: ok` with the served name and `max_batch_size` 8; cold and warm start times; the startup line's model bytes, stream count and stream bytes (compared with 0.29 GiB computed here); peak `device_memory_peak_bytes` under 8 concurrent full-context requests, CapyCTL's per-process reading of the engine and the pool's `MemAvailable` drop, which together set the recipe figure and say whether the cap needs the context's difference taken off (§8). |
| TF3 | Requests: chat streamed and collected, thinking on and off, greedy and sampled (the docs disagree), `reasoning_content`, the `tensorfold` object passed through, `cache_salt` refused, a `--no-drafts` deployment answering the same greedy tokens (`token_sha`) as a drafted one; decode tokens per second at 1 and 8 streams against 0.6.6 on the same requests. |
| TF4 | Refusals and admission: `kv_cache_dtype`, `model.draft`, a `topology` and a parking residency refused at resolution; a Qwen3-4B BF16 checkpoint refused `capability_missing:model_family` before any launch; `max_concurrent_requests: 16` at a 262,144-token context under the 32 GiB cap (17.25 GiB of weights and 16 streams of 1.82 GiB) refused by the engine with the fix shown; with another deployment holding memory so that the pool's free memory is between the engine's default reserve and its need, the launch with the default reserve and with `TENSORFOLD_MEMORY_RESERVE_GIB=2` (open question 3). |
| TF5 | Idle gate and stop: a stop under 8 running streams waits for CapyCTL's drain, reads the two gauges at 0, signals, and the process is gone within the measured time (expected about 2 s); a stop with the engine still busy at the 30 s bound is not signalled and reported uncertain; `capyctl park deployment` refused. |
| TF6 | Cancelled requests: the "How quickly each engine stops" protocol (four long generations, clients killed once the gauges read four, the gauges read every 100 ms), streamed and not, three repeats each, for the guide's table; a request cut before any output stays `cancelling` until the next verified stop; 32 such requests restart the instance (ADR 0032 item 7), journaled, and the next request is served. |
| TF7 | Host path: host A as a host of a server on the control host; load reports read idle when the engine is idle and busy under load, with `kv_cache_usage_ratio` per pool and both histograms forwarded; a cancelled answered request settles on the remote idle reading; per-request `ttft_ms` and `prefill_ms` from the `tensorfold` object. |
| TF8 | Switching and wake: on host A, the 1.0 deployment and a vLLM 0.32.0 deployment that do not fit together; a request for each in turn switches the pool per SPEC §10, the 1.0 wake is a fresh launch within the Initialize window, and the single-launch wake canary passes on the completion probe's text (no token ids). |
| TF9 | 0.6.6 unchanged: the same checkpoint on the 0.6.6 profile registers, starts, reads `/health` `ok`/`busy`, drains and stops exactly as before this change. |
| TF10 | Listener check: the owned process listens only on `127.0.0.1:<port>` after the probe and after a re-proof; no other socket is open. |

## Open questions for the owner

1. **Other native versions.** 1.0.x patches and 1.1.x exist; 1.1.5 changes `/health` and
   adds `--idle-unload`. Options: (a) only 1.0.0; every other native version refused at
   `add`; (b) every native version registers as `custom`, gated by the capabilities
   document; (c) 1.0.x patches register as `custom` when the capabilities document passes,
   1.1.0 and later are refused until an amendment reads them. Recommendation: (c): a patch
   is unlikely to change the interface, and the document and the readiness checks catch it
   if it does, while 1.1 is known to change it.
2. **Checkpoints other than the qualified one.** Options: (a) refuse everything but the
   pinned revision; (b) refuse families and formats the installation does not list, run
   any other checkpoint of a listed family and format unqualified with a note in status
   (§7); (c) refuse nothing and let the binary fail the launch. Recommendation: (b): it
   refuses early exactly what the engine cannot load, and an unqualified note is how CapyCTL
   already treats unverified combinations.
3. **TensorFold's own memory reserve.** By default the engine keeps about 12 GiB of a GB10
   free beyond CapyCTL's accounting. Options: (a) leave the default, accepting that a
   nearly full pool refuses launches CapyCTL admitted; (b) render
   `TENSORFOLD_MEMORY_RESERVE_GIB=2`, the engine's minimum, since the host's reserve is
   already in CapyCTL's ledger; (c) render the host's own reserve figure. Recommendation: (b),
   confirmed by TF4; the cap still bounds the engine.
4. **Context compaction.** 1.0 can rewrite a conversation's older turns into a note
   (`--compact-at`, `--compact-keep`) and keep the notes on disk (`--compact-memory`).
   Options: (a) `--compact-at` and `--compact-keep` need host approval by name,
   `--compact-memory` reserved (§4); (b) all three reserved in 0.2.0; (c) all three
   ordinary. Recommendation: (a): turning compaction on is an administrator's choice about
   what the model sees, while notes written to disk contradict zero data retention.
5. **Listener check on the native line.** Options: (a) run ADR 0031 §7's check on every
   native launch (§14); (b) rely on the rendered `--host` as for 0.6 and llama.cpp.
   Recommendation: (a): it costs one read of `/proc` after the probe, and the binary is new
   to CapyCTL.
6. **Default profile name.** Options: (a) `tensorfold-native`, beside a 0.6 `tensorfold`
   (§2); (b) `tensorfold` when free, else the operator passes `--name`. Recommendation: (a):
   both lines are supported at once, and the binary's own name says which line a profile is.
7. **Which line the docs recommend for Nemotron 3.5 Lightning on a GB10.** Options: (a)
   0.6.6 stays the documented TensorFold path and 1.0 is an option; (b) once TF1 to TF10
   pass, 1.0.0 is the documented path for that model on a GB10 and 0.6.6 for every other
   TensorFold model. Recommendation: (b): no build at start, memory that fails closed, and
   the release notes report 1.01 to 1.10 times 0.6.6's decode on the same machine (TF3
   checks it).
8. **Host B.** Options: (a) host A only, as decided; (b) a byte-identical copy on host B
   for a second machine of the same class (no groups exist on 1.0). Recommendation: (a): one
   GB10 qualifies the class, and the copy would need its own approval.
