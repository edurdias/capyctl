# Status: Fresh connections for inference forwards — 2026-11-11 (branch `fix/em8-large-answer`)

SPEC §10 and ADR 0036 §§3, 4, 10: both inference forwarding hops now disable
idle connection pooling with `pool_max_idle_per_host(0)`. The cached reqwest
Client remains; each request opens one TCP connection, over loopback to an
engine or over the LAN to an enrolled host's ingress. This connection cost is
intentional: uncertain inference work is never replayed.

The focused EM8 run on vLLM 0.33.0 completed the initial 16 same-sized requests,
then its seventeenth request was reset before the response head. Diagnostics
recorded `ConnectionReset`, no timeout, zero received-body bytes and a supplied
10,486,312-byte request body. Uvicorn can expire an idle connection before its
protocol receives the next request's buffered bytes; closing with unread data
can reset an upload already started by the client. A delayed reset does not
establish a request-size limit or an inference timeout. A fresh socket avoids
that previous-response idle timer without changing the engine environment.

A shared, barrier-driven socket fixture keeps the first connection open until
it sees the next upload in its kernel buffer, then expires that connection
without parsing the new request. Both regression tests failed before the fix:
direct forwarding became uncertain with `ConnectionReset`; host ingress
returned 502. Both now pass and require exactly two accepted connections, two
parsed requests and zero reset attempts, ruling out replay as the repair. All
42 shared forwarding tests and all 18 ingress tests pass, including existing
large-answer streaming, cancellation and backpressure coverage.

Connection, response-head, request-deadline, stream-idle and body bounds remain
unchanged. There is no arbitrary size cutoff, batching or splitting: the
supported embeddings request still goes to the engine whole. CPU socket and
Fake-engine tests are not native-engine qualification; the native EM8 load,
large-answer and pinned-retrieval gates remain distinct qualification evidence.

# Release note: Fixes

- Inference forwards use fresh TCP connections instead of reusing idle sockets
  that an engine may expire before reading the next request. This avoids the
  resulting upload resets without retrying uncertain work; it adds one TCP
  connection setup per request and preserves all configured bounds.
