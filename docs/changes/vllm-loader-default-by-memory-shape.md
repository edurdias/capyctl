# Status: vLLM's safetensors loader defaults by memory shape — 2026-10-09 (branch `feat/vllm-lazy-default-unified`)

Owner decision 2026-10-09 (ADR 0014 amendment A21). The note on §3 and §4 (2026-10-07,
PR #73) left vLLM's safetensors loader defaulting to `eager` under sleep mode on every
host, from a Qwen3-4B qualification on a discrete GPU (vLLM 0.29, `785b887`: wake 57 s to
7.5 s). A controlled catalog comparison now makes the default follow the host's own
unified/discrete memory shape instead: host A, Qwen3.6-35B NVFP4, vLLM 0.30, deep park,
`eager` vs `lazy` — start 98 s vs 180 s, loading peak 54.2 vs 34.5 GiB, ready footprint
43.95 vs 26.63 GiB, parked charge after three cycles 28.3 vs 8.3 GB, wake to first token
about 51 s vs 81 s. `eager` also drove amendment A19's parked-growth guard into stops.

Resolution now picks `lazy` on unified memory and keeps `eager` on a discrete GPU (ADR
0019) absent a declared `engine_config.vllm.safetensors_load_strategy`, reusing the same
device-domain detection the memory request and margin already derive on
(`EngineInputs::device`). The resolved value is always present in the effective
configuration (`VllmLaunchSettings::safetensors_load_strategy`), with
`provenance["vllm.safetensors_load_strategy"]` naming a defaulted value `capyctl default`;
a declared one still has no entry. Rendering is unchanged: under sleep mode the resolved
value always renders beside `--enable-sleep-mode`, and outside it only a declared value
renders, so an undeclared, non-parking launch keeps its command identity and fingerprint.

The startup placeholder (`STARTUP_WEIGHTS_FACTOR`, 2.25) was measured with no loader flag
rendered at all (vLLM's own default, memory-mapped); it stays the factor for every
non-eager launch. A new `VLLM_EAGER_STARTUP_WEIGHTS_FACTOR` (3.6, scaled from the evidence's
54.2 / 34.5 loading-peak ratio and rounded up for headroom) applies exactly when CapyCTL
will actually render `--safetensors-load-strategy eager`
(`VllmLaunchSettings::renders_eager_loader`), in both the deploy-time placeholder
(`resolve_startup`) and the store's solo-first-start recomputation
(`ordinary_lifecycle::startup::weighed_placeholder`).

Tests, failing before and passing after:

- `the_default_load_strategy_follows_the_host_memory_shape` (capyctl-adapters): a unified
  fixture resolves `lazy`, a discrete one resolves `eager`, a declared choice wins on either
  and carries no provenance entry, and the resolved value (declared or defaulted) renders in
  `sleep_flags` and the argv.
- `an_omitted_load_strategy_keeps_existing_identities` (capyctl-config): the unified
  fixture's omitted loader now resolves to `lazy` with a `capyctl default` provenance entry
  that round-trips through the effective snapshot; the command fingerprint stays pinned.
- `the_startup_placeholder_follows_the_effective_loader` (capyctl-config): a declared
  `eager` loader scales the startup placeholder by `VLLM_EAGER_STARTUP_WEIGHTS_FACTOR`; a
  declared `lazy` loader on the same host keeps `STARTUP_WEIGHTS_FACTOR`.
- `sleep_mode_follows_the_host_deep_park_switch` and `the_load_strategy_follows_the_deployment_setting`
  (capyctl-adapters) updated for the unified fixture's new default.

CPU and Fake-engine tests only; they are not qualification. Live check still needed: on
host A or B, a unified-memory deep-park deployment with no declared loader wakes with
`lazy` and holds the lower footprint and parked charge the catalog measured; a discrete-GPU
deployment still gets `eager`.

# Release note: Memory

- **vLLM's weight loader now defaults by the machine's memory.** A deep-parking vLLM
  deployment that does not set `engine_config.vllm.safetensors_load_strategy` gets `lazy`
  on a machine with unified memory (GPU and system memory share one pool) and `eager` on a
  discrete GPU. On unified memory this trades a slower wake for far less memory held while
  loading, running and parked; set the field to `eager` by hand if the wake time matters
  more. See [vLLM weight loading while parking](../guide/engines.md#vllm-weight-loading-while-parking).
