# Status: SGLang dump options and DUMPER_ reserved; engine flags table — 2026-10-20 (branch `fix/sglang-flag-reservations`)

Owner decision 2026-10-20 (SPEC §8.2, T21; ADR 0014 §3, §8; ADR 0028 §2.1;
ADR 0021). SGLang options and environment variables that write requests,
tensors or debug data to disk are now reserved: refused in every spelling,
whatever the host approves, where before the path-shaped ones only needed a
named approval. The list was read from the installed SGLang 0.5.21 and 0.5.22
parsers (`ServerArgs.add_cli_args`) on host A and from the code that reads
each field or variable:

- options (`SGLANG_DUMP_RESERVED` in `engine_policy.rs`, `_RESERVED_DUMP` in
  `runtime/sglang_server_args.py`): `--watermark-config` (0.5.22; the
  watermark key file or JSON), `--debug-tensor-dump-output-folder`,
  `--debug-tensor-dump-layers`, `--debug-tensor-dump-input-file` and the whole
  `--debug-tensor-dump-*` family, `--msprobe-dump-config`,
  `--export-metrics-to-file` and `--export-metrics-to-file-dir` (a record of
  every request, hourly files), and `--spec-trace-dir`. `--log-requests`,
  `--log-requests-target` and `--crash-dump-folder` were already CapyCTL's.
- environment (`engine_env.rs`, refused as `engine_env_reserved:<name>`,
  compared upper-cased at both levels, and refused as an approval glob): the
  `DUMPER_`, `SGLANG_DUMP`, `SGLANG_KERNEL_API_` and `SGLANG_CUDA_COREDUMP`
  families, and `SGLANG_DSPARK_DEBUG_DUMP`, `SGLANG_DSPARK_ENABLE_SPS_RECORD`,
  `SGLANG_EXPERT_DISTRIBUTION_RECORDER_DIR` and `SGLANG_TORCH_PROFILER_DIR`.

New command `capyctl engine flags <family> [--version V]` prints every option
and environment name the checks know for a family with its class (owned,
reserved, sensitive with the approval kind, or ordinary) and the reason; text
by default, `--format json` on request (ADR 0021). It is built from the same
tables the checks read (`engine_policy::flag_table`, `engine_env::env_table`).
Documented in `docs/guide/engine-flags.md`.

Tests (T21): `sglang_dump_destinations_are_reserved_in_every_spelling`
(exact, abbreviated, `_` and `=value` spellings and the family, with every
option approved; failing before), `dump_environment_names_are_reserved`
(`DUMPER_*` and the SGLang names in any case, both levels, approval globs;
failing before), `the_environment_table_matches_the_check`,
`engine_flag_ownership.rs` (every table row run through
`validate_extra_args` agrees with its class; every reserved and typed name is
listed; the JSON form carries the classes), the runtime parity test and
`test_dump_destinations_are_refused_however_spelled` in
`runtime/tests/test_sglang_server_args.py`, and `engine_flags_parses`. CPU
and Fake-engine tests are not qualification. Live check still needed: on host
A, deploy an SGLang 0.5.22 model with `--watermark-config` and with
`--export-metrics-to-file` in `extra_args` and with `DUMPER_ENABLE` in
`engine_config.env`, and confirm each is refused before any launch, then that
an ordinary SGLang deployment still starts.

# Release note: Engines

- **SGLang options that dump to disk are refused.** `--watermark-config`,
  the `--debug-tensor-dump-*` options, `--msprobe-dump-config`,
  `--export-metrics-to-file`, `--export-metrics-to-file-dir` and
  `--spec-trace-dir` are reserved, whatever the host approves, and so are the
  `DUMPER_*`, `SGLANG_DUMP*`, `SGLANG_KERNEL_API_*` and
  `SGLANG_CUDA_COREDUMP*` variables and a few other SGLang dump variables.
  `capyctl engine flags <family>` prints which options and variables CapyCTL
  owns, reserves, gates behind an approval or passes unchanged, and why
  (`--format json` for scripts).
