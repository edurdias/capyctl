# Status: Engine logs in memory; vLLM request logging off; data retention docs — 2026-10-20 (branch `feat/zero-retention`)

Owner decisions of 2026-10-19 and 2026-10-20, under SPEC §13.3 (engine logs are
private, redacted and served only as a bounded tail):

- **`engine_logs: files | memory`.** A new role-local host setting (standalone:
  `host.engine_logs`; also `--set` and `CAPYCTL_SET__…`), `files` by default,
  which is the existing behaviour. With `memory`, the redacting log writer keeps
  the last 4 MiB of each launch's redacted output in its own memory and writes
  no file. It answers reads on a Linux abstract Unix socket named from a digest
  of the launch's log path, and both ends refuse a peer that is not the same
  user. It keeps answering for 60 seconds after the engine exits, then leaves
  with the buffer. Every reader of a launch's output goes through
  `capyctl_adapters::engine_log::tail_bytes`, which falls back to the buffer
  when no file exists. That covers the management engine-log read (the host's
  `EngineLogTail` and the standalone tail), the launch-failure reason quoted
  by vLLM, llama.cpp, TensorFold and group workers, and SGLang's own tail,
  which used to read the file directly and now uses the shared reader. A role
  started with `--debug-engine-logs` and `engine_logs: memory` refuses to
  start with a named reason; the flag is otherwise unchanged.
- **vLLM request logging off.** Every launch renders `--no-enable-log-requests
  --no-enable-log-outputs`. vLLM 0.29.0, 0.30.0 and 0.31.0 define both as
  `BooleanOptionalAction` switches, checked against the source of each tag, so
  no version gate or entry pin is needed. `--enable-log-outputs` is now
  reserved beside `--enable-log-requests` and `--disable-log-requests`, in
  `VLLM_RESERVED_FLAGS` and in the protected entry's `RESERVED`.
- **Data retention.** A new page, `docs/operations/data-retention.md`, linked
  from the guide and docs indexes and the site, lists what CapyCTL keeps per
  request and for how long, and how to turn each part off where that is
  possible. The settings reference documents `engine_logs`.

Tests (T20, T21), each failing before this change and passing after:
`memory_mode_writes_no_file_and_serves_the_tail` and
`a_startup_failure_is_classified_from_memory` (capyctl-launchers, a real child
through the exec launcher with the test binary as the writer),
`the_memory_buffer_is_bounded_to_whole_lines` and
`a_log_with_neither_file_nor_buffer_is_missing` (capyctl-adapters),
`engine_logs_default_to_files_and_stay_role_local` and
`standalone_honours_engine_logs` (capyctl-config),
`debug_engine_logs_are_refused_with_memory_engine_logs` (the `capyctl` binary),
`vllm_0_29_renders_request_and_output_logging_off`, `vllm_0_30_…`, `vllm_0_31_…`,
`request_and_output_logging_cannot_be_enabled`, `request_logging_off_on_every_rank`
and the Python `test_request_and_output_logging_are_off_on_every_supported_version`.

CPU and Fake-engine tests are not qualification. The live check still needed:
on a lab host with `engine_logs: memory`, launch vLLM 0.30 or 0.31 and SGLang,
confirm no file under the host's log directory, read the management engine-log
tail, force a launch failure (an unknown engine argument) and confirm its reason
in status, restart the host role and read the tail again; and confirm on vLLM
0.29, 0.30 and 0.31 that the rendered off flags are accepted and no request
line is logged.

# Release note: Operations

- **Engine logs can stay in memory.** Set `engine_logs: memory` on a host (or
  `host.engine_logs: memory` in a standalone document) and CapyCTL writes no
  engine log file. It keeps the last 4 MiB of each launch's redacted output in
  memory for the management engine-log read and for the reason a failed launch
  gives, until a minute after the engine exits. `--debug-engine-logs` is refused
  with it. vLLM now starts with request and output logging explicitly off on
  every supported version, and neither can be turned back on. A new
  [data retention](../operations/data-retention.md) page lists what CapyCTL
  keeps about requests, for how long, and how to turn each part off.
