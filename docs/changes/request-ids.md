# Status: Request IDs and decision metrics — 2026-10-20 (branch `feat/request-ids`)

Owner decisions of 2026-10-19, recorded as two SPEC §10 notes and an amendment
entry. A request to `/v1/chat/completions` or `/v1/systemone` may carry the
caller's own `X-Request-Id`: 1 to 128 bytes of printable ASCII with no space or
control character, refused `400 invalid_request` naming the header after
authentication and before the body is read. A well-formed one is echoed on the
answer, success or error, a stream's response headers included. CapyCTL consumes
it: the router never sends it to a host's ingress or an engine (the ingress
rebuilds every upstream header, which a test now pins for both request IDs), it
goes only into the router's role logs for the request, and it is never written
to the ledger or an event. Every answer of both endpoints carries
`X-CapyCTL-Request-Id`, the ID of the request's first granted lease, or a fresh
ULID when the request was refused before any lease. The router's
`router_selection` and `router_failover` lines carry `capyctl_request_id` and,
when given, `caller_request_id`; lines decided before the first lease are held
until the offer is decided, so they carry the lease's ID. A lease close that
could not be written names both IDs. `request_cancellation_acknowledged` is
unchanged and keeps only CapyCTL's identifiers. A decision's answer now carries
`capyctl.metrics`: the router's `total_ms` and `time_to_response_ms`, the
engine's `input_tokens` from `usage`, and `queue_ms`, `prefill_ms` and
`cached_tokens` when the engine reports them, each labelled with its source;
absent values stay absent.

Tests: `crates/capyctl-router/tests/request_ids.rs` (T17 T19 T21 T43: echo on
collected, streamed and decision answers and on errors after a lease; the CapyCTL
ID equals the lease ID; a fresh ULID per request refused before any lease; a
malformed or repeated header refused before anything is leased or sent; an engine
double that records headers never sees either ID, and its own `X-Request-Id`
never reaches the client), the `request_id` and `engine_metrics` unit tests,
`decisions.rs` (decision metrics present, no time to first token or decode
speed), `crates/capyctl-cli/tests/router_payloads.rs` (router, engine forwarder,
host ingress and engine: both IDs on the answer, neither at the engine) and
`crates/capyctl-agent/tests/ingress.rs` (the ingress forwards neither ID). Before
the change the headers were absent and the decision test asserted no
`capyctl.metrics`. CPU and Fake-engine tests are not qualification. The live
check still needed: on host A, a chat request (collected and streamed) and a
decision on llama.cpp v0.7.0 or SGLang 0.5.22 sent through the server to a host's
ingress with `X-Request-Id`, confirming both headers on the answers, the
`capyctl_request_id` in the router's role log matching the lease, and which
engine figures each decision engine actually reports.

# Release note: Requests

- **Request IDs.** Every answer from `/v1/chat/completions` and `/v1/systemone`
  carries `X-CapyCTL-Request-Id`, CapyCTL's ID for the request, also written in
  its logs. Send your own `X-Request-Id` (1 to 128 printable ASCII characters,
  no spaces) and it comes back on the answer and appears beside CapyCTL's ID in
  the router's logs; it is never sent to the engine or stored. Decision answers
  now carry `capyctl.metrics` too: total time, time to the engine's answer,
  input tokens, and the engine's own figures when it reports them. See
  [Request IDs and metrics](../guide/requests.md#request-ids-and-metrics).
