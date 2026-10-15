# Status: vLLM 0.31.0 verified — 2026-10-14 (branch `feat/verify-vllm-0.31.0`)

Owner decision 2026-10-14: vLLM 0.31.0 joins the verified set (ADR 0018 §1).
AGENTS.md already lists vLLM 0.31.0 on host A.

What changed: `(Engine::Vllm, "0.31.0")` is in `registration::VERIFIED`, so a
0.31.0 installation no longer lists as `CUSTOM yes`. `docs/guide/engines.md`
(custom builds), `docs/guide/install-engines.md` (versions this release knows)
and ADR 0018 §1 (dated note) name it.

Tests: `capyctl-config` `registration.rs` `the_verified_set_marks_custom_builds`
(T22; 0.31.0 verified, 0.31.1 not).

Live evidence:

- Laptop (RTX 4090, 2026-10-10 and 2026-10-14): see
  `docs/changes/vllm-0310-support.md` (launch, chat, `cached_tokens`, deep park,
  stop), and today's wake-canary check (two matching wakes, and a tampered
  reference failing `wake_mismatch` with the engine stopped and memory
  released) and load read (fresh engine samples, `load_source: engine`).
- Host A (GB10, `engine-quick-check.sh --engine vllm --new 0.31.0 --old 0.30.0
  --model qwen3-4b`, origin/main a230cf9a): venv created with the guide's steps;
  engine add; cold start 142.4 s (0.30.0: 99.2 s), warm 71–87 s; streamed
  reasoning request passed (first token 0.186 s), stop and 409 after it;
  aggregate tok/s C1 22.0/22.0, C4 106.2/106.6, C8 207.0/207.3 (0.31.0/0.30.0),
  TTFT p50 within noise; reply lengths differ at C4/C8 as batch-order effects
  allow (no token hash on vLLM). Two harness failures were not CapyCTL's: the
  probe read `/metrics` without the launch key, which CapyCTL's own guard
  refuses on 0.30.0 too (fixed in the quick-check on branch
  `fix/vllm031-metrics-auth`), and the context sweep's calibration could not
  read a 1-token reply from a reasoning model (fixed in capyctl-recipes main).
- Rerun with both harness fixes (host A, 2026-10-14, origin/main `ae6852bf`, warm
  starts): every lifecycle step passed, including the new `engine load via CapyCTL`
  step (12 of 12 load reads during a request carried a fresh engine sample).
  Aggregate tok/s C1 21.9 / 21.9, C4 106.5 / 106.4, C8 207.4 / 207.0 (0.31.0 /
  0.30.0); decode at 2k and 8k within 0.2%, prefill within 2%. The 32k context
  point was refused by both versions (`engine_rejected`, maximum context 29104):
  that is the context CapyCTL fitted for the preset's Qwen3-4B deployment, not a
  difference between versions.

# Release note: Engines

- vLLM 0.31.0 verified.
