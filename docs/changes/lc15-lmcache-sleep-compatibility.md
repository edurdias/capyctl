# Status: LMCache sleep compatibility and held-KV accounting — 2026-11-10 (branch `fix/lc15-lmcache-hits`)

ADR 0034 LC15 exposed two separate failures on an enrolled discrete-GPU host.
The engine and LMCache server both rendered engine-driven transfer correctly, but
vLLM sleep cleared the external connector's cache. The downloaded vLLM 0.32.0
wheel has the same reset chain as the installed 0.33.0; LMCache 0.5.6 and 0.5.7
provide no built-in server option to ignore that CLEAR. The October successful
checks used `restart_only`, which restarts the engine instead of calling sleep.
The ADR amendment records the source chain, live evidence and qualification limits.

The launch-time source-shape check now rejects the known destructive sleep path
for `deep` and `host_backed`: `required` refuses
`lmcache_unavailable:sleep_clears_cache`; `auto` keeps parking with LMCache off
and reports `sleep_clears_cache` with its explanation. Embedded and enrolled
launches pass the residency into the same check. Restart-only parking is unchanged.
Engine-driven grants now reserve zero held engine KV; LMCache-driven grants keep
A13's reserve and A15's release guards. LC15's engine actually discarded its 5 GiB
KV cache, while the store unconditionally recorded that amount as held. The
separate parked host-memory overshoot remains failed evidence, not a demonstrated
GPU-KV leak or a passing qualification.

Tests (T45; failing before, passing after):

- `sleep_that_clears_external_cache_refuses_lmcache_parking_only` originally
  accepted the destructive installation. It now refuses both sleep residencies,
  preserves restart-only behavior and accepts an explicit connector-preserving fix.
- `engine_driven_lmcache_never_reserves_held_engine_kv` originally reserved 4 GiB
  instead of zero; it now checks embedded and enrolled plans, both sleep
  residencies, and the unchanged restart-only reserve.
- The embedded installation gate tests both `auto` and `required`, and the remote
  host's closed refusal mapping includes the new reason.

Targeted checks passed: agent LMCache installation tests (22), store LMCache tests
(37), controller installation-gate tests (13), configuration LMCache tests (22),
protocol LMCache tests (5), and the agent's static refusal mapping test (1).
`cargo fmt --all -- --check` passed. All Cargo commands used the shared engine
lock with `CARGO_TARGET_DIR` unset and this worktree's own target directory.
CPU and Fake-engine tests are not qualification. No live rerun was performed;
LC13–LC16 require a compatible installation, real post-sleep hits and their
original memory/cost bounds, and LC15 also requires its enrolled-host wake canary.

# Release note: Fixes

- **Do not silently discard LMCache on sleep-based parking.** Known vLLM sleep
  implementations that clear the external cache now report `sleep_clears_cache`:
  required LMCache refuses the launch, while automatic LMCache keeps parking with
  caching off. Restart-only caching is unchanged. Engine-driven launches no longer
  reserve engine KV that the server never mapped.
