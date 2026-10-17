# Status: Decision models, slice B: `POST /v1/systemone` — 2026-10-17 (branch `feat/decisions-route`)

ADR 0030 (accepted, owner decisions 2026-10-17) §2 and §5. Spec §10 gains
the decision endpoint (note and amendment log entry). Readiness and the wake
check for decisions deployments (ADR 0030 §§3–4) are another slice.

- **Router.** `POST /v1/systemone` shares the chat endpoint's API key, body
  bound, JSON parse, lookup by `model`, waiting, activation, admission,
  instance selection, request lease and settlement (one dispatch path,
  `chat::dispatch_timed`, with an `Endpoint`). The request is validated
  against its own closed contract before anything is touched: `model`,
  `state` (required: string, object, array, or null passed to the engine),
  `questions` (non-empty object; each `type` `noul`, `choice` or `score`,
  optional `instructions` string, object or array, optional `criteria`: an
  object for `choice` and `noul`, an array for `score`), optional `images` and
  `files` (arrays of strings), nothing else. SGLang's `chat_template_kwargs`
  is refused, since llama.cpp does not read it. Shapes only: counts and
  semantics are the engine's.
- **`api_mismatch`.** A chat deployment asked on `/v1/systemone`, or a
  decisions deployment asked on `/v1/chat/completions` (streaming or not), is
  HTTP 400 `api_mismatch` naming the endpoint to use, after the route lookup
  and before waiting, activation or a slot. The router reads the API from the
  deployment's current revision (`LifecyclePort::deployment_api`, store
  `deployment_api`).
- **Forwarding.** The engine forwarder's path allowlist gains exactly
  `/v1/systemone`; `ChatForward::forward_decision` sends one JSON body with
  `model` replaced by the served name and reads the answer whole (at most
  16 MiB, the chat collection bound), returning it with the public name. A
  whole, unambiguous JSON object whose `answers` is a non-empty object of
  objects each naming a `type` is completion; an engine 400, 413 or 422 with
  a complete JSON body is `engine_rejected` (SGLang's 422 validation
  `detail` now gives its message); anything else, a truncated or duplicated
  body included, is uncertain and never replayed. There is no first-token
  event: the whole answer is due by the request deadline, and a cut there
  is cancelling, as for chat.
- **Host ingress.** Exactly `POST /v1/systemone` besides chat completions,
  with its own body check (the decision contract), its fixed upstream path,
  JSON-only answers, and the same keys, gate, 8 MiB bound, guard and timings.
- **Metrics.** Generic timings and lease accounting are unchanged; a decision
  records no time to first token or decode speed and its answer carries no
  `capyctl.metrics`; the engine's `usage` passes through. No new labels.
- **`/v1/models`.** Each entry carries `capyctl.api` (`chat` or
  `decisions`), read from the store without waking anything
  (`LifecyclePort::list_enabled_routes` replaces `list_enabled_route_ids`).

Tests (T43): `capyctl-router/tests/decisions.rs` (admitted, forwarded and
settled like chat through the real forwarder; contract refusals and
`api_mismatch` both ways before activation, lease or engine; engine 400 and
SGLang 422 relayed with the lease completed; truncated, partial, ambiguous,
non-JSON and failed answers leave the lease uncertain without replay; the
request deadline bounds a stalled engine; `/v1/models` names the API);
`capyctl-agent/tests/ingress.rs` (path, contract and model checks, engine
rejection relayed); forward unit tests (contract, answer evidence, path
allowlist, validation detail); store test (the API follows the current
revision). CPU and engine-double tests only; not qualification. The live
check still needed: a decision request through the router and a host ingress
to llama.cpp v0.7.0 and SGLang 0.5.22, and whether SGLang's running and
waiting gauges count decision requests.

# Release note: Requests

- **Decision requests.** A decision model (`api: decisions`) answers
  `POST /v1/systemone` on the inference port, with the same key, waiting and
  wake-on-request as chat: typed questions (`noul`, `choice`, `score`) about a
  `state`, answered with probabilities and the engine's `usage`. Sending a
  model's requests to the endpoint it does not answer is `400 api_mismatch`,
  naming the right one, without waking anything, and `GET /v1/models` shows
  each model's API as `capyctl.api` ([Decision requests](../guide/requests.md#decision-requests)).
