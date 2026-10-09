# Status: The compile-job count leaves out the model on unified memory — 2026-10-09 (branch `fix/max-jobs-unified-memory`)

Found live on a GB10 (catalog model 8, SGLang 0.5.21, `main` eb214aad): CapyCTL
set the engine's `MAX_JOBS` from `MemAvailable` at launch, 14 jobs there. SGLang
builds its FlashInfer JIT kernels after the model has loaded; on unified memory
the model had taken about 95 GiB of the same pool, about 20 GiB was left, the
parallel `nvcc` jobs took the machine to 116.4 GiB and the first start was
killed for memory twice (also with `--disable-flashinfer-autotune`). With
`MAX_JOBS=2` the first start built and became Ready in 1745 s, inside the
1800 s Initialize bound.

The rule (owner decision 2026-09-25, SPEC §13.3 amendment) now leaves out what
the engine itself holds when the builds run:
`MAX_JOBS = clamp(floor((MemAvailable at launch − unified Ready charge) / 8 GiB),
1, CPU count)`.

- The unified Ready charge is the sum of the deployment's Ready-phase
  allocations on `unified` domains (`engine_env::unified_ready_bytes`,
  `crates/capyctl-adapters/src/engine_env.rs`; SPEC §7.2, ADR 0007
  footprints). The Ready charge, not the startup charge, is used: the builds run
  once the model is resident, and a measured startup peak can itself be the
  build's (ADR 0014: a 111 GiB peak recorded during a 9-minute build). With the
  figures above, 117 GiB less 95 GiB gives 2 jobs.
- On a discrete GPU the model sits in the card's own memory: `device` and
  `distinct` domains add nothing, so a discrete host's count is unchanged
  (ADR 0019).
- The 8 GiB per job stays: each fused-MoE `nvcc`/`cicc` job held 7 to 9 GB on
  GB10 (2026-09-25), and the 2 jobs that the new rule gives here are the count
  that built.
- The SGLang launch (`NativeLaunch::unified_ready_bytes`) and the vLLM and
  TensorFold plans (`unified_ready_bytes`) carry the charge from the frozen
  effective deployment to Initialize. All three engines compile after the
  weights load (TensorFold builds extensions on its first request, ADR 0023
  §6).
- A `MAX_JOBS` in the resolved engine env (the profile's `env` or the
  deployment's `engine_config.env`, ADR 0028 §2.1) still wins; the host log
  now names it `engine env`, and a computed count names the Ready charge it
  left out.
- `docs/operations/install.md`, `docs/operations/configuration.md` and the
  host examples describe the rule and the override.

Tests: `engine_env` unit tests (a 95 GiB charge on 117 GiB gives 2 jobs, a
charge at or above what is available keeps the floor of 1, a set `MAX_JOBS`
wins whatever the charge, only `unified` domains count);
`vllm_frozen.rs` and `device_namespace.rs` check that the vLLM plan and the
SGLang launch carry the Ready charge on unified memory and none on a device
domain. The old rule gave 14 jobs for the 117 GiB case. CPU tests
only; they are not qualification. Live check still needed: a first start of
catalog model 8 on SGLang 0.5.21 on a GB10 with an empty JIT cache, with no
`MAX_JOBS` set, should log `MAX_JOBS=2` and become Ready without a memory kill.

# Release note: Fixes

- **First starts on a GB10 no longer run out of memory compiling kernels.**
  The engines compile some GPU kernels after the model has loaded. On a
  unified-memory machine CapyCTL now sizes the number of compile jobs from the
  memory left once the deployment's Ready memory is taken, not from the free
  memory before the model loads; a model using most of the machine compiles
  with 1 or 2 jobs. Discrete GPUs are unchanged. A `MAX_JOBS` set in the engine
  profile's `env` or the deployment's `engine_config.env` is still used as
  written ([registering engines](../operations/install.md#registering-engines)).
