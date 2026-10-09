# Status: Per-request engine metrics on every chat answer — 2026-10-09 (branch `feat/per-request-engine-metrics`)

Owner decision 2026-10-09: every completed chat response carries normalized
per-request engine metrics, and SGLang's cache report is on by default
(SPEC §17). The router adds `capyctl.metrics` to a non-streaming body and one
SSE comment, `: x-capyctl-metrics {...}`, before `data: [DONE]` on a completed
stream (beside the opt-in `x-capyctl-timing` comment; engine chunks are
relayed byte for byte). Figures: `ttft_ms`, `queue_ms`, `prefill_ms`,
`decode_tokens_per_second`, `cached_tokens`, each `{"value", "source"}` with
source `engine` or `router`, and absent when nobody measured it. Mapping
(`crates/capyctl-router/src/engine_metrics.rs`): vLLM 0.30 `metrics`
(`queue_time_ms`; `time_to_first_token_ms`, which runs from scheduling, as
prefill; their sum as TTFT; 1000 / `mean_itl_ms` as decode rate); TensorFold
0.6.x `tensorfold` (`time_to_first_token`, `prefill_seconds`,
`tokens_per_second`, whose 0.0 placeholder counts as unknown); cached tokens
from `usage.prompt_tokens_details.cached_tokens` for every engine. The router
derives TTFT (forward start to the first chunk with generated text) and the
decode rate ((completion tokens − 1) over first text to last chunk) only where
the engine reported none, which is always the case for SGLang 0.5.21. A
collecting forwarder now lends each chunk to the router (`ChatSink::collected`)
so a non-streaming answer is timed the same way, and a collected vLLM answer
keeps the `metrics` object from its usage chunk.

Launch defaults: vLLM renders `--enable-per-request-metrics` in its reserved
block (single-rank and group head; a headless worker has no API server), and
the protected entry compares `enable_per_request_metrics` (present in 0.29.0
and 0.30.0 `FrontendArgs`; read from the published 0.30.0 sdist). SGLang's
`enable_cache_report` is a reserved constant `True` (`fields/serving.py` in
0.5.20 and 0.5.21; 0.5.21 read from the published wheel). Both are in the
deploy-time reserved lists, so extra arguments cannot set or reverse them;
`docs/guide/engine-flags.md` has rows for both and for vLLM's
`--enable-prompt-tokens-details` (extra). `docs/guide/requests.md` documents
the figures and the per-engine table.

Tests (T14, T40, T41): router unit tests per engine on real-shaped bodies
(`vllm_metrics_map_and_nulls_stay_absent`,
`tensorfold_statistics_map_and_placeholders_stay_absent`,
`sglang_reports_cached_tokens_only`, `only_chunks_that_may_report_are_read`,
`attach_leaves_the_engine_fields`); router integration tests
(`crates/capyctl-router/tests/engine_metrics.rs`) per engine, non-streaming
and streaming, with relayed chunks checked byte for byte and absent-not-zero
cases; `a_collected_response_keeps_the_vllm_metrics_object`;
`per_request_metrics_render_by_default_and_are_reserved`; reserved spellings
in `engine_config.rs`, `sglang_args.rs`, `test_vllm_entry.py` and
`test_sglang_server_args.py`. `stream_safety.rs` expectations now include the
metrics comment. CPU and Fake-engine tests are not qualification.

Live check still needed (no lab host was used): on each engine (vLLM 0.30.0,
SGLang 0.5.21, TensorFold 0.6.5), one non-streaming request and one stream
with `stream_options.include_usage`, checking that the vLLM launch is accepted
by the protected entry with the flag, that `capyctl.metrics` and the comment
carry engine figures, and that SGLang reports `cached_tokens` on a repeated
prompt.

# Release note: Requests

- **Per-request metrics.** Every chat answer now carries the engine's
  figures for that request: time to first token, queue time, prompt time,
  generation speed and cached prompt tokens, each marked as measured by the
  engine or by CapyCTL, and left out when unknown rather than shown as zero.
  A non-streaming answer has them under `capyctl.metrics`; a stream ends
  with a `: x-capyctl-metrics` comment line before `data: [DONE]`. vLLM now
  starts with `--enable-per-request-metrics` and SGLang with
  `--enable-cache-report`; neither can be turned off. See
  [Make a request](../guide/requests.md#per-request-metrics).
