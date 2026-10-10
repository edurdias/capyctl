# Status: llama.cpp guide and first live rows on a discrete GPU (plan slice L6) — 2026-10-09 (branch `feat/llamacpp-l6-docs-live`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md), plan slice L6
(`docs/plans/2026-10-09-llamacpp-engine.md`). The user guide gains llama.cpp, and
the first live rows ran on a maintainer's laptop (one 16 GiB discrete NVIDIA GPU,
CUDA 13.4 toolkit already installed, nothing installed system-wide) in a standalone
role with its own state directory and ports. No lab host was used.

## Docs

`docs/guide/engines.md` (a llama.cpp section: registration, the typed fields, the
derived memory, restart-only, what is refused and why), `docs/guide/install-engines.md`
(building `llama-server` from the tag with `-DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF`),
`docs/guide/engine-flags.md` (the llama.cpp v0.6.0 table, now checked by
`engine_flag_table.rs`, which reads llama.cpp's typed refusal `ReservedField` as
typed), `docs/guide/errors.md` (`engine_config_file`, `effective_args_mismatch`,
llama.cpp in `engine_not_found`, `engine_unsupported`, `capability_missing`),
`docs/guide/requests.md` (`cache_salt`), `docs/operations/install.md` (registering a
`llama-server` binary), `docs/operations/configuration.md` (`local_engine.llamacpp`,
`engine_config.llamacpp`), `docs/examples/deployment-llamacpp.yaml` (validated by
`every_documented_example_passes_validate_config`), the engine lists of the guide
pages, the site's platform list and `capyctl --help`.

## Live rows (laptop, standalone)

Binary: a source build of tag `v0.6.0` (commit `d81235049`) with
`-DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF -DCMAKE_CUDA_ARCHITECTURES=89`;
`llama-server --version` prints `version: 0.6.0 (build 1, commit d81235049)` on
standard error. Model: `Qwen/Qwen3-4B-GGUF` revision
`bc640142c66e1fdd12af0bd68f40445458f3869b`, `Qwen3-4B-Q4_K_M.gguf` (2 497 280 256
bytes, sha256 `7485fe6f…fdf5`), deployed with `context_length: 8192` and the default
4 slots, no `resources`.

| Check | Derived or expected | Measured |
|---|---|---|
| LC1 `engine add` | llama.cpp 0.6.0, fingerprint `<v>+<commit>` | `Registered llamacpp (llamacpp 0.6.0)`, `0.6.0+d81235049`, custom `yes`, deep park disabled; the version check took 0.89 s (CUDA initialisation included) |
| Start to Ready, first deploy | within the Initialize window | 7.3 s for `deploy --activate --wait`, the 2.5 GB checkpoint digest included; llama-server listening 1.37 s after its start |
| Start to Ready, `start` after `stop` | a fresh launch | 2.2 s, generation 2 |
| Rendered command | ADR 0029 §5 | `--ctx-size 32768 --parallel 4 --no-kv-unified --gpu-layers all --cache-type-k f16 --cache-type-v f16 --fit off --cache-ram 0 --no-context-shift --metrics --slots --offline --no-webui --cors-origins localhost --no-cors-credentials`; readiness found `n_slots = 4, n_ctx_slot = 8192` |
| KV cache | 4 × 8192 × 36 × 8 × (128 × 2 + 128 × 2) = 4 831 838 208 B = 4608.00 MiB | `llama_kv_cache: CUDA0 KV buffer size = 4608.00 MiB` (equal) |
| GPU memory | derived 8 921 023 708 B (8507 MiB) | 7342 MiB for the process at its peak and while serving (model 2375.91, KV 4608.00, compute 85.01, about 273 CUDA context); 1165 MiB (16 %) under the request |
| Host memory | derived `ram` 4 GiB (provenance `default`) | resident set 877 MiB |
| Stop (SIGTERM) | exit and full release | `stop` returned in 0.15 s, the process exited 0.51 s after the command, GPU back to its 113 MiB baseline |
| Park | refused, restart-only | `capyctl park deployment` exit 5, `unsupported_capability` |
| Chat, non-streaming | answer with `timings` and figures | `content` and `reasoning_content`; after the fix below, `timings` relayed and `capyctl.metrics` `prefill_ms`, `decode_tokens_per_second` (engine), `cached_tokens`, `ttft_ms` |
| Chat, streaming | deltas within `STREAM_DELTA_FIELDS` | deltas `role`, `content`; `timings` on the last chunk; with `stream_options.include_usage` that chunk also has `usage`; the `x-capyctl-metrics` comment carries the engine's `prefill_ms` and decode rate either way |
| Tool call and reasoning | relayed | `tool_calls` (`get_weather`, `{"city": "Lisbon"}`, `finish_reason: tool_calls`) and `reasoning_content`, non-streaming and streaming (deltas `role`, `content`, `reasoning_content`, `tool_calls`) |
| Load during 3 streams | running 3, KV usage from `/slots` | `/metrics` `requests_processing 3`; `/slots` 5081 bytes; after the fix below the management load read shows `running 3, waiting 0, kv_usage_ppm 35950`, fresh |
| Client hang-up mid-decode | `requests_processing` 0 promptly | 0 at the first read after the cut (under 0.1 s) |
| Client hang-up mid-prefill (6.6k-token prompt) | 0 promptly | 0 within 0.5–0.8 s |

The design note's live-check list, item by item: 1, start time — above (laptop
only). 2, peak and steady memory against the request — above: the process holds what
it allocates at start, so peak equals steady; the derived request is 16 % above it
and the `ram` default is about 4.7 times the resident set. The margin and the `ram`
figure are not recomputed from one small model; that waits for a larger model and
the GB10 row. 3, the KV formula — equal to llama.cpp's own line. 4, SIGTERM — above.
5, stream shapes — above. 6, hang-up — above, mid-prefill included. 7, `/slots` KV
usage under load — above. 8, the MTP context's cache with `draft-mtp` and a quantized
V cache with flash attention `auto` — not checked (Qwen3-4B has no MTP heads). 9, a
prebuilt `b11429` binary's version line — not checked (source build). 10, whether the
version check needs CUDA initialisation time — 0.89 s on this CUDA build, inside the
bound.

## Defects found live and fixed

- **A non-streaming answer lost llama.cpp's `timings`.** CapyCTL collects a
  non-streaming request from the engine's stream; `assemble`
  (`capyctl-adapters/src/forward.rs`) kept `tensorfold` and `metrics` from the final
  chunk but not `timings`, so the answer had no `timings` and `capyctl.metrics` had
  no `prefill_ms` and the router's decode rate. `timings` is now kept. Test:
  `a_collected_response_keeps_the_llamacpp_timings` (failed before).
- **Standalone reported no load for llama.cpp.** The embedded load sampler
  (`capyctl-controller/src/embedded_load.rs`) left out a launch with no recorded
  inference key, and an embedded llama.cpp launch has none (ADR 0029 §4), so the
  load read stayed `sample: null`. `LoadTarget.native` is now optional and a keyless
  target is scraped without an `Authorization` header (`capyctl-agent/src/load.rs`);
  a host's ingress targets still always carry their key. Test:
  `a_target_without_a_key_is_scraped_unkeyed` (`crates/capyctl-agent/tests/load.rs`).

## Still open

- `VERIFIED` does not gain `(llamacpp, "0.6.0")`: the plan's acceptance is LC1–LC6
  and two parts did not run on the laptop. LC4 (a llama.cpp and a vLLM deployment
  switching under memory pressure) needs a vLLM environment on the same machine,
  and the laptop has none. LC6's `--spec-type draft-mtp` needs a GGUF model with MTP
  heads. Every llama.cpp build still lists as `custom`.
- The GB10 row (after the owner approves a lab-host build and it is added to
  AGENTS.md's list): start time, peak and steady memory on unified memory, whether the
  default mmap load keeps a page-cache copy beside the device copy, and the prebuilt
  `b11429` arm64 archive's version line.
- The margin and the `ram` figure in `context_fit/llamacpp.rs`, recomputed from the
  GB10 row and a larger model; the live row scripts `scripts/live/matrix/rows/LC1.sh`
  to `LC6.sh` for the lab harness.
- `capyctl park deployment` on llama.cpp says only `unsupported_capability:
  Requested capability is unavailable`; the guide explains why, the message does not.

CPU and Fake-engine tests are not qualification. The laptop rows above are live
evidence for one discrete GPU and one small model only.

# Release note: llama.cpp

- **llama.cpp's `llama-server` as a fourth engine.** Register a `llama-server` built
  from llama.cpp v0.6.0 with `capyctl engine add <path to llama-server>` and deploy a
  GGUF model directory with `engine: llamacpp`. CapyCTL reads the GGUF header and
  derives the memory from the weights and a full-window KV cache for each of the
  deployment's slots (`max_concurrent_requests`, 4 by default). A llama.cpp model
  does not park: when CapyCTL needs the memory, it stops it and starts it again on
  the next request. Answers carry llama.cpp's `timings` and CapyCTL's per-request
  figures; tool calls and reasoning come from the model's chat template. Every
  llama.cpp build lists as custom for now. See
  [llama.cpp](../guide/engines.md#llamacpp).
