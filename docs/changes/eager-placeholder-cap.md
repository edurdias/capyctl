# Status: The eager loader's startup placeholder is capped at 32 GiB over the weights — 2026-10-18 (branch `fix/eager-placeholder-cap`)

Owner decision 2026-10-18 (ADR 0014 amendment A22, option b of the question left open on
2026-10-16 about the 3.6 × weights placeholder). An unmeasured vLLM deployment that renders
`--safetensors-load-strategy eager` (every parking deployment by default) had the startup
placeholder `max(request + graphs, weights × 3.6 + margin)` (amendment A21). It is now
`max(request + graphs, weights + min(weights × 13/5, 32 GiB) + margin)`
(`VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES`), which is `min(weights × 3.6, weights + 32 GiB)` to
the byte: a checkpoint up to 13215283988 bytes (about 12.3 GiB) keeps exactly the old figure,
a larger one is capped. The non-eager factor (2.25) is unchanged; a measured peak still
replaces the placeholder.

Evidence (CapyCTL's `startup.measured`, vLLM 0.30.0, `eager`, GB10 host A): the peak over the
weights was 12.4 to 30.2 GiB, KV cache included, from 22 to 71 GiB of weights. Placeholder
before and after, against the measured peak:

| Checkpoint | Before (A21) | After (A22) | Measured peak |
|---|---|---|---|
| Qwen3.8-27B NVFP4, KV 4 GiB | 94087532268 (87.6 GiB) | 66699005648 (62.1 GiB) | 56220348416 (52.4 GiB) |
| Qwen3-14B BF16, KV 4 GiB | 114921930896 (107.0 GiB) | 72486338600 (67.5 GiB) | 44531412992 (41.5 GiB) |
| Qwen3-30B-A3B BF16, KV 4 GiB | 228429606924 (212.7 GiB) | 104016248608 (96.9 GiB) | 76076666880 (70.9 GiB) |
| Qwen3.8-27B BF16, KV 4 GiB | 208616758985 (194.3 GiB) | 98512679736 (91.7 GiB) | 68840255488 (64.1 GiB) |
| Nemotron 3.5 Super VL 120B-A12B NVFP4 | 283371241601 (263.9 GiB) | 119277813796 (111.1 GiB) | 101695578112 (94.7 GiB) |
| Qwen3.6-35B-A3B NVFP4 (A21's run, 23.5 GB) | about 86.8 GiB | about 61.9 GiB | 54.2 GiB |

Every row stays covered. The derived cold phase adds the 1.25 GiB CUDA context: Super VL's
is 120619991076 bytes (112.3 GiB, from 284713418881), still above a 108 GiB managed limit, so
its unmeasured first start still runs alone on the machine and reserves the whole managed
limit.

The cap is recorded in the revision as `startup_eager_cap_bytes`, beside
`startup_graphs_bytes`, and re-derived when a snapshot decodes; the store's solo-first-start
recomputation uses it. A revision frozen under amendment A21 records none and keeps
`weights × 3.6` exactly as it was frozen, as amendment A8 let earlier revisions keep 1.6: a
frozen revision must re-resolve to its own bytes. It holds the larger figure only until its
first start is measured; a new revision takes the cap.

Tests (`capyctl-config` `startup_budget`):
`the_eager_placeholder_keeps_3_6_below_the_cap_and_caps_above_it` (the last byte below the
cap and the first above, a 10 GiB checkpoint unchanged),
`the_capped_eager_placeholder_covers_every_measured_peak` (each row above),
`a_revision_frozen_before_the_eager_cap_keeps_its_placeholder` (a frozen A21 revision decodes
with 116 GiB, a new one gets 70 GiB, a snapshot that drops its cap is refused), and
`the_startup_placeholder_follows_the_effective_loader` (the recorded cap, a forged one refused)
and `the_placeholder_startup_covers_the_first_starts_graphs` updated from 116 to 70 GiB for
30 GiB of weights. `capyctl-controller`
`status_shows_the_whole_host_a_start_above_the_managed_limit_reserves` now pins Super VL's
120619991076 and still expects `whole_host` at 108 GiB. CPU and Fake-engine tests are not
qualification. Live check still needed: a first start of an unmeasured large eager checkpoint
whose capped placeholder fits beside other work, to confirm its peak stays under it; the
measurements cover vLLM 0.30.0 on GB10 only.

# Release note: Fixes

- **A large vLLM model that parks no longer estimates an impossible startup peak.** Before a
  first start is measured, CapyCTL estimated a parking vLLM model's startup peak at 3.6 times
  its weights; Nemotron 3.5 Super VL showed 265 GiB. The estimate now adds at most 32 GiB to
  the weights (112 GiB for Super VL), which still covers every peak measured on a GB10. Models
  under about 12 GiB keep the same estimate. A deployment revision accepted before this
  release keeps its old estimate until its first start is measured or a new revision replaces it.
