# F1 Coverage Audit — Test-ID Mapping

Every F1 target test id (F1 design §9 / SPEC §18 F1) mapped to its concrete
test function(s). Test names grepped from the tree (never invented); live-tier
evidence cites `docs/runbooks/spark-qualification-f1.md` sections.

| Test | Scenario | Simulator tier (test fns) | Live tier (Spark) |
|---|---|---|---|
| T07 | Online host without prepared runtimes | `mllm-agent/tests/doctor.rs::doctor_reports_missing_profile_without_launching` (profile missing → specific failure, no install) | §2 doctor capture; §3 deploy preflight on missing profile |
| T10 | Administrative stop vs idle stop | `mllm-controller/tests/managed_ops.rs::administrative_stop_blocks_autoactivation`, `::idle_stop_leaves_on_demand_eligible` | restart-only qualification step: live stop/re-deploy |
| T11 | Attached service | `mllm-controller/tests/attachment.rs::attach_registers_route_and_rejects_lifecycle`, `::attached_usage_is_conservative_not_reclaimable`, `::restart_guarantees_marked_unavailable` | deferred (live attach step held with the switch stage; §5 pending — Spark unreachable) |
| T12 | Patched foreground wrapper | `mllm-controller/tests/managed_ops.rs::start_spawns_real_process_and_stop_terminates_it`, `mllm-launchers/tests/exec.rs::terminate_reports_signal_when_grace_expires`, `::spawn_creates_live_handle_and_terminate_stops_group` | real vLLM launch/termination on Spark |
| T14 | Reserved flags / profile change | `mllm-adapters/tests/vllm_args.rs::reserved_flag_conflicts_fail`, `::user_sleep_flag_is_reserved_not_rendered`, `::fingerprint_redacts_api_key_values` | fingerprint capture via `doctor`; profile-change invalidation at recipe freeze |
| T15 | Simultaneous activation | `mllm-router/tests/switching.rs::simultaneous_activations_join_one_wake` | deferred (live simultaneous-wake step held with the switch stage; §5 pending — Spark unreachable) |
| T16 | A → B → A | `mllm-router/tests/switching.rs::a_to_b_to_a_alternates_with_release_evidence` | §3 two-profile alternation with release evidence |
| T17 | Active streaming during swap | `mllm-router/tests/router_stream.rs::streaming_chat_returns_sse_events_in_order`, `::abandoned_request_keeps_inflight_until_confirmed` | §3 streaming through a switch (if platform allows) |
| T18 | Late ingress request | `mllm-controller/tests/generations.rs::stale_generation_dispatch_rejected` | stale dispatch at live tier where reachable |
| T19 | Fairness and queue bounds | `mllm-router/tests/switching.rs::fairness_window_is_bounded_and_non_resetting`, `mllm-router/tests/router_core.rs::queue_bounds_return_structured_error` | — (simulator) |
| T20 | Park/reload timeout or partial failure | `mllm-controller/tests/park_flow.rs::ambiguous_park_reconciles_without_blind_repeat` | §4 ambiguous park (kill mid-sleep) → reconcile |
| T21 | Experimental-controls policy | `mllm-controller/tests/policy_gate.rs::vllm_sleep_profile_launch_denied_by_default`, `::opt_in_enables_the_experimental_profile`, `mllm-adapters/tests/vllm_adapter.rs::park_denied_by_default_without_engine_call` | §5 opt-in live path only; denial live check |

## Supporting evidence (conformance + contracts)

- Fake-engine conformance (runs against ANY adapter): `tests/harness` — readiness
  gating, park policy gate (level-2-only and profile-gated modes), cancellation
  uncertainty, handle ownership. The vLLM adapter passes it:
  `mllm-adapters/tests/vllm_adapter.rs::passes_conformance_suite`.
- vLLM HTTP client: `mllm-adapters/tests/vllm_http.rs` (SSE, uncertainty on dropped
  ack, reachability).
- Argument rendering: `mllm-adapters/tests/vllm_args.rs` (budgets → explicit units,
  reserved-flag conflicts, secret redaction).
- Real launcher: `mllm-launchers/tests/exec.rs` (process groups, SIGTERM→SIGKILL
  escalation, PID-reuse detection).
- Doctor: `mllm-agent/tests/doctor.rs` (fingerprints, memory observation, no install).
- Generations/reservations: `mllm-controller/tests/generations.rs`.
- Standalone wiring: `mllm-cli/tests/roles_f1.rs`, `mllm-cli/tests/standalone_lifecycle.rs`.

## Live-tier ledger

Live claims exist ONLY in `docs/runbooks/spark-qualification-f1.md` and are labeled
`live-tier (Spark)`. Simulator claims above never substitute for them.