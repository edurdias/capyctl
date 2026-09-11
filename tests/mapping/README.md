# F0 coverage mapping — target spec ids → tests

Every F0 target test id and the concrete `#[test]` / `#[tokio::test]` functions that
claim it. Audited at F0 exit gate (commit 932e26f, task 13 of the F0 foundation plan).
Ids outside the F0 target set (T05–T07, T10–T25, T28–T40) belong to later milestones
and are deliberately absent.

| Spec id | Claim | Requirement (F0 tier: fake engine / simulator only) |
|---|---|---|
| T01 | `crates/mllm-cli/tests/grammar.rs` | Action-first CLI grammar parses all F0 verbs |
| T02 | `crates/mllm-config/tests/noconfig.rs` | Missing/implicit config generates once, owner-protected |
| T03 | `crates/mllm-config/src/strict_yaml.rs` (unit tests) | Strict-YAML config schema: rejects invalid, accepts valid |
| T04 | `crates/mllm-config/tests/noconfig.rs` | Concurrent inits generate credentials once, no clobber |
| T08 | `crates/mllm-store/tests/acceptance.rs` | Deployment id persists across new client |
| T09 | `crates/mllm-store/tests/acceptance.rs` | Same key returns same deployment (idempotency) |
| T26 | `crates/mllm-scheduler/src/admission.rs` (unit tests) | Unified-memory charging counted once |
| T27 | `crates/mllm-scheduler/src/admission.rs` (unit tests) | Disjoint devices still share system RAM in accounting |

## Detailed mapping

### T01 — action-first grammar (`mllm-cli/tests/grammar.rs`)

- `action_first_grammar` — the core grammar shape (all F0 verbs)
- `list_and_status_never_activate`
- `start_roles_with_config`
- `init_targets`
- `invite_join_inspect_doctor_qualify`
- `deploy_flags`
- `lifecycle_forms`
- `validate_config_file`
- `machine_mode_output_flag`
- `malformed_invocations_rejected`

Adjacent exit-code/JSON behavior lives in `crates/mllm-cli/tests/errors.rs`
(`exit_code_table_matches_design_section_7`, `parse_errors_map_to_invalid_config`,
`structured_error_json_shape`, `not_yet_implemented_error`,
`output_format_flag_parsing`).

### T02 — no-config startup + protected generation (`mllm-config/tests/noconfig.rs`)

- `t02_missing_implicit_generates_once_with_protected_files` — generates once,
  config 0600, state root / identity dir 0700, credentials 0600, no engine
  execution at startup, second call loads without regenerating (mtime stable)

Supporting same-file coverage: `implicit_existing_invalid_is_an_error_not_a_reset`,
`explicit_missing_path_fails`, `explicit_invalid_path_fails`,
`explicit_valid_path_loads_without_generating`,
`non_standalone_generation_is_rejected`.

### T03 — strict YAML schema (`crates/mllm-config/src/strict_yaml.rs`, `#[cfg(test)] mod`)

- `duplicate_key_rejected`
- `unknown_mllm_field_rejected`
- `unknown_nested_field_rejected`
- `unknown_standalone_server_block_field_rejected`
- `invalid_unit_rejected`
- `missing_required_rejected`
- `multi_document_rejected`
- `schema_version_two_rejected`
- `valid_server_parses_to_json_view`
- `valid_unit_accepted`

Cross-check that generated configs satisfy the T03 schema:
`generated_config_passes_task3_validate` in `mllm-config/tests/noconfig.rs`.

### T04 — concurrent init (`mllm-config/tests/noconfig.rs`)

- `concurrent_starts_do_not_clobber` — 8 threads resolve startup in parallel
  against the same state dir; credentials generated exactly once

### T08 / T09 — store acceptance (`mllm-store/tests/acceptance.rs`)

- `t08_id_returned_after_persistence_survives_new_client` — id survives
  persistence and a fresh client handle
- `t09_retry_with_same_key_returns_same_deployment` — idempotent retry by key

Supporting same-file coverage: `same_key_different_content_is_conflict`,
`store_file_is_owner_only`.

### T26 / T27 — admission memory accounting (`mllm-scheduler/src/admission.rs`, `#[cfg(test)] mod`)

- `t26_unified_memory_charged_once` — unified memory charged once
- `t27_disjoint_devices_still_share_system_ram` — system RAM shared in
  accounting across disjoint devices

Supporting same-file coverage: `t24_shape_sublimit_blocks_on_retained_host_kv`,
`replace_dont_stack_includes_candidate_in_charged`,
`stale_observation_blocks_admission`,
`transition_peak_must_cover_parked_residue`; sizing constants in
`mllm-scheduler/src/auto.rs` (`constants_match_adr_0005`,
`small_host_degenerates_with_diagnostic`).

## Supporting evidence

### Fake-engine scenario suite (`mllm-adapters/tests/fake_scenarios.rs`)

The fake engine is the executable spec for behaviors real F1/F2 adapters must
honor:

- `slow_startup_liveness_is_not_readiness` — liveness is not readiness
- `sleep_level_two_discards_weights_and_kv`
- `level_one_park_keeps_cpu_weight_backup`
- `ambiguous_park_reports_uncertainty_not_success`
- `cancellation_without_ack_reports_uncertainty`
- `deep_park_denied_without_policy_opt_in`
- `reload_weights_also_denied_without_policy_opt_in`
- `pid_reuse_rejects_stale_handle`
- `crash_at_phase_is_reported`

### Conformance suite (`tests/harness/src/lib.rs`, `#[cfg(test)] mod`)

- `fake_engine_passes_full_conformance_suite` — the fake engine passes every
  harness check
- Readiness gating: `fabricated_ready_adapter_fails_readiness_gating`,
  `uncertain_readiness_warns_not_fails` (uncertainty is never fabricated
  readiness)
- Park policy gate: `deep_park_denied_without_policy_opt_in` /
  `reload_weights_also_denied_without_policy_opt_in` (fake_scenarios.rs) and
  `embedded_host_denies_experimental_deep_park_by_default`
  (`mllm-agent/src/lib.rs`)
- Cancellation uncertainty: `cancellation_without_ack_reports_uncertainty`
  (fake_scenarios.rs) and `cancel_without_ack_is_uncertain_never_success`
  (`mllm-adapters/src/lib.rs`)
- Handle ownership: `reuse_oblivious_launcher_fails_handle_ownership` and
  `pid_reuse_launcher_exercises_reuse_detection` (harness),
  `pid_reuse_rejects_stale_handle` (fake_scenarios.rs)
- Launcher contract: `launcher_contract_is_object_safe_and_matches_handle_status`,
  `payload_types_carry_the_fields_task_9_reads` (`mllm-adapters/src/lib.rs`)

### Wire round-trip and skew tolerance (`mllm-protocol/tests/wire_roundtrip.rs`)

- `agent_control_roundtrip_over_real_channel` — Envelope round-trips over a
  real tonic channel (simulator-tier transport evidence)
- `deadline_enforced_with_skew_tolerance`
- `protocol_version_is_pinned`

### F0 exit-gate lifecycle (`mllm-cli/tests/standalone_lifecycle.rs`)

- `standalone_boot_runs_full_fake_lifecycle` — full embedded lifecycle over the
  fake engine: deploy → start → ready → stop → stopped
- `resubmitting_the_same_request_is_idempotent`
- `stop_is_illegal_from_stopped`

Embedded-host piece-level support: `embedded_host_runs_the_fake_lifecycle_pieces`
(`mllm-agent/src/lib.rs`).

## Gap check result

All F0 target ids (T01–T04, T08, T09, T26, T27) are claimed by real, named
tests. No unclaimed ids; no tests were added by this audit. F0 claims nothing
about real engines, GPUs, or hardware — the fake-engine suite is the simulator
tier and the wire round-trip is simulator-tier transport evidence.