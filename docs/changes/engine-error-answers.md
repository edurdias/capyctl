# Status: Engine error answers end cancelling, not uncertain — 2026-10-17 (branch `fix/engine-error-answers`)

Found live on 2026-10-17 on host A (llama.cpp v0.7.0, decision deployments
Julia-1 and Laya, context 8192): an input over the context got llama.cpp's
complete JSON error with HTTP 500 (`input (8195 tokens) is too large to
process. increase the physical batch size`). The forwarder read every 5xx but
a `shutting_down` 503 as uncertain, the router ended the request lease
uncertain, and an uncertain lease stays charged for the life of the session,
so after about 56 such answers the deployment refused every request with 429
`queue_full` ("outstanding request bound reached") until it was deleted.
Clients saw only `engine_error` "backend completion unverified".

Spec §10 is amended (2026-10-17, with the T17 row and the amendment list):
an engine's complete error answer, HTTP 5xx other than 503 with a JSON object
body read whole within the bounds a rejection is read with (16 KiB, 5 s), or,
before any output, a stream error event with such a code, is relayed to the
client as HTTP 502 `engine_error` with the engine's bounded message,
`retryable: false`, in-band on a stream, and never offered to another
instance. Uncertainty still retains accounting: the answer does not prove the
engine stopped, so the lease ends `cancelling`, charged until the instance's
adapter reports engine-wide quiescence, exactly as after a client hang-up
(the coordinator's existing settlement; its receipt now reads "after the
request was cancelled or answered with an error"). That settlement already
covers local llama.cpp (`/metrics` `requests_processing` and
`requests_deferred` both 0), SGLang and vLLM (their running and waiting
gauges) and remote launches (the host's load report, including the ingress's
in-flight count). A truncated, oversized, unreadable or non-JSON 5xx body,
and every 503 but a role's `shutting_down` refusal, stays uncertain.

- **Forwarder** (`capyctl-adapters` `forward.rs`): `engine_error_status`,
  `engine_error_message` and a new `AdapterError::EngineError { status,
  message }`, on every path: decisions, collected and streamed chat, and the
  in-band error event before any chunk.
- **Host ingress**: relays such an answer with the engine's status as
  `engine_error` and its message; its own failures keep the code
  `inference_unavailable` (`INGRESS_FAILURE_CODE`), which the forwarder never
  reads as the engine's answer.
- **Router**: `EngineError` ends the lease `cancelling` and answers 502
  `engine_error` (collected, decisions) or an in-band `engine_error` event
  (streams); the in-memory slot is released as for an uncertain end, since
  the ledger keeps the charge.
- **Controller**: `EngineError` is `Uncertain` in fault classification and
  operations (`engine_error:<status>`).

Tests (T17, T19, T43): `capyctl-cli`
`roles_f1` `engine_error_answers_never_exhaust_the_deployment` (a standalone
role with the real coordinator and ledger; an engine answering every request
with llama.cpp's 500 gets three times the outstanding bound of answers
through, each a 502 with the engine's message, never replayed) and
`a_cut_engine_error_answer_stays_uncertain` (a 500 whose body is cut short
leaves its lease uncertain and the bound refuses with 429 after several
quiescence questions, as before the change); `capyctl-adapters` forwarder unit tests
`an_engine_error_answer_carries_the_engine_message` and the updated status
and in-band event tests; `capyctl-router` `router_core`
`an_engine_error_answer_is_relayed_and_leaves_the_lease_cancelling`,
`decisions` `an_engine_error_answer_is_relayed_and_leaves_the_lease_cancelling`
and the cut 500 in `an_unverified_decision_answer_leaves_the_lease_uncertain`,
`stream_safety` lease ends; `capyctl-agent` ingress
`engine_answers_are_relayed_and_other_engine_errors_stay_bad_gateway`. Every
new engine-error test, and the full-stack one, fails with the forwarder's
5xx reading turned off; with the router ending the lease uncertain instead
of cancelling, the full-stack test fails on the deployment staying
exhausted. CPU and Fake-engine tests are not qualification.

Live on host A, 2026-10-18, this branch's build in an isolated standalone,
llama.cpp v0.7.0: Julia-1 and Laya (Q8_0, context 8192) under capyctl-bench's
decisions run. Each model's 8k-token latency points sent 31 inputs over the
context; every one answered 502 `engine_error` with llama.cpp's message
("input (… tokens) is too large to process. increase the physical batch
size"), and the ledger journaled 41 `request_cancellation_acknowledged`
events. Both deployments kept serving: the throughput runs that followed
(200 requests at each of 1, 2, 4 and 8 streams) had no failures, where
before this change the same runs on `main` got 429 `queue_full` for every
request.

# Release note: Fixes

- **An engine's error no longer locks a model.** When the engine answers a
  request with an error (llama.cpp refusing an input longer than its batch,
  for example), the client now gets `502 engine_error` with the engine's own
  message instead of `500 backend completion unverified`, and the request
  stops counting against the model's 32 running requests once the engine is
  idle. Before, a few dozen such errors left the model answering every
  request with `429 queue_full` until it was deleted. See
  [Errors](../guide/errors.md).
