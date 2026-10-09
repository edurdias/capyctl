# Status: Tables an engine keeps on disk count against the models disk — 2026-10-09 (branch `feat/disk-backed-weights`)

Owner decision 2026-10-09 ([ADR 0014 amendment A20](../design/adr/0014-deployment-engine-configuration.md)).
Every safetensors byte counted as memory weights, so Qwen3.8-Flash-Next (126 GiB, 47.7 GiB of
it one n-gram table) never fit a 121.7 GiB GB10. The host now reads the n-gram table shards
(`ple_embedding.ngram_embedding.shard_<n>.{weight,scales,biases}`) from the safetensors headers
in the layout pass and reports them with the digest. When the engine arguments turn on SGLang
0.5.21 `--ple-offload-backend file` or TensorFold 0.6.5 `--ple-on-ssd` (vLLM 0.30 has none),
the memory weights are the rest of the checkpoint (a group member: its share of it) plus the
engine's cache of each table (SGLang 8 GiB, its default budget; TensorFold 1 GiB). The record
is `engine_config.memory.disk_tables`; launch plans carry `checkpoint_tables` (capability
`checkpoint_tables`), which the host checks against its own headers. Without the option or the
tables nothing changes: fingerprints pinned from main. Setting
`SGLANG_QWEN4_PLE_FILE_RSS_BUDGET_GB` beside the file backend is refused.

Tests, failing before (rule disabled: 5 of 7 in `crates/capyctl-config/tests/disk_tables.rs`
fail) and passing after: `flash_next_fits_one_gb10_with_sglang_keeping_the_table_on_disk`,
`flash_next_fits_one_gb10_with_tensorfold_reading_the_table_from_ssd`,
`flash_next_does_not_fit_one_gb10_without_the_option`,
`a_group_member_keeps_its_share_of_the_table_on_disk`,
`without_the_option_the_tables_change_nothing`, `a_changed_sglang_table_budget_is_refused`,
`the_engine_options_are_recognized`; plus `tables_are_found_beside_the_layout`,
`measured_tables_size_an_engine_that_keeps_them_on_disk` (store),
`the_checkpoint_tables_ride_beside_the_weights` (protocol) and the agent's plan check. CPU
tests only; not qualification.

Live check outstanding: model 8 (Qwen3.8-Flash-Next) on one GB10 with SGLang
(`--ple-offload-backend file`) and with TensorFold (`--ple-on-ssd`). The derived Ready phase is
about 108.5 GiB, above the automatic managed limit (97.4 GiB), so the host needs a managed
limit of about 110 GiB and a declared startup peak, or a first start that measures it. SGLang's
file backend also writes its own sparse copy of the table under its cache directory, outside
the model store.

# Release note: Memory

- **A model whose engine keeps a large table on disk is sized without it.** With SGLang's
  `--ple-offload-backend file` or TensorFold's `--ple-on-ssd` in a deployment's engine
  arguments, Qwen3.8-Flash-Next's 47.7 GiB n-gram table counts against the models disk instead
  of memory, plus the engine's in-memory cache of it (8 GiB for SGLang, 1 GiB for TensorFold).
  Without those options nothing changes.
