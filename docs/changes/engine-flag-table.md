# Status: Engine options page and a memory-fraction mapping — 2026-10-08 (branch `docs/engine-flag-table`)

Owner decision 2026-10-08: no raw override of SGLang's `--mem-fraction-static`
(it stays reserved, ADR 0014 §3); a documented table and a mapping instead.
`docs/guide/engine-flags.md` has one table per engine (vLLM 0.30, SGLang 0.5.21,
TensorFold 0.6.5) classing common options as typed, extra, approval or reserved,
and a worked mapping from a target SGLang fraction to `memory.request` and
`memory.kv_cache` on `main` (8 GiB margin, 2 GiB static overhead; open PRs #74 and
#75 revisit both). `enable_linear_replayssm_spec` was found declared in the
published SGLang 0.5.21 wheel (`arg_groups/fields/exec_.py`); it is an ordinary
extra argument. `engine_policy::typed_options` is now public for the test.

Tests (T14): `the_engine_option_tables_match_the_policy` runs every tabled option
through `validate_extra_args` with no approvals and checks each row's class and
typed field, and that every policy typed option is tabled; `a_wrong_row_fails_the_check`
proves a wrong class, a wrong field and a missing typed row fail (a deliberately
wrong `--enable-mixed-chunk` row failed the first test before it was reverted).
CPU tests only; the engine option lists were read from the published vLLM 0.30.0
sdist, the SGLang 0.5.21 wheel and TensorFold's `v0.6.5` tag, not from the lab
hosts' environments, and no live run was made.

# Release note: Documentation

- **Engine options page.** A new [Engine options](../guide/engine-flags.md)
  page has one table per engine (vLLM 0.30, SGLang 0.5.21, TensorFold 0.6.5)
  for the options people ask about most: speculative decoding, parsers,
  context and concurrency, state and KV caches, memory, CUDA graphs,
  quantization, logging, KV offload and plugins. Each row says whether the
  option is a typed field, an extra argument, an extra argument that needs
  host approval, or reserved, and what to use instead. A test checks every
  row against CapyCTL's argument checks. The page also shows how to reach a
  given SGLang memory fraction with `memory.request` and `memory.kv_cache`;
  there is still no setting for the fraction itself.
