# Status: A wake that does not complete stops the instance — 2026-10-15 (branch `fix/vllm-deep-wake`)

Found live on host A (GB10, standalone, vLLM 0.30.0, Qwen3.8-27B NVFP4, deep park, no
declared loader, so `lazy` under ADR 0014 amendment A21; run
`target/live/0.1.3-backlog/2026-10-14-2311-release-checks/`, check B1). The first wake's
`/wake_up?tags=weights` returned at once, then vLLM's `Reloading weights inplace` took
123 s. CapyCTL's `reload_weights` collective had a fixed 120 s bound
(`WEIGHT_RELOAD_TIMEOUT_SECS`), so at 121 s the step failed as uncertain (`collective_rpc:
error sending request`) and the request got `activation_uncertain`. The engine finished the
reload 2 s later and was never asked anything again. An uncertain restore is never re-armed
and nothing stopped it, so the instance read `uncertain` with its restore `running`, its
memory charged and dispatch closed, for 21 minutes until the script deleted the deployment;
the next two wakes failed at once on it.

vLLM logged `WARNING [layerwise.py:268] MRotaryEmbedding: Failed to load weights` during that
reload. It is not evidence of a failed reload: the six correct `eager` wakes of check B4 on
the same model log it too. In vLLM 0.30 it is `finalize_layerwise_processing` putting a
layer's kernel tensors back when the checkpoint holds none of its weights, and
`reload_weights` returns `None` either way, so the collective's answer carries no load
result. CapyCTL does not scrape engine logs for it. The structured signal vLLM does give is
the collective's status: when `reload_weights` raises, `/collective_rpc` answers an error.

What changed (SPEC §9.1, §13.2, T20; the 2026-10-09 owner decision for `wake_mismatch`,
extended):

- **One deadline for the whole restore.** The persisted restore was already bounded by its
  wake window (`timeouts.wake`, under the Initialize ceiling), but the reload had its own
  shorter, fixed 120 s bound and the wake canary was bounded only by the adapter honouring
  the bound it was passed. Now vLLM's persisted `ReloadWeights` step gives the collective
  what is left of the restore's deadline (`EngineHttp::collective_rpc(timeout)`), and the
  coordinator runs the canary under the same deadline (`execute_residency`); the canary at
  first readiness is bounded by `CANARY_DEADLINE` in the coordinator too. The adapter-level
  `EngineAdapter::reload_weights` keeps the 120 s bound.
- **A failed reload fails the wake.** A `reload_weights` collective vLLM answers with an
  error status is `RuntimeError::ReloadFailed` (new): the wake fails with the new closed code
  `wake_reload_failed`, and the adapter refuses every later step, as before.
- **A wake that ended without its evidence stops the instance.** Every single launch's wake
  that ends uncertain (deadline passed, a step's outcome unknown, canary unanswered, evidence
  not accepted, the controller stopped during it) records `wake_uncertain` as the instance's
  last error (`Store::record_wake_failure`), unless it is `wake_reload_failed` or
  `wake_mismatch`, and the step reads uncertain. `Store::wake_stops_due` (replacing
  `wake_mismatch_stops_due`) lists every single-launch instance whose restore at its current
  generation ended uncertain and that still reads desired ready, whatever the code, and the
  scheduler retries each one's ordinary stop every pass under `EXIT_PRINCIPAL`
  (`retry_wake_stops`, keyed `wake-stop:<operation>:<generation>`). As for `wake_mismatch`,
  the host terminates the recorded engine and the reservation, claim and gate are released
  only on its gone evidence; status reads `failed` with the code as last error, and the next
  request starts the instance fresh. A group's wake still owes its group stop
  (`group_residency_stops_due`); an uncertain park still waits for an operator stop.
- Remote hosts report every failed restore step as uncertain (the host protocol has no
  reload-failed category), so a remote reload failure reads `wake_uncertain`, with the same
  stop.
- New closed status codes `wake_reload_failed` and `wake_uncertain` (no exit; CLI codes,
  errors guide, parking guide).

Tests, failing before and passing after:

- `capyctl-controller`
  `tests::residency::a_wake_whose_reload_never_returns_is_stopped_and_starts_fresh`
  (embedded, the reload never answers), `a_wake_whose_canary_never_answers_is_stopped_and_starts_fresh`
  (the canary never answers) and `an_uncertain_wake_is_stopped_and_starts_fresh` (remote,
  outcome lost): on `origin/main` all three time out after 30 s waiting for the release (the
  wake stays uncertain and charged; with the canary, the worker hangs); now the instance is
  stopped once, released on the host's gone evidence, reads `stopped` with
  `wake_uncertain`, and the next on-demand request starts a fresh launch that reaches Ready.
- `capyctl-controller` `tests::residency::a_wake_whose_reload_fails_is_stopped_and_starts_fresh`
  and `capyctl-adapters` `vllm_residency::a_failed_reload_fails_the_wake_and_is_never_repeated`
  (renamed from `a_failed_reload_is_uncertain_and_never_repeated`; a 500 from
  `/collective_rpc` was uncertain before): new API, so they did not build before.
- `capyctl-store` `wake_canary::tests::an_uncertain_wake_owes_the_stop_until_a_new_generation`
  and `a_mismatch_names_its_code` (replacing `a_mismatch_owes_the_stop_while_its_wake_is_uncertain`).
- `capyctl-cli` `group_status_codes_have_no_exit` covers the two new codes.

CPU and Fake-engine tests only; they are not qualification. Live check still needed: on host
A, model 1 (Qwen3.8-27B NVFP4) on vLLM 0.30 with deep park and a declared
`safetensors_load_strategy: lazy`, park and wake three times. Each wake either answers
correctly (the reload now has the 180 s wake window), or ends within the wake window with
the instance stopped, `wake_uncertain` or `wake_reload_failed` as its last error, its memory
released once the engine is gone, and the next request starting it fresh. Never an instance
left `uncertain`.

# Release note: Fixes

- **A wake that fails or hangs no longer leaves a model stuck.** If a parked model's wake
  does not finish within its wake window (`timeouts.wake`), or vLLM reports that reloading
  the weights failed, CapyCTL now stops the model, with `wake_uncertain` or
  `wake_reload_failed` as its last error, instead of leaving it uncertain with its memory
  held until you stopped it by hand. The next request loads it fresh. A vLLM weight reload
  during a wake may now take the whole wake window instead of at most two minutes. See
  [Errors](../guide/errors.md).
