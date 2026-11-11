# Status: Inference forwarding follows the router's bounds — 2026-11-10 (branch `fix/em8-forward-timeout`)

SPEC §10 and ADR 0036 §§4, 10: the shared inference forwarder and the host
ingress now explicitly disable reqwest's implicit 30-second TCP user timeout.
A busy engine can stop reading an upload and advertise a zero receive window
without losing its connection. That backpressure must not close the request
before its configured bounds. Connection timeouts, the ingress response-head
bound, router request deadlines, stream-idle bounds and conservative lease
accounting are unchanged; no request is retried. The shared forwarder covers
chat, decisions and embeddings, both direct and through a host ingress.
Control and readiness clients keep their existing operation bounds.

Two Linux socket regressions send a large embeddings body to a healthy peer
that starts reading after 35 seconds. Before the fix, the direct forward
became uncertain after 30.60 seconds and the ingress returned 502 after 30.45
seconds. Both pass after the fix. All 39 shared forwarding tests and all 17
host-ingress tests pass, including their existing large streamed-answer and
cancellation coverage; `cargo fmt --all -- --check` passes. CPU socket and
Fake-engine tests are not native-engine qualification. EM8 still needs its
native load, large-answer and pinned-retrieval qualification.

# Release note: Fixes

- Large inference uploads no longer fail at an implicit 30-second transport
  deadline when a busy engine temporarily stops reading them. Configured
  request, connection, response-head and stream-idle bounds remain in force.
