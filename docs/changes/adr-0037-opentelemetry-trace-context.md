# Status: ADR 0037 proposed: OpenTelemetry trace context — 2026-10-24 (branch `docs/adr-otel`)

ADR 0037 proposes, for 0.1.9 and for the owner to decide, how CapyCTL takes part in a
distributed trace. Tracing stays off unless a role names an OTLP endpoint
(`observability.tracing.endpoint`). With it on, the router reads W3C `traceparent` and
`tracestate` after authentication on the inference endpoints, ignores malformed ones
(never refuses a request for them), and records fixed spans for admission, queue wait,
activation wait, dispatch, the forward to the engine or ingress, and settlement; an
activation (cold start, wake, switch) is its own trace, linked from each request that
waited for it. Spans carry an allow-list of identifiers, timings, codes and token
counts, never request or response content, the caller's `X-Request-Id`, salts or keys;
`X-CapyCTL-Request-Id` is the join key. Remote hosts get the context through a
`trace_context` capability and the command envelope, outside the journal. Engine spans
(vLLM 0.31.0 or newer, SGLang 0.5.22 or newer) are a host opt-in with a loopback-only
endpoint and a launch-time check; the engines' tracing options become owned. Export is
OTLP over HTTP/protobuf with a bounded queue, never on the request path, ignoring
`OTEL_*` variables. Zero retention keeps engine tracing off. The ADR lists T48, live rows
OT1–OT11 and twelve open questions. Docs only; nothing implemented. CPU and Fake-engine
tests are not qualification; the live rows have not run.

# Release note: none
