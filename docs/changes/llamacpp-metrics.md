# Status: llama.cpp per-request figures and load reports (plan slice L5) — 2026-10-09 (branch `feat/llamacpp-l5-metrics`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §11, plan slice L5
(`docs/plans/2026-10-09-llamacpp-engine.md`). The shapes were read from the v0.6.0
tag (`tools/server/server-task.cpp`, `server-common.cpp`, `server-chat.cpp`,
`server-context.cpp`); nothing ran against a llama-server.

- **Per-request figures** (`capyctl-router/src/engine_metrics.rs`). `prefill_ms` from
  `timings.prompt_ms`, `decode_tokens_per_second` from `timings.predicted_per_second`
  (positive only), `cached_tokens` from `usage.prompt_tokens_details.cached_tokens`;
  `ttft_ms` is the router's and `queue_ms` is absent. The router's decode rate counts
  `usage.completion_tokens`, else `timings.predicted_n`. A stream chunk is parsed when
  it carries `timings` (the usage chunk, else the finish chunk); `timings` is relayed
  unchanged.
- **Streamed fields.** llama.cpp's deltas (`role`, `content`, `reasoning_content`,
  `tool_calls`) are already on the relay's list; the table in
  `docs/guide/requests.md` gains its column.
- **Load reports** (`capyctl-agent/src/load.rs`). Running is
  `llamacpp:requests_processing`, waiting `llamacpp:requests_deferred`; the KV usage is
  the tokens of processing slots over the sum of the slots' `n_ctx`, from one keyed,
  bounded `/slots` read per tick. A failed or malformed `/slots` read leaves the engine
  load out. No latency series is forwarded and the sample names no engine family.

Tests (T40, T42, CPU only): `capyctl-router` `engine_metrics` unit tests (answer,
stream with the usage chunk and with the finish chunk only, zero rate absent, the
`predicted_n` fallback), `crates/capyctl-agent/tests/load.rs` (the v0.6.0 `/metrics`
body, the `/slots` ratio, a refused and a malformed `/slots`). Fake-engine tests are not
qualification.

Live checks still open: a captured v0.6.0 answer, stream and `/metrics` body; the
`/slots` body size with long prompts (it carries prompt and generated text, against the
4 MiB scrape bound) and its read time under load.

Release note: none.
