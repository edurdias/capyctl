# Status: Engine logs kept, redacted as written, with a management tail — 2026-10-08 (branch `engine-log-redaction`)

Owner decision 2026-10-08, SPEC §13.3 amended. Every engine's stdout and stderr now pass through a redacting writer process (the `capyctl` binary re-executed as `/proc/self/exe __engine-log-relay`, own process group, outlives an agent restart, ends at EOF) on the way to the launch's owner-only log (`crates/capyctl-launchers/src/engine_log_relay.rs`, used by `DurableSpawn` and `ExecLauncher`). It replaces the launch's credentials by value (protected-descriptor keys and observation credential, secret-named environment values, handed over on descriptor 3) and by rule URL userinfo, URL query strings and fragments (`capyctl_domain::redact::redact_urls`, shared with the model-source work), secret-named assignments, `hf_` tokens, bearer values and ≥48-character credential runs (`capyctl_adapters::engine_log::LogRedactor`). The log rotates at 16 MiB keeping `.1` and `.2`. SGLang now runs at `log_level`/`log_level_http` `info` (was `error`) and its output is no longer sent to `/dev/null` (`contain_startup_output` removed; the Python scrubber now runs in both modes); `log_requests` stays off, crash dumps stay off, and SGLang's refusal reason is logged in both modes. `--debug-engine-logs` keeps its meaning (SGLang debug level, output written without the writer) and leaves a `<log>.raw` marker. New route `GET /management/v1/deployments/{id}/engine-log?instance=<n>&kib=<1..256>` (admin token; server through the new `ExecuteMember.engine_log_tail` action, capability `engine_log_tail`, host→server message limit 320 KiB; standalone in-process) returns a bounded, re-redacted tail; a raw log is refused `403 forbidden`.

Prompt evidence at default level (source read, not live): SGLang 0.5.21 logs request text only under `log_requests` (`srt/utils/request_logger.py:88-90, 159-163`; OpenAI path `serving_base.py:87-89`), and logs `server_args=` with both keys at info (`srt/entrypoints/engine.py:1123`), which the writer redacts by value; vLLM 0.30.0 logs prompts only with `--enable-log-requests` (default off, `engine/arg_utils.py:3000`; prompts at DEBUG, `serve/utils/request_logger.py:44-61`), reserved by CapyCTL; TensorFold (0.6.3 read; 0.6.5 not available locally) prints request lines and token counts only and has no request-logging switch.

Tests (T21, T34): `crates/capyctl-launchers/tests/engine_log_relay.rs` (`a_launch_log_holds_no_owned_secret`, `a_worker_log_holds_no_observation_credential`, `the_exec_launcher_log_is_redacted_too`: seen failing with redaction disabled), `engine_log_relay::tests::*` (rotation, split reads, overlong lines), `capyctl_adapters::engine_log::tests::*`, `crates/capyctl-cli/tests/engine_log_relay.rs` (the built binary in writer mode), `runtime/tests` (`test_info_is_the_default_level_and_request_logging_stays_off`, `test_default_guard_keeps_output_and_still_rejects_plugins`, `test_output_is_scrubbed_of_credentials`: seen failing on the previous runtime), `vllm_args::request_logging_is_never_rendered_and_cannot_be_enabled` (locks existing behaviour), `crates/capyctl-management/tests/engine_log.rs` (bounds, redaction, raw refusal, auth), protocol `engine_log_tail` tests, `version_skew::an_engine_log_tail_needs_the_capability_and_is_otherwise_answered`, agent `engine_log_tail_serves_a_claimed_launch_bounded_and_redacted`. CPU tests only; not qualification. Live check needed: SGLang 0.5.21 and vLLM 0.30.0 on a GPU host, grep the engine log for the launch's keys and for a driven prompt's text, confirm SGLang's info-level warnings now appear, and read the tail route on server and standalone.

# Release note: Engine logs

- **Every engine keeps a redacted log.** SGLang's output was discarded unless
  CapyCTL ran with `--debug-engine-logs`; vLLM and TensorFold kept theirs
  unredacted. Now every engine's output is kept at its default level (info
  for SGLang), and a writer between the engine and the file replaces the
  launch's keys and other credentials, URL credentials, presigned URL query
  strings, bearer values and credential-shaped text with `<redacted>` before
  anything is written. Request logging stays off, so no prompt or completion
  is logged. The log stays owner-only and rotates at 16 MiB, keeping two
  older files. See [engine logs](../operations/install.md#engine-logs-and-troubleshooting).
- **Read the end of an engine log through the management API.**
  `GET /management/v1/deployments/<id>/engine-log?instance=<n>&kib=<N>`
  returns up to 256 KiB (64 by default) of an instance's log, whole lines,
  redacted, with the admin token. A log written under `--debug-engine-logs`
  is never served.
- **`--debug-engine-logs`** still raises SGLang to debug level; every engine's
  output is then written without the writer, as before (SGLang's Python-level
  output is still scrubbed of credential shapes).
