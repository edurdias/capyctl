# CapyCTL 0.1.5

CapyCTL 0.1.5 adds decision models. A deployment with `api: decisions` runs a decision
model on llama.cpp v0.7.0 or SGLang 0.5.22, becomes ready by answering a fixed decision,
and answers `POST /v1/systemone` with probabilities; CapyCTL's recipes repository has
measured recipes and a comparison of six decision models. SGLang 0.5.22 now launches, an
engine's error reaches the client as `502 engine_error` instead of locking the model,
CapyCTL adds less time to each request, and a large vLLM model that parks no longer gets
an impossible startup estimate before its first start.

## Decision models

- **Deployments declare their API.** A deployment takes `api: decisions` to
  serve a decision model, or `api: chat` (the default). A decisions
  deployment runs on llama.cpp 0.7.0 or newer or SGLang 0.5.22 or newer
  (anything else is refused `capability_missing:decisions`), on one machine,
  without parking and without a tool-call or reasoning parser; a machine
  whose CapyCTL is too old is never chosen for it
  (`host_capability_missing:decisions`). See
  [Decision models](../guide/deploy.md#decision-models).
- **Decision requests.** A decision model (`api: decisions`) answers
  `POST /v1/systemone` on the inference port, with the same key, waiting and
  wake-on-request as chat: typed questions (`noul`, `choice`, `score`) about a
  `state`, answered with probabilities and the engine's `usage`. Sending a
  model's requests to the endpoint it does not answer is `400 api_mismatch`,
  naming the right one, without waking anything, and `GET /v1/models` shows
  each model's API as `capyctl.api` ([Decision requests](../guide/requests.md#decision-requests)).
- **Decision models become ready on a decision.** A deployment with
  `api: decisions` is proven ready by one fixed question it must answer with
  a complete set of probabilities, at start and whenever CapyCTL re-checks
  it; it is never sent a chat request. See
  [Decision models](../guide/deploy.md#decision-models).
- **Measured recipes.** CapyCTL's recipes repository has decision-model recipes for
  Clef-Flash, Clef, Kev-4B and Julia-1 on llama.cpp v0.7.0 and Clef-Flash and Clef on
  SGLang 0.5.22, and a comparison of six decision models on one GB10: speed, throughput
  and quality on five public test sets. On llama.cpp, set `--batch-size` and
  `--ubatch-size` to the context; the [guide](../guide/deploy.md#decision-models) shows
  how.

## Engines

- **SGLang 0.5.22 launches.** SGLang 0.5.22 installs plugins of its own
  (from `smg-grpc-servicer`, which it requires), and every launch failed
  `plugin_closure_failed`. CapyCTL now starts SGLang with
  `SGLANG_PLUGINS=capyctl-loads-no-plugins`, a selection that names no plugin,
  so SGLang loads none, and accepts those installed plugins under it. A
  plugin from any other package is still refused. `SGLANG_PLUGINS`,
  `SGLANG_PLATFORM` and `SMG_*` are now names CapyCTL owns
  (`engine_env_reserved:<name>`).

## Requests

- **Lower router overhead per request.** The router no longer reads a
  deployment's instances twice per request, reuses its prepared store
  statements, and turns Nagle's algorithm off on its listeners. On a local
  reproduction this removed about a quarter of what the router added to a
  decision request; the request ledger's guarantees are unchanged.
- **Less time added per request.** A completed answer now reaches the client
  before CapyCTL records the request as finished, and on a standalone machine
  the request ledger no longer waits for the disk on every request. On a local
  reproduction this cut what CapyCTL adds to a decision request from about
  4.5 ms to about 2 ms. A power loss on a standalone machine can lose the last
  request records, but it stops the engines they describe as well. A client
  that sends its next request at once may briefly find the previous one still
  counted against the deployment's limit.

## Fixes

- **An engine's error no longer locks a model.** When the engine answers a
  request with an error (llama.cpp refusing an input longer than its batch,
  for example), the client now gets `502 engine_error` with the engine's own
  message instead of `500 backend completion unverified`, and the request
  stops counting against the model's 32 running requests once the engine is
  idle. Before, a few dozen such errors left the model answering every
  request with `429 queue_full` until it was deleted. See
  [Errors](../guide/errors.md).
- **A large vLLM model that parks no longer estimates an impossible startup peak.** Before a
  first start is measured, CapyCTL estimated a parking vLLM model's startup peak at 3.6 times
  its weights; Nemotron 3.5 Super VL showed 265 GiB. The estimate now adds at most 32 GiB to
  the weights (112 GiB for Super VL), which still covers every peak measured on a GB10. Models
  under about 12 GiB keep the same estimate. A deployment revision accepted before this
  release keeps its old estimate until its first start is measured or a new revision replaces it.

## Known issues

- **Decision models run restart only, on one machine.** A deployment with
  `api: decisions` cannot park and cannot span machines.
- **llama.cpp can starve slots under several concurrent decision requests.** With several
  decision requests at once, some llama.cpp slots can wait without being served; this
  reproduces on llama.cpp without CapyCTL.
- **vLLM has no decision support yet.** A decisions deployment on vLLM is refused
  `capability_missing:decisions`.
- **A standalone request ledger trades durability for latency.** On a standalone machine
  the ledger commits with SQLite's `NORMAL` durability, as decided: a power loss can lose
  the last request records, which describe engines the power loss also stopped.
