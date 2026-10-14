# Status: TensorFold 0.6.6 verified — 2026-10-14 (branch `feat/verify-tensorfold-0.6.6`)

Owner decision 2026-10-14: TensorFold 0.6.6 joins the verified set (ADR 0023
§1), installed on both lab hosts (host B's venv a byte-identical copy of host
A's); AGENTS.md's engine list also records vLLM 0.31.0 and a llama.cpp build in
the home directory on host A, approved the same day.

Source audit of v0.6.6 (`cb2ebf05`) against v0.6.5: the only upstream change
CapyCTL sees is the new `tensorfold serve --name-priority ID=background`
option (CUDA only), an ordinary extra argument; no environment, HTTP route,
metrics or build change. `--name-priority` names a served name or alias, and
CapyCTL passes no `--name` to a TP2 follower, so it may fail there
(documented in the engine options table). The audit found two gaps that
predate 0.6.6, both fixed here:

- `TENSORFOLD_API_KEY` was not an owned name. TensorFold reads it
  (`server/authentication.py`) and then refuses the unkeyed health and
  metrics routes CapyCTL reads; a profile or deployment `engine_env` entry
  under an approval such as `TENSORFOLD_*` reached the engine through
  `engine_env.values()` → `tensorfold/frozen.rs` `build_env` → the launch
  environment. It is now in `engine_env::OWNED_NAMES`.
- The router's TensorFold mapping read only the MLX statistics
  (`prefill_seconds`, `time_to_first_token`, `tokens_per_second`). The CUDA
  Qwen engine (`families/qwen3_5/cuda/engine.py` `generate`, sent by
  `cuda/http.py`) reports `prefill_s`, `decode_s` and draft counters instead,
  so a CUDA answer had no engine figures beyond cached tokens. A finite
  non-negative `prefill_s` now maps to `prefill_ms` (source `engine`) when
  `prefill_seconds` is absent. `decode_s` is not turned into a rate: the
  first token comes out of prefill and a gated request's statistics cover its
  last run only, so the engine gives no rate CapyCTL can state; time to first
  token and decode rate stay the router's on CUDA. `docs/guide/requests.md`
  says so.

Tests: `capyctl-config` `tensorfold.rs` (0.6.6 verified, 0.6.4 and 0.6.7
not), `capyctl-cli` `engine_cli.rs`
`tensorfold_066_is_verified_and_064_and_067_are_custom` (T07),
`capyctl-config` `engine_env.rs` `owned_names_are_never_settable` (T21;
failed before the owned-name change, passes after),
`capyctl-router` unit `tensorfold_cuda_statistics_map_prefill_only` and
integration `tensorfold_cuda_reports_prefill_only`, streamed and not (T40
T41; both failed before the mapping change, pass after). The old unit fixture
labelled `"engine": "cuda"` carried the MLX shape and is now labelled `mlx`.
CPU and Fake-engine tests are not qualification.

Live (host A, quick-check): PENDING

# Release note: Engines

- **TensorFold 0.6.6 verified.** CapyCTL lists TensorFold 0.6.6 as verified
  beside 0.6.0 to 0.6.3 and 0.6.5. Its new `--name-priority ID=background`
  option is an ordinary extra argument; on the follower of a two-GPU group,
  which CapyCTL starts without `--name`, it may fail.
- **TensorFold's key variable is reserved.** `TENSORFOLD_API_KEY` can no
  longer be set in a profile's or deployment's `engine_env`, whatever the
  approvals: it turned on TensorFold's own authentication and locked CapyCTL
  out of the engine's health and metrics.
- **Prefill time on TensorFold's CUDA engine.** A TensorFold answer from a
  CUDA Qwen model now carries the engine's `prefill_ms`; its time to first
  token and decode rate are measured by the router.
