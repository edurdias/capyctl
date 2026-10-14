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

Live on host A, 2026-10-14 (one GB10, `scripts/live/engine-quick-check.sh
--engine tensorfold --new 0.6.6 --old 0.6.5 --host a`, standalone from
origin/main `a230cf9a`; a new `~/tensorfold-0.6.6-venv` at `cb2ebf05` with the
same pins as 0.6.5: Python 3.12.14, torch 2.13.0+cu130, triton 3.7.1, CUDA
13.0). Qwen3.8-27B NVFP4 with the DFlash2 drafter, `--parallel 8`, greedy.
Every lifecycle step passed: `engine add`, cold start 101.5 s (0.6.5: 139.2 s),
warm 13.9 s, a reasoning stream (14 reasoning chunks, content, `stop`, usage,
first token 0.142 s), stop and a 409 after it, `--evict` and wake on request
(15.6 s), the variant without drafts, engine `/health` and `/metrics` (no key).
Benchmark against 0.6.5 (medians, two interleaved passes): aggregate tok/s C1
42.5 / 42.4, C4 122.1 / 120.3, C8 195.0 / 194.2; decode and prefill at 2k, 32k
and 128k within 1%; TTFT at 128k 87.7 / 87.1 s; peak memory equal. C4's TTFT
p50 read 0.269 / 0.187 s with p95 equal: both versions show the same discrete
tiers by queue position (about 0.10, 0.14, 0.21, 0.33 and 0.38 s), and 0.6.5's
own two passes differ the same way. `token_sha` matched on 30 of 30 keys,
between versions and within each. Host B's `~/tensorfold-0.6.6-venv` is a
byte-identical copy of host A's (file digest compared). That run used the
binary from before the CUDA `prefill_s` mapping; the mapping is checked on the
live release checks.

# Release note: Engines

- **TensorFold 0.6.6 verified.** CapyCTL lists TensorFold 0.6.6 as verified
  beside 0.6.0 to 0.6.3 and 0.6.5. Its new `--name-priority ID=background`
  option is an ordinary extra argument; on the second machine of a two-machine
  group, which CapyCTL starts without `--name`, it may fail.
- **TensorFold's key variable is reserved.** `TENSORFOLD_API_KEY` can no
  longer be set in a profile's or deployment's `engine_env`, whatever the
  approvals: it turned on TensorFold's own authentication and locked CapyCTL
  out of the engine's health and metrics.
- **Prefill time on TensorFold's CUDA engine.** A TensorFold answer from a
  CUDA Qwen model now carries the engine's `prefill_ms`; its time to first
  token and decode rate are measured by the router.
