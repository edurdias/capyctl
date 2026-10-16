# Status: Status shows the whole-host startup reservation of an unmeasured model — 2026-10-16 (branch `fix/startup-placeholder-deep`)

Found live 2026-10-16 (Nemotron 3.5 Super VL 120B-A12B NVFP4 on vLLM 0.31.0, one GB10 of
121 GiB, standalone managed limit raised to 108 GiB): every new parking deployment
(`residency: deep`, the default) of the model, `engine_config.memory.request: 96GiB`, showed
`startup` 284713418881 bytes (265.2 GiB, provenance `default`) in `status deployment`, more
than the machine has. The checkpoint weighs 76328140836 bytes with the LiLiCorr drafter's
files (75010188240 without, which showed 260.7 GiB for the MTP deployments).

The figure is the placeholder exactly as specified, not an arithmetic fault. A parking vLLM
deployment gets `--safetensors-load-strategy eager`, so its placeholder uses
`VLLM_EAGER_STARTUP_WEIGHTS_FACTOR` (3.6, ADR 0014 amendment A21):
`max(request + graphs, weights × 3.6 + 8 GiB)` = 76328140836 × 18 / 5 + 8 GiB =
282.4e9 bytes, plus the 1.25 GiB CUDA context placeholder every derived cold phase carries:
284713418881. With `residency: restart_only` the loader stays vLLM's default and the factor
is 2.25 (about 169 GiB); those deployments showed 97.2 GiB only because their first start
had already been measured.

What was wrong is the status figure, not admission. Owner decision 2026-09-23 (solo first
start, ADR 0015 amendment of 2026-09-23): an unmeasured placeholder above the managed limit,
with a steady request that fits, makes the start reserve the whole managed limit, alone on
its host, provenance `whole_host`. `frozen_plan_startup` did that for every start here (the
265 GiB cold phase could not have passed the arm against a 108 GiB limit; the session always
started on an emptied host). But the deployment's `startup` in status
(`ordinary_lifecycle/startup.rs`, `status`) re-implemented only the measured and
weighed-placeholder branches of `frozen_plan_startup` and never its whole-host branch, so it
showed the placeholder as `default`, contrary to the owner decision of 2026-10-07 that status
shows the figure admission reserves. Only an instance still starting showed `whole_host`.
`status` now calls `frozen_plan_startup` itself, so the two cannot drift again: such a
deployment shows `bytes` = the managed limit, provenance `whole_host`, until a first start is
measured.

Measurement of deep first starts: they are recorded. Decoding the commands' request
identities against `measured_at_ms` shows the peaks of `supervl-vllm`, `-nosr`, `-nospec` and
`-nopc` (deep) and of `-ro7` and `nemotron-super-vl` (restart only) recorded within a second or
three of Ready (the later stop requests of the session followed Ready immediately, which made
some look recorded at stop). The five deployments with no row (`-lc5`, `-lc5-4seq`,
`-lc7-4seq`, `-mtp3`, `-mtp5`) are the configurations started for the first time with a new
speculative or concurrency shape, whose starts took 308 to 892 s against 166 to 188 s for the
same shapes once warm. [INFERENCE] Their samples fell inside reported kernel builds, which
ADR 0014 amendment A12 leaves out, or the start was contended; the engine logs that would
show which were not kept. Status lists every recorded row, so it is not a display issue, and
each such start keeps reserving the whole managed limit until one is measured, as specified.

Open for the owner, not changed here: the measured eager-loader peak of this model on deep
park was 101695578112 bytes (94.7 GiB, 1.33 × the weights), against the 3.6 × the weights
amendment A21 derived from one Qwen3.6-35B-A3B NVFP4 run.

Tests: `status_shows_the_whole_host_a_start_above_the_managed_limit_reserves`
(`capyctl-controller`, `coordinator/tests_startup.rs`) resolves the live shape (parking
vLLM, request 96 GiB, 76328140836 bytes of weights, managed limit 108 GiB), checks the
placeholder is 284713418881 and that status shows `whole_host` 108 GiB, the same figure the
start then holds; it failed before the fix (status `default`, 284713418881).
`an_unmeasured_model_above_the_managed_limit_starts_alone_and_is_measured` and
`weights_sized_while_the_digest_is_pending_trigger_the_solo_first_start` asserted the old
status figure and now assert `whole_host` at the managed limit. CPU and Fake-engine tests
only; not qualification. Live check still needed: a new parking deployment of a model whose
placeholder is above the managed limit shows `whole_host` at the limit before and during its
first start, and `measured` after it.

# Release note: Fixes

- **`status deployment` shows the startup memory a first start really sets aside.** A model
  whose estimated startup peak is above the machine's managed limit starts alone and sets
  aside the whole limit; status now shows that limit (provenance `whole_host`) instead of the
  estimate, which could exceed the machine's memory (265.2 GiB for a 71 GiB checkpoint on a
  121 GiB machine).
