# Status: vLLM's safetensors loader stays `eager` by default while parking — 2026-10-15 (branch `fix/vllm-deep-wake`)

ADR 0014 amendment A21 and its note of 2026-10-15. On 2026-10-09 amendment A21 made a
parking vLLM deployment's undeclared loader follow the host's memory shape (`lazy` on unified
memory, `eager` on a discrete GPU), from a catalog comparison on host A (Qwen3.6-35B NVFP4,
vLLM 0.30, deep park, `eager` vs `lazy`: loading peak 54.2 vs 34.5 GiB, parked charge after
three cycles 28.3 vs 8.3 GB, wake to first token about 51 s vs 81 s). The same release
reverted it on live evidence from host A (GB10, vLLM 0.30.0, Qwen3.8-27B NVFP4, deep park,
run `target/live/0.1.3-backlog/2026-10-14-2311-release-checks/`):

- No declared loader (`lazy` default): cold start 601.6 s; the first wake's in-place weight
  reload took 123 s, beyond the 120 s `reload_weights` bound, so the wake ended uncertain and
  the instance stayed uncertain for 21 minutes until the deployment was deleted.
- Declared `eager`: six wakes of 23-28 s, each with a correct answer, parked charge flat at
  4.8 GiB.
- The `MRotaryEmbedding: Failed to load weights` warning appeared in both runs and is not the
  cause.
- The same branch (change file `vllm-deep-wake.md`) bounds a wake's weight reload by the
  wake window (180 s here) instead of a fixed 120 s, so that 123 s reload would no longer be
  cut; a `lazy` wake of this model has not been rechecked live.

What ships: absent a declared `engine_config.vllm.safetensors_load_strategy`, a parking
deployment resolves `eager` on unified memory and on a discrete GPU alike, as 0.1.2 did, shown
in the effective configuration with `provenance["vllm.safetensors_load_strategy"]` naming it
`capyctl default`. A declared `lazy` or `eager` wins and has no provenance entry. A
non-parking, undeclared deployment still resolves and renders no loader. The startup
placeholder uses `VLLM_EAGER_STARTUP_WEIGHTS_FACTOR` (3.6) exactly where
`--safetensors-load-strategy eager` renders (`VllmLaunchSettings::renders_eager_loader`), which
now includes the defaulted `eager` on unified memory; a declared `lazy` keeps
`STARTUP_WEIGHTS_FACTOR` (2.25).

Tests, failing before and passing after:

- `the_default_load_strategy_is_eager_on_every_memory_shape` (capyctl-adapters, renamed from
  `the_default_load_strategy_follows_the_host_memory_shape`): unified and discrete fixtures
  both resolve `eager` with a `capyctl default` provenance entry and render it; a declared
  `lazy` wins on either shape with no provenance entry.
- `sleep_mode_follows_the_host_deep_park_switch` and
  `the_load_strategy_follows_the_deployment_setting` (capyctl-adapters): the unified fixture's
  undeclared loader renders `eager`.
- `an_undeclared_strategy_stays_unset_without_parking` (capyctl-adapters): unchanged.
- `an_omitted_load_strategy_keeps_existing_identities` (capyctl-config): the unified fixture's
  omitted loader resolves `eager` with `capyctl default`; a declared `lazy` resolves `lazy`
  with no provenance entry; the command fingerprint stays pinned.
- `the_startup_placeholder_follows_the_effective_loader` (capyctl-config): a defaulted parking
  deployment on unified memory and a declared `eager` both use the eager factor; a declared
  `lazy` uses `STARTUP_WEIGHTS_FACTOR`.
- Recipe fingerprints re-pinned where the parking fixture's resolved loader changed
  (`an_absent_drafter_keeps_every_encoding_and_fingerprint`, `declared_resources_are_unchanged`,
  `a_single_host_deployment_is_unchanged`, `effective-vllm-golden.json`).

CPU and Fake-engine tests only; they are not qualification. Live check still needed: on host
A, model 1 on vLLM 0.30, deep park, no declared loader: the argv carries
`--safetensors-load-strategy eager`, the effective configuration shows provenance
`capyctl default`, and three wakes answer correctly.

# Release note: Memory

- **vLLM's weight loader while parking stays `eager` by default.** 0.1.3 does not change the
  default: a deep-parking vLLM deployment that does not set
  `engine_config.vllm.safetensors_load_strategy` loads with `eager`, as in 0.1.2, on every
  machine. You can set `lazy` instead; on unified memory it holds far less memory while
  loading, running and parked (Qwen3.6-35B NVFP4: 34.5 against 54.2 GiB while loading, 8.3
  against 28.3 GB parked), at the cost of a slower start and wake. On vLLM 0.30 with
  Qwen3.8-27B NVFP4, a `lazy` wake took over two minutes to reload the weights and did not
  complete. See [vLLM weight loading while parking](../guide/engines.md#vllm-weight-loading-while-parking).
