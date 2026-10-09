# Status: Wake canary for single-host launches — 2026-10-09 (branch `single-host-wake-canary`)

Owner decision 2026-10-09. Single-host wakes compare the engine's answer with a
reference, as groups do (ADR 0028 §12). At a launch's first readiness the
coordinator (`worker::record_wake_canary`) asks `EngineAdapter::wake_canary` for one
completion of `completion_probe::PROMPT` (`CANARY_TOKENS` = 8, temperature 0,
bounded by `CANARY_DEADLINE`) and stores the token ids and text in the new
`wake_canaries` table (schema v48), keyed by instance, generation and incarnation:
a controller restart keeps it, and a new launch records its own (earlier
generations are dropped). After a restore's own steps succeed and before its
evidence commits, `wake_canary_check` repeats the probe. Token ids are compared
with the group rule (`canary_matches`) when both answers carry them, the text
otherwise (`wake_canary::verdict`); a launch with no reference records one; an
unanswered canary leaves the wake uncertain, as a failed fresh probe does. A
different answer records `wake_mismatch` (the instance's last error and
`mismatch_operation_id`), marks the wake uncertain and accepts an ordinary stop
under `EXIT_PRINCIPAL` (as after an engine exit), retried every scheduler pass from
`wake_mismatch_stops_due`, so a restart retries it too. Every wake of a single
launch is checked, deep and `host_backed` alike.

Remote launches run the probe on the host agent (`MemberAction::Probe` with
`max_tokens`, sent only to hosts declaring `engine_groups`; older hosts keep the
wake checks they had); embedded vLLM and SGLang probe their own endpoint;
TensorFold is restart-only and unaffected. The completion probe now answers
`ProbeAnswer { tokens, text }` (adapters `complete_probe`, renamed from
`complete_token_ids`; new `MemberExecutionResult.probe_text`, at most 4096 bytes),
so an engine that returns no token ids is compared by text; groups still require
token ids. New closed code `wake_mismatch` (CLI codes, errors guide, parking guide).

Tests (new APIs, so they did not build on `main`; all pass after):
`capyctl-controller` `tests::residency::a_matching_wake_canary_completes_the_wake`
(remote and embedded, three cycles), `a_differing_wake_canary_fails_the_wake_and_stops_the_instance`,
`a_new_launch_records_a_new_reference`, `a_controller_restart_keeps_the_reference`,
`wake_canary::tests::*`; `capyctl-store` `wake_canary::tests::*` (including a
reopened store); `capyctl-protocol` `probe_text_answers_a_completion_probe_only`;
adapter `completion_probe` unit and integration tests for the text fallback. CPU
and Fake-engine tests are not qualification.

Live check still needed: model 1 on vLLM, deep park and wake three times on a
GB10, each wake passing the canary. A known-broken wake is refused upstream
(#76), so a mismatch is tested by hand: on a maintainer's local standalone
machine (never a lab server), with a vLLM model ready, stop the role, run
`UPDATE wake_canaries SET tokens_json='[1]', text='x'` on its store, start the
role (it adopts the running engine), then park and wake the model; the wake must
fail `wake_mismatch` and the instance stop, `wake_mismatch` as its last error.

# Release note: Fixes

- **A woken model must answer as it did before parking.** After a vLLM or
  SGLang model on one machine wakes, CapyCTL asks it a fixed prompt and
  compares the answer with the one it gave when it first became ready. A
  different answer, for example from weights that did not reload intact,
  fails the wake with `wake_mismatch` and stops the model instead of serving
  wrong answers. See [Parking and switching](../guide/parking.md#park-and-wake).
