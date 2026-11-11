# Status: Safe inference-forwarding failure diagnostics — 2026-11-11 (branch `fix/em8-large-answer`)

SPEC §§10, 13.3 and ADR 0036 §4: an inference send failure now records a
structured `inference_forward_failed` role-log event. Embeddings also record
body-transfer errors, an unexpected response head, and incremental-validation
or end-of-answer failures. Events contain the fixed endpoint, observed phase,
elapsed and wall-clock time, supplied request-body size, received-body bytes,
reqwest error categories, recognized peer closure and an I/O error kind when
available. Request/response contents, credentials, upstream URLs and arbitrary
error messages are never recorded. Sent bytes and connection reuse remain
unknown (`null`): reqwest does not expose them. A send failure cannot distinguish
uploading headers/body from awaiting the response head, so its phase says
`send_or_await_head` rather than inventing a finer observation.

The logging does not change forwarding, retries, deadlines, validation or lease
settlement. Focused socket tests cover peer closure and timeout categories and
prove that private URL, request and credential strings do not enter the event.
CPU socket tests are not native-engine qualification; the remaining EM8 live
failure needs a rerun with these diagnostics before attributing its cause.

# Release note: Fixes

- Failed inference forwards now record secret-safe transport diagnostics;
  embeddings failures also identify transfer and answer-validation phases.
