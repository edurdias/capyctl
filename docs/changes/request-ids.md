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
when given, `caller_request_id` in JSON output; in the default text output
(ADR 0021) they read `request <capyctl id> (caller <id>) <deployment> -> <host>
instance <n>`, the caller part only when given (found by the live check on host
A: the text form first showed neither ID). Lines decided before the first lease
are held until the offer is decided, so they carry the lease's ID. A lease close
that could not be written names both IDs. `request_cancellation_acknowledged` is
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
the `role_text` unit test `router_lines_name_the_request_ids` (the text form
names both IDs, and nothing extra without a caller ID),
`decisions.rs` (decision metrics present, no time to first token or decode
speed), `crates/capyctl-cli/tests/router_payloads.rs` (router, engine forwarder,
host ingress and engine: both IDs on the answer, neither at the engine) and
`crates/capyctl-agent/tests/ingress.rs` (the ingress forwards neither ID). Before
the change the headers were absent and the decision test asserted no
`capyctl.metrics`. CPU and Fake-engine tests are not qualification.

Live on host A, 2026-10-20, with this branch (`5de86c59`) merged with
`feat/zero-retention` and `fix/sglang-flag-reservations` on main (integration
build `daffad0d`), an isolated standalone, Julia-1 Q8_0 as a decision
deployment on llama.cpp v0.7.0 and Qwen3-4B chat on SGLang 0.5.22, each request
sent with `X-Request-Id: live-check-1`: the decision, a collected chat and a
streamed chat (`text/event-stream`, 6 data lines) answered 200 with
`x-request-id: live-check-1` and an `x-capyctl-request-id` ULID, a different one
per request; a request without the header got only `x-capyctl-request-id`. The
decision's `capyctl.metrics` held `input_tokens` 33 (engine), `time_to_response_ms`
30.2 and `total_ms` 31.4 (router); llama.cpp reported no `queue_ms`,
`prefill_ms` or `cached_tokens`, and they were absent. `X-Request-Id: live check`
was answered `400 invalid_request` ("X-Request-Id must be one value of 1 to 128
printable ASCII characters with no spaces") with an `x-capyctl-request-id` and
no echo. Each answered request had one `router_selection` line naming its
`X-CapyCTL-Request-Id`: with `--format json` as `capyctl_request_id` and
`caller_request_id: "live-check-1"`, and in text as `request
01M5C29SXMR5FQ3133Q2RE59QG (caller live-check-1) 01M5C26QQBW5 -> <host A>
instance 0` (the caller part absent for the request without the header). Neither
engine log contained `live-check-1` or `capyctl-request-id`. The standalone's
embedded host was used, so the path through a separate host's ingress was not
run live; `ingress.rs` and `router_payloads.rs` cover it.

# Release note: Requests

- **Request IDs.** Every answer from `/v1/chat/completions` and `/v1/systemone`
  carries `X-CapyCTL-Request-Id`, CapyCTL's ID for the request, also written in
  its logs. Send your own `X-Request-Id` (1 to 128 printable ASCII characters,
  no spaces) and it comes back on the answer and appears beside CapyCTL's ID in
  the router's logs; it is never sent to the engine or stored. Decision answers
  now carry `capyctl.metrics` too: total time, time to the engine's answer,
  input tokens, and the engine's own figures when it reports them. See
  [Request IDs and metrics](../guide/requests.md#request-ids-and-metrics).
