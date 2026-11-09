# Status: Escaped stream keys retain their meaning — 2026-11-09 (branch `fix/escaped-role-chunk`)

SPEC §10 and §17: role-only stream openers with Unicode-escaped keys no longer count as first output or progress. An engine disconnect before any token therefore ends with the expected error event. Router metric prefiltering also admits escaped keys for JSON classification. Engine payload bytes remain unchanged. The router regression `an_escaped_role_only_chunk_then_disconnect_ends_with_an_error` and metric regression `escaped_metric_keys_are_not_filtered_out` failed before the fix and passed afterward. CPU and Fake-engine tests are not qualification; a live engine stream with an escaped role-only opener followed by a disconnect before the first token still needs checking.

# Release note: Fixes

- **Streams with escaped JSON keys keep their error handling.** A role-only opener no longer hides an engine failure before its first token, and escaped metric keys are recognized without rewriting engine payloads.
