# Status: A stream that fails before its first token ends with an error event — 2026-10-30 (branch `fix/stream-empty-200`)

Issue #213, SPEC §10. A streamed chat completion whose forward failed before
anything reached the client could end as an HTTP 200 `text/event-stream` with
no events at all: no content, no error event and no `data: [DONE]`. Two
defects caused it.

- The router's stream pump answered in-band only for refusals (an engine
  rejection, an engine error answer, a refusal before sending, a request not
  accepted). Any other error, such as an uncertain end, closed the stream
  silently. It now sends an error event when the forward ends in an error and
  no engine chunk was handed to the client: code `engine_error`, message
  `backend completion unverified`, `retryable: false`, as the non-streamed
  answer to the same failure. Lease accounting is unchanged: the lease stays
  uncertain, and the per-process slot is released only with a durable lease,
  as before.
- A host ingress at its 64-forward limit answers 429 with its own
  `inference_unavailable` code before anything reaches the engine. The
  forwarder passed that 429 through and the stream reader called it
  uncertain. The forwarder now reads a 429 whose body carries the ingress's
  code as not accepted, so the plan fails over to another instance, the
  refused lease closes as not accepted, and with no other instance the client
  gets a retryable `unavailable` (503, or an error event on a stream). No new
  marker was needed: the ingress never relays an engine's 429 (it answers any
  other engine status as its own 502), so a 429 with that code comes from the
  ingress's capacity refusal alone. Any other 429, an engine's own included,
  stays uncertain. This is a new not-accepted case under SPEC §10's rule that
  only evidence the engine never saw a request closes it as not accepted, for
  the owner to review.

Tests, failing before and passing after:

- `router_core::an_ingress_at_capacity_is_a_retryable_refusal_never_an_empty_stream`
  (before: the stream body was empty, `no event: ""`).
- `router_core::an_uncertain_failure_before_the_first_token_is_an_in_band_error`
  (an engine's own 429, and a 200 that is not a stream; before: empty stream).
- `instances::an_ingress_at_capacity_fails_over_to_another_instance` (a real
  forwarder against an ingress answering 429; before: no failover, the
  stream did not carry the other instance's answer).

CPU and Fake-engine tests are not qualification. Live check still needed: on
a host with several busy deployments whose requests together exceed the
ingress's 64 forwards, a streaming client sees either output from another
instance or a retryable error event, never an empty 200 stream, and the
refused leases close as not accepted.

# Release note: Fixes

- **A streaming request that fails before its first token now ends with an
  error event** instead of an empty HTTP 200 stream. When a serving host's
  ingress is at its forward limit, the request goes to another instance of
  the model, or the client gets a retryable `unavailable` error.
