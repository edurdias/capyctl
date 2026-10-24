# ADR 0036 — Embedding models (`api: embeddings`, `POST /v1/embeddings`)

**Status:** Proposed (2026-10-24), for the owner to decide. Target release 0.1.9. Nothing
is implemented; the SPEC, guide and operations texts this ADR lists change with the
implementation, as for ADRs 0030 and 0033. The section "Open questions for the owner"
lists what the owner has to decide, each with options and a recommendation.
**Amends (with the implementation):** `SPEC.md` §6.1 (readiness of a model that answers
with a vector), §10 (the inference surface, request IDs, settlement of a cut request),
§15.1/§16 (a third `api` value), §20 (T47); ADR 0030 §1 (`api` gains `embeddings`); ADR
0029 §6 (`--embedding` and `--pooling` stop being reserved for embeddings deployments);
ADR 0014 §3 (vLLM's `--runner` and `--convert` and SGLang's `--is-embedding` become
reserved); ADR 0031 §2 (a custom engine's manifest `api` gains `embeddings`).
**Related:** ADR 0012 (deep parking and its protections), ADR 0013 (instances and
request leases), ADR 0017 (capability gating), ADR 0024 (parsers apply to chat only),
ADR 0028 (groups), ADR 0032 (settling a cancelled request), ADR 0033 (LMCache), ADR 0034
(LMCache by default), ADR 0035 (one model across several GPUs on one host).

## Context

An embedding model turns a text into a fixed-length vector, for retrieval, search,
clustering and deduplication. Home users run one beside a chat model: a retrieval
pipeline embeds every document once and every question as it arrives, and then asks the
chat model with what it found. Clients speak OpenAI's Embeddings API, `POST
/v1/embeddings`; every engine CapyCTL runs except TensorFold serves it.

CapyCTL does not serve it. SPEC §10: "The first inference surface is `GET /v1/models`
and streaming/non-streaming `POST /v1/chat/completions` ... Do not claim full API
equivalence for embeddings, Responses, native endpoints, or other surfaces without
contracts and tests." ADR 0030 added `POST /v1/systemone` with its own contract; this ADR
gives embeddings theirs. An embedding model also generates no text, so, like a decision
model before ADR 0030, it would launch and never pass CapyCTL's 8-token chat probe
(SPEC §6.1).

### The API

OpenAI's reference ([Create embeddings](https://developers.openai.com/api/reference/resources/embeddings/methods/create),
read 2026-10-24): `model`; `input`, "a string or array of tokens. To embed multiple
inputs in a single request, pass an array of strings or array of token arrays", which
"cannot be an empty string, and any array must be 2048 dimensions or less";
`dimensions`, the number of dimensions of the output, "only supported in
`text-embedding-3` and later models"; `encoding_format`, `float` (the default) or
`base64`; and `user`. The answer is `{object: "list", data: [{object: "embedding",
index, embedding}], model, usage: {prompt_tokens, total_tokens}}`, where `embedding` is
an array of floats or, with `base64`, a base64 string. There is no streaming form.

### What the engines serve

Each engine's public source and documentation at the version on the lab hosts, read on
GitHub on 2026-10-24 (no engine was run and no lab host was used):

| Engine | Version | Launch | `POST /v1/embeddings` |
|---|---|---|---|
| vLLM | [v0.32.0](https://github.com/vllm-project/vllm/tree/v0.32.0) (`5df63a7`) | `--runner pooling`; a generative architecture is converted with `--convert embed` | yes: strings, token arrays, batches, chat `messages`; `float`, `base64`, `bytes`, `bytes_only`; `dimensions` only for Matryoshka models |
| SGLang | [v0.5.22](https://github.com/sgl-project/sglang/tree/v0.5.22) (`baa9bef`) | `--is-embedding` for decoder-style models | yes: strings, token arrays, batches, multimodal objects; `float`, `base64`; `dimensions` checked against the model's Matryoshka sizes |
| llama.cpp | [b11669](https://github.com/ggml-org/llama.cpp/tree/b11669) (`620d222`, the v0.7.0 build) | `--embeddings`, a pooling other than `none` | yes: strings, token arrays, batches, multimodal `content` objects; `float`, `base64`; `dimensions` ignored |
| TensorFold | [v1.1.4](https://github.com/ashhart/TensorFold/blob/v1.1.4/docs/api.md) and v0.6.6 | none | no: the endpoint table (`docs/api.md:64-81`) has no embeddings route, and neither the API page nor the README of v0.6.6 or v1.1.4 mentions embeddings |

Details that shape the decision:

- **vLLM.** The OpenAI-compatible route is `POST /v1/embeddings` (`vllm/entrypoints/pooling/embed/api_router.py:28-43`),
  wrapped in `with_cancellation`, so a dropped connection aborts the request. The
  request takes `input: list[int] | list[list[int]] | str | list[str]`
  (`entrypoints/pooling/base/protocol.py:214`), `encoding_format` from `float`,
  `base64`, `bytes`, `bytes_only` (`:322`; `utils/serial_utils.py:32`), `dimensions`
  (`:347`), and extras CapyCTL would not forward: `truncate_prompt_tokens` (`:42`), its
  own `request_id` (`:62`), `cache_salt` (`:84`), `embed_dtype` and `endianness`, whose
  default is `native` (`:334`). The answer has `id` (`embd-<uuid>`), `object`,
  `created`, `model`, `data` and `usage` (`entrypoints/pooling/embed/protocol.py:174-186`).
  The documentation ([Embedding Usages](https://github.com/vllm-project/vllm/blob/v0.32.0/docs/models/pooling_models/embed.md))
  says vLLM "returns an error for requests that attempt to change the output dimension
  of models that do not support Matryoshka Embeddings" (`is_matryoshka` or
  `matryoshka_dimensions` in `config.json`, or `--hf-overrides`), and that a model not
  in its table is converted "using `as_embedding_model` ... from the normalized hidden
  state corresponding to the last token".
- **SGLang.** `EmbeddingRequest` takes `input` (strings, token arrays, or multimodal
  objects), `model`, `encoding_format` (default `float`), `dimensions`, `user`, and
  extras: `rid`, `priority`, `lora_path`, `embed_override_token_id`, `embed_overrides`
  (`python/sglang/srt/entrypoints/openai/protocol.py:1347-1372`). The handler accepts
  only `float` and `base64`, refuses empty or whitespace-only inputs and negative token
  IDs (`entrypoints/openai/serving_embedding.py:44-83`), packs base64 as little-endian
  float32 explicitly (`:269-279`), and answers `data`, `model` (set to the engine's model
  path, not the served name), `object` and `usage`, with no `id` (`:281-308`; protocol
  `:1411-1415`). The [embedding models page](https://github.com/sgl-project/sglang/blob/v0.5.22/docs/docs/supported-models/embedding_models.mdx)
  says decoder-style models need `--is-embedding`, and that without `is_matryoshka` the
  server checks a requested `dimensions` against `matryoshka_dimensions`. A batched
  request needs one `rid` per input (`managers/io_struct.py:1363-1367`). For a
  non-streamed request whose client has gone, the tokenizer aborts it itself
  (`managers/tokenizer_manager.py:1963-1974`).
- **llama.cpp.** `/v1/embeddings` answers only on a server started with `--embeddings`
  and a pooling other than `none` (`tools/server/server-context.cpp:6122-6132`); it
  takes `input` as a string, a token array, or an array of either, plus multimodal
  `content` objects with `--mmproj` (`:6137-6175`), and `encoding_format` `float` or
  `base64` (`:6148-6157`). Nothing reads `dimensions` anywhere in the server: the full
  vector comes back. The base64 form is the float array's own bytes
  (`server-common.cpp:1498-1508`, native byte order) and each item then carries an extra
  `encoding_format` key; the answer's `model` echoes the request's, and it has no `id`
  (`:1489-1530`). Each input must fit one physical batch: the server sets `n_batch` to
  `n_ubatch` when embeddings are on (`server.cpp:159-166`), and an input longer than
  `n_ubatch` (512 by default) fails with "input (N tokens) is too large to process.
  increase the physical batch size" (`server-context.cpp:3654-3657`). The README
  documents the endpoint and these limits ([`/v1/embeddings`](https://github.com/ggml-org/llama.cpp/blob/b11669/tools/server/README.md#post-v1embeddings-openai-compatible-embeddings-api)).

Every host CapyCTL supports (x86-64 and AArch64 Linux) is little-endian, so vLLM's and
llama.cpp's native byte order is the little-endian float32 that OpenAI's base64 form and
SGLang use. [INFERENCE: from the architectures; no engine was run.]

### What CapyCTL does today

- The router serves chat and decisions with one pipeline: authenticate, check
  `X-Request-Id`, read the body up to `max_buffered_bytes_total`, look the deployment up
  by `model`, refuse a mismatched API with `api_mismatch` before any waiting or
  activation, then wait, activate, admit, select and lease
  (`crates/capyctl-router/src/lib.rs:179-224`, `chat.rs:83-106`, `endpoint.rs:21-87`).
  `GET /v1/models` gives each entry `capyctl.api` (`lib.rs:137-162`).
- A decision is forwarded as one JSON body with `model` replaced, read whole up to
  `DECISION_RESPONSE_LIMIT` (16 MiB), and is completion only when it is a whole JSON
  object with an `answers` object; anything else is uncertain
  (`crates/capyctl-adapters/src/forward.rs:840-874`, `:352`).
- Request IDs (SPEC §10, note of 2026-10-19, PR #159): the caller's `X-Request-Id` is
  checked and echoed and never forwarded; CapyCTL's own `X-CapyCTL-Request-Id` is the
  request's first lease ID (`crates/capyctl-router/src/request_id.rs:23-141`). On an
  SGLang launch with the ADR 0032 capability it becomes `rid` on chat requests only;
  decisions carry none (`forward.rs:858`, `crates/capyctl-agent/src/ingress.rs:668-686`).
- A cut collected request is `cancelling`. It settles on the engine's idle reading only
  if the engine answered it (chat: a parsed progress event, or a complete engine error
  answer); a decision never reports progress, so a cut decision settles at its
  instance's next verified stop, and ADR 0032 §7 restarts an instance when such
  requests alone fill the deployment's 32 places
  (`crates/capyctl-router/src/chat.rs:431-454`, `:629`;
  `crates/capyctl-store/src/request_lease_cancellations.rs:118-230`;
  `crates/capyctl-store/src/ordinary_lifecycle/held_restart.rs`).
- Readiness: chat sends "Say ready." with 8 tokens; decisions send the fixed decision
  of ADR 0030 §3 (`crates/capyctl-adapters/src/decision_probe.rs`, kind `decision`,
  version 1). The wake canary stores a reference per kind and version and compares a
  later answer with a tolerance (`crates/capyctl-store/src/wake_canary.rs:34-201`,
  `crates/capyctl-controller/src/wake_canary.rs:36-122`).
- Admission reserves no generated tokens: it counts requests and buffered bytes, and
  `max_tokens` is a forwarded field, not a credit (`crates/capyctl-router/src/admission.rs:57-149`).
  Memory is the deployment's weights, KV and margin (`crates/capyctl-config/src/effective/engine_config.rs:729-863`).
- The host ingress permits only the chat and decision paths, with a closed validator
  each and an 8 MiB body (`crates/capyctl-agent/src/ingress.rs:445-500`, `:599-637`).
- llama.cpp's `--embedding`, `--embeddings` and `--pooling` are reserved and never
  rendered (ADR 0029 §6; `crates/capyctl-config/src/llamacpp.rs:232-234`). vLLM's
  `--runner` and `--convert` and SGLang's `--is-embedding` are not reserved
  (`crates/capyctl-config/src/engine_policy.rs:60-139`, `:150-237`), so a chat
  deployment's extra arguments can turn its engine into an embedding server that then
  never passes the chat probe.

## Decision

### 1. A deployment declares `api: embeddings`

`api` (ADR 0030 §1) gains a third value, `embeddings`. As for `decisions`, it is part of
the frozen revision and of its identity: the same checkpoint served as chat and as
embeddings (vLLM converts a generative checkpoint with `--convert embed`) is two
revisions, and a revision that omits `api` keeps its fingerprint. An embeddings
deployment gets no reasoning or tool-call parser (ADR 0024 applies to chat).

Resolution refuses `api: embeddings` with `capability_missing:embeddings` on an
installation that cannot serve it, read from the registered build:

- **vLLM 0.32.0 or newer:** served, once row EM1 passes on 0.32.0 (open question 1).
- **SGLang 0.5.22 or newer:** served, once row EM1 passes.
- **llama.cpp v0.7.0 (b11669) or newer:** served, once row EM1 passes.
- **TensorFold, every version:** refused. Neither 0.6.6 nor 1.1.4 documents an
  embeddings route; TensorFold 1.0 is not yet an engine CapyCTL runs either.
- **Custom engines (ADR 0031):** a manifest's `api` gains `embeddings`, and a
  deployment's `api` must equal it, as today (open question 8).

A host advertises the capability `embeddings` (ADR 0017). A server never places an
embeddings deployment on a host without it (`host_capability_missing:embeddings`), so
an older host never runs a chat probe against an embedding server.

### 2. Rendering

- **vLLM:** `--runner pooling`. vLLM picks the conversion itself (`--convert auto`),
  converting a generative architecture to an embedding model. `--runner` and
  `--convert` become reserved for every vLLM deployment, so a chat deployment cannot
  launch a pooling server through its extra arguments. `--pooler-config` and
  `--hf-overrides` (for example `{"is_matryoshka": true}`) stay ordinary.
- **SGLang:** `--is-embedding`, reserved for every SGLang deployment and rendered only
  for embeddings. `--json-model-override-args` stays ordinary.
- **llama.cpp:** `--embeddings`. `--pooling` becomes ordinary on an embeddings
  deployment only, except `none`, which is refused because the OpenAI route refuses it;
  on a chat deployment it stays reserved, as do `--embedding`, `--embeddings` and
  `--rerank` everywhere. CapyCTL renders `--ubatch-size` and `--batch-size` equal to the
  per-slot context, so any input that fits the context fits one physical batch; an
  explicit `--ubatch-size` or `--batch-size` in the arguments wins. [INFERENCE: the
  compute buffer grows with the physical batch; row EM1 measures the peak.]
- **LMCache** (ADRs 0033 and 0034) is off for an embeddings deployment, with the reason
  `embeddings_unsupported`: an embedding request reuses no generated prefix across turns,
  and no pooling runner has been qualified with the connector. `lmcache: required` beside
  `api: embeddings` is refused `lmcache_unavailable:embeddings_unsupported`.

### 3. The inference surface gains `POST /v1/embeddings`

The router serves `POST /v1/embeddings` beside chat completions and decisions, with the
same API key, body bound, lookup by `model`, `X-Request-Id` handling (SPEC §10, note of
2026-10-19), waiting, activation on request, admission, instance selection, failover
before acceptance (ADR 0013) and request lease. Only an `api: embeddings` deployment
answers it. A chat or decisions deployment refuses it, and an embeddings deployment
refuses the other two endpoints, each with HTTP 400 `api_mismatch` naming the endpoint to
use, before any activation. `GET /v1/models` keeps its shape; `capyctl.api` is
`embeddings`.

The request has its own closed contract, OpenAI's:

- `model`: a non-empty string.
- `input`, required: a non-empty string; an array of 1 to 2048 non-empty strings; a
  non-empty array of token IDs; or an array of 1 to 2048 non-empty arrays of token IDs.
  A token ID is an integer from 0 to 2³¹−1. Mixed arrays are refused. Whether a token
  ID is in the vocabulary, and whether an input fits the model's length, is the
  engine's to judge.
- `encoding_format`: `float` (the default) or `base64`. vLLM's `bytes` and `bytes_only`
  are refused.
- `dimensions`: a positive integer. vLLM and SGLang judge it against the model's
  Matryoshka sizes and refuse it otherwise. llama.cpp ignores it, so on llama.cpp the
  router refuses a `dimensions` other than the deployment's native dimension (recorded
  at readiness, §5) with `400 invalid_request` saying llama.cpp returns full vectors
  (open question 4).
- `user`: a string, forwarded as chat forwards it.

Any other field is refused with `400 invalid_request` naming it: `stream`, `cache_salt`
(open question 10), vLLM's `messages`, `truncate_prompt_tokens`, `request_id`,
`embed_dtype`, `endianness`, `use_activation`, SGLang's `rid`, `priority`, `lora_path`,
`embed_overrides`, llama.cpp's `content` and `embd_normalize`, and multimodal input
objects (open question 5). The router does not tokenize, truncate, normalize or batch
(SPEC §10).

The request goes to the engine's `/v1/embeddings` as one JSON body with `model` replaced
by the served name, and the answer comes back whole with `model` replaced by the public
name; nothing else in it is rewritten (vLLM's `id` and `created` stay, SGLang's and
llama.cpp's absence of them stays).

### 4. Evidence, answer size and settlement

**Completion.** An answer is completion when it is a whole JSON object, read within the
answer limit below, with `object: "list"` and a `data` array holding exactly one entry
per input, whose `index` values are 0 to n−1 each once, whose `embedding` values are, for
`float`, arrays of finite numbers all of one length, or, for `base64`, strings all
decoding to the same whole number of 4-byte values, and, when `dimensions` was
asked, of that length. `usage.prompt_tokens`, when present, is a non-negative integer.
An engine 400, 413 or 422 with a complete JSON body is a rejection, relayed as
`engine_rejected` (SPEC §10): an input longer than the model's limit, a `dimensions` the
model does not support. Anything else, including a whole answer that fails these checks,
leaves the request uncertain, as for decisions: the engine may have run it, so it is not
replayed.

**Answer size.** A batch of 2048 inputs of 4096 floats is up to about 200 MB as JSON
text, so the decision limit of 16 MiB is not enough and nothing larger may be read
unbounded. The answer is read whole up to a fixed `EMBEDDING_RESPONSE_LIMIT` of 64 MiB
(open question 6). To keep an engine from computing an answer the router will then
refuse to read, the router refuses before forwarding, with `413 request_too_large`, a
request whose answer could exceed the limit: number of inputs × dimension × the worst
case per value (24 bytes for `float`, 6 for `base64`), plus 256 bytes per input. The
dimension is `dimensions` when given, else the native dimension recorded at readiness.
The message names the limit and suggests `base64` or fewer inputs. With 1024-dimension
vectors every batch the contract allows fits (2048 inputs in `float` are at most about
51 MB); with 2560 dimensions a `float` batch is refused above 1,087 inputs, and with 4096
above 680, while 2048 inputs fit in `base64` at either size. [INFERENCE: arithmetic,
64 MiB ÷ (dimension × 24 + 256).]

**Request IDs.** The caller's `X-Request-Id` is checked, echoed on the answer or error,
and never forwarded, exactly as for chat and decisions (SPEC §10, note of 2026-10-19).
CapyCTL's own request ID is not sent to the engine in the first version, on any engine:
SGLang needs one `rid` per input of a batch, and the settlement below does not use one
(open question 7). The ingress consumes CapyCTL's request ID header and does not set `rid` on
embeddings.

**Cancellation and settlement.** There is no stream: a client that hangs up, or a
request cut by its deadline, drops the engine request, which closes the engine
connection; vLLM (`with_cancellation`), SGLang (its tokenizer's disconnect check) and
llama.cpp (`should_stop`) then abort it. The lease is `cancelling`. An embedding answer
is all or nothing, so a cut embedding request was never answered: it settles only at its
instance's next verified stop (SPEC §10, amendment of 2026-10-21, owner decision 6),
and when such requests alone fill the deployment's 32 places CapyCTL restarts the
instance and journals `instance_restarted_for_held_requests` (ADR 0032 §7). A complete
engine error answer is answered, as today, and settles on the engine's idle reading.
Park, idle park and switch drains treat embedding requests as they treat decisions.

### 5. Readiness is a fixed embedding

Where a chat deployment generates text, an embeddings deployment sends one fixed request,
version `1` of the probe of kind `embedding` (a new
`crates/capyctl-adapters/src/embedding_probe.rs`), to the launch's own loopback endpoint
with its own key, never through ingress or the router:

```json
{
  "model": "<the engine's served model name>",
  "input": "CapyCTL readiness check: a quiet library with tall shelves of old books.",
  "encoding_format": "float"
}
```

It is ready evidence when the answer is completion by §4 for one input: one `data` entry
with `index` 0 and an `embedding` of at least one finite number whose Euclidean norm is
finite and above 1e-6. The vector does not have to be normalized (vLLM and llama.cpp
normalize by default, a pooler setting can turn that off). The answer's length is
recorded as the instance's native dimension, used by §3 and §4. This holds at start,
after a restart, and when a host or a restarted standalone role re-proves a retained
launch; `/health` and `/v1/models` alone are never readiness (SPEC §6.1), and an
embeddings deployment is never sent a chat probe in place of it. The kind and version are
recorded with the reference (§6); any change to the request is a new version, and a
reference recorded by another version is never compared with it.

### 6. The wake canary compares vectors with a tolerance

The first ready answer is recorded as the reference: the dimension and the vector, stored
as little-endian float32 (4 KiB for 1024 dimensions) beside the kind, version and
tolerance (a new stored form beside `StoredDecisionCanary`). A later check passes when the
dimension is the same and the relative distance `‖v − r‖ / ‖r‖` is at most the tolerance;
otherwise it fails `wake_mismatch` and the instance is stopped, as today. For unit
vectors a relative distance `d` is a cosine similarity of `1 − d²/2`.

The tolerance is set from row EM3's measurements plus a stated margin, and no number is
committed before that (open question 3). The proposal to measure against is 0.02 (cosine
0.9998 for unit vectors): the probe goes to an engine with no other traffic at start and
after a restart, BF16 vectors of one input move slightly with batching and kernel choice
[INFERENCE, not measured], and a model loaded wrong gives an unrelated vector, far beyond
that.

With §7.2, an embeddings deployment does not park, so in the first version the check runs
after a restart of a deployment that has a reference, and is the basis for parking later.

### 7. Scope of the first version

1. **One GPU on one machine.** An embeddings deployment with a `topology` is refused, and
   it is not combined with ADR 0035's several GPUs on one host: the embedding models in
   this release fit one GB10 or one discrete card, and group readiness, stall detection
   and the group canary would each need an embedding form.
2. **Restart instead of park.** An embeddings deployment resolves `restart_only`, and
   `deep` or `host_backed` is refused. The models start in seconds, llama.cpp cannot
   park, and neither vLLM's sleep mode nor SGLang's memory release has been read or run
   with a pooling server (open question 2).
3. **Text only.** Multimodal inputs (vLLM's `messages`, SGLang's and llama.cpp's content
   objects) are refused (open question 5).

### 8. Accounting

An embedding request generates nothing: no output tokens, no decode. Admission already
reserves no generated tokens, so nothing changes there. Memory is sized as for the same
checkpoint today (weights, KV and margin): the engine holds KV only while it reads each
input, so the KV an embeddings deployment needs is about its concurrent inputs times
their length, small beside a chat model's. For Qwen3-Embedding-0.6B (28 layers, 8 KV
heads of 128, BF16) that is 112 KiB a token, 3.5 GiB for one full 32,768-token input.
CapyCTL keeps today's sizing rules and the recipes declare small requests (open question
11). [INFERENCE: encoder-only checkpoints may need no KV at all on vLLM; row EM1 records
the peak.]

Waiting accounts a request's body bytes as for chat; an embeddings deployment's requests
count against the same per-deployment bound of 32.

### 9. Metrics

The router records embedding requests in its latency series
(`crates/capyctl-router/src/timing.rs:47-58`, published as JSON at
`GET /management/v1/metrics/latency`) as it records decisions: queue wait, activation
wait, selection, lease grant, pre-forward, first byte and total; not time to first
content or to the last chunk, which have no meaning without a stream. The answer carries
`capyctl.metrics` with the figures that describe an embedding, each labelled with its
source: `total_ms` and `time_to_response_ms` (router), `input_tokens` (engine: the
answer's `usage.prompt_tokens`) and `inputs` (router: the number of inputs). A figure
nobody measured is absent, never zero; time to first token, decode speed, `queue_ms`,
`prefill_ms` and `cached_tokens` are not reported for embeddings, since no engine puts
them in its embedding answer. Whether each engine's running and waiting gauges count
embedding requests (vLLM's `vllm:num_requests_running`, SGLang's `sglang:num_running_reqs`,
llama.cpp's `requests_processing`) is qualified per engine (row EM5) before parking,
switching or idle settlement relies on them. [INFERENCE: each engine schedules embedding
requests through the same scheduler or slots as generation, so they should be counted.]

### 10. Host ingress

The ingress permits `POST /v1/embeddings` besides chat and decisions, with its own
closed validator (the contract of §3, the `model` equal to the bound served name) and the
same 8 MiB body limit. It relays the answer unchanged; the router bounds it (§4).

## Consequences

- Clients that embed documents and questions can use the same CapyCTL endpoint and API
  key as their chat model, and an embedding model shares a GPU with a chat model through
  the ordinary switching of SPEC §10.
- Every place that proves readiness gains an embedding branch (adapters, agent,
  controller, the stored canary); the fake engine gains an embeddings mode, so CPU tests
  cover routing, accounting, readiness and the canary. CPU and Fake-engine tests are not
  qualification.
- A cut embedding request settles only at the next verified stop. A client that times out
  often can fill the deployment's 32 places with held requests, and ADR 0032 §7 then
  restarts the instance; for a model that starts in seconds that is short, but it is a
  restart (open question 7).
- A large `float` batch is refused before it runs (§4); clients that embed thousands of
  documents at once batch in smaller requests or use `base64`, which the OpenAI Python
  client already sends when the caller names no format, and decodes as float32
  ([`lib/_parsing/_embeddings.py`](https://github.com/openai/openai-python/blob/main/src/openai/lib/_parsing/_embeddings.py),
  lines 34-38 and 55-56, read 2026-10-24). Most clients therefore exercise the `base64`
  path first.
- Recipes gain an embedding form: latency at p50 and p95 by batch size and input length,
  inputs per second as users increase, peak memory, start time, and a retrieval score on
  a pinned public set, without tokens per second. `capyctl-bench` (in the recipes
  repository) gains an embeddings mode.
- vLLM's `--runner` and `--convert` and SGLang's `--is-embedding` become reserved for
  chat deployments too; a profile or deployment that passes them today is refused with
  the existing reserved-option reason after the upgrade.

## Alternatives considered

- **Forward `/v1/embeddings` without a deployment field.** Rejected for the reason ADR
  0030 gives: readiness and the canary must know the API before any request, and the
  engine must be launched as an embedding server.
- **Detect embeddings from the checkpoint** (a `sentence_transformers` config,
  `pooling` metadata in GGUF). A generative checkpoint can be served as either, and the
  operator chooses; detection may later fill in a default.
- **Pass the engine's own extras through** (vLLM's truncation and dtype controls,
  llama.cpp's normalization). Rejected for the first version: each engine's extras
  differ, and an open contract is what SPEC §10 warns against.
- **Readiness on the vector's length only.** Too weak: a model that answers NaN or zero
  vectors has a length. The probe also checks finiteness and a non-zero norm.
- **Exact canary equality.** Rejected: floating-point vectors are not bit-stable across
  batches and kernels.
- **Cosine similarity as the canary measure.** Equivalent for normalized vectors, but it
  ignores a change of scale on a deployment that turned normalization off; the relative
  distance catches both.
- **Reading an answer of any size.** Rejected: the router holds the whole answer in
  memory; a limit with a refusal before forwarding keeps the engine from running work
  nobody will receive.
- **Settling a cut embedding on the engine's idle reading.** Unsound under owner
  decision 6 of 2026-10-21: the request was never answered, so the engine may still run
  it.

## Implementation plan (target 0.1.9)

1. **Config.** `DeploymentApi::Embeddings` (`crates/capyctl-config/src/effective.rs`), a
   new `embeddings.rs` beside `decisions.rs` with the engine floors, the topology and
   residency refusals and `restart_only`; vLLM `--runner`/`--convert` and SGLang
   `--is-embedding` reserved (`engine_policy.rs`); llama.cpp `--pooling` ordinary on
   embeddings deployments, `none` refused (`llamacpp.rs`); LMCache off with
   `embeddings_unsupported` (`lmcache.rs`); the custom manifest's `api` gains
   `embeddings` (`custom.rs`).
2. **Protocol.** The host capability `embeddings`, the embedding probe requirement and
   `host_capability_missing:embeddings` (`crates/capyctl-protocol/src/capabilities.rs`).
3. **Adapters.** Rendering per §2 (vLLM `args.rs`, SGLang and llama.cpp launch); in
   `forward.rs`, `EMBEDDING_REQUEST_FIELDS`, `validate_embedding_request`, `embed()`,
   `embedding_answered` and `EMBEDDING_RESPONSE_LIMIT`; `embedding_probe.rs`; each
   adapter's initialize chooses the probe by API.
4. **Router.** A third `Endpoint` (`endpoint.rs`), the route (`lib.rs`), the collected
   path shared with decisions (`chat.rs`), the pre-forward answer-size refusal, the
   llama.cpp `dimensions` refusal, and `capyctl.metrics` (`engine_metrics.rs`).
5. **Agent and controller.** The ingress path and validator (`ingress.rs`); the retained
   re-proof by API (`native_execution.rs`, `local_readiness.rs`); the native dimension
   recorded with the launch's readiness.
6. **Store.** `StoredEmbeddingCanary` (dimension, vector, tolerance, kind, version) and
   its comparison (`crates/capyctl-store/src/wake_canary.rs`,
   `crates/capyctl-controller/src/wake_canary.rs`), with a migration.
7. **Fake engine.** An embeddings mode in `crates/capyctl-testkit/src/fake_engine.rs`
   returning set vectors, refusing chat and decisions.
8. **Docs.** The SPEC texts below; `docs/guide/requests.md` ("Embedding requests"),
   `docs/guide/deploy.md` ("Embedding models"), `docs/guide/engines.md` (each engine's
   flags and limits), `docs/guide/errors.md` (`api_mismatch`, `invalid_request` and
   `request_too_large` on `/v1/embeddings`), `docs/operations/configuration.md` (`api`,
   the manifest value), `docs/operations/network-access.md` (the ingress paths); one
   change file with the release note.

SPEC texts to land with the implementation: §6.1 (an embeddings deployment is ready on
the fixed embedding of ADR 0036 §5, its canary compares vectors within a tolerance,
restart only); §10 (the inference surface includes non-streaming `POST /v1/embeddings`
for `api: embeddings`, its closed contract, the answer limit and the refusal before
forwarding, settlement of a cut request at the next verified stop); §15.1 (`api`:
`chat`, `decisions` or `embeddings`); §20 (T47).

Tests, tagged **T47 — embedding models**: `api: embeddings` resolves and round-trips
through the frozen revision, and is refused on an engine without the capability
(`capability_missing:embeddings`, TensorFold and versions below the floors), with a
`topology`, with a parking residency, with a stated parser and with `lmcache:
required`; the reserved vLLM and SGLang options are refused on every deployment and
`--pooling none` on llama.cpp; each engine renders its option and llama.cpp its batch
sizes; `POST /v1/embeddings` accepts each input shape and refuses every other field,
empty inputs, mixed arrays, more than 2048 inputs, negative or oversized token IDs,
`bytes` and `stream`; it is routed, admitted, leased and settled like decisions; the
mismatched endpoints are refused before activation; `model` is rewritten both ways and
nothing else; an answer with the wrong count, a repeated index, ragged vectors, a
non-finite value or the wrong `dimensions` is uncertain, an engine 400 is
`engine_rejected`; the answer-size refusal fires at the limit and not one input below,
for `float` and `base64`; llama.cpp refuses a non-native `dimensions`; a cut request
settles at the verified stop and a full deployment of them restarts; the deployment is
ready only on a well-formed fixed embedding and never on a chat probe; the canary passes
within the tolerance and fails on a changed dimension or a vector beyond it; a reference
from another kind or version is not compared; an old host never receives an embeddings
deployment (`host_capability_missing:embeddings`); `capyctl.metrics` holds only measured
figures. Tests also tag T17 (settlement) and T43 (`api_mismatch`) where they apply. CPU
and Fake-engine tests are not qualification.

## Live qualification

Run in a live session the owner schedules (one live session on the hosts at a time), on
host A unless stated, in the approved environments as they are (`~/vllm-0.32.0-venv`,
SGLang 0.5.22 and the llama.cpp v0.7.0 build; no new environment is needed), with
[Qwen3-Embedding-0.6B](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) (BF16, revision
`97b0c61`; Matryoshka from 32 to 1024 dimensions, per its model card) on vLLM and SGLang,
and its [Q8_0 GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF) on llama.cpp:

| Row | What it shows |
|---|---|
| EM1 | Start per engine: the deployment starts with its rendered option, becomes ready on the fixed embedding (native dimension 1024 recorded), shows `api: embeddings` in status and `capyctl.api` in `/v1/models`, and LMCache off with `embeddings_unsupported`; peak memory, start time and the charge recorded, llama.cpp with the rendered batch sizes. |
| EM2 | Contract per engine: a string, an array of strings, a token array and an array of token arrays each answer one vector per input in order; `base64` decodes to the `float` answer's values (within float32 rounding); `dimensions: 256` answers 256-length vectors on vLLM and SGLang (with the Matryoshka override in the arguments if the checkpoint needs it) and is refused on llama.cpp; `stream`, an unknown field and an empty input are refused by CapyCTL; an input over the model's length is the engine's `engine_rejected`; `model` in the answer is the public name. |
| EM3 | Tolerance: the probe text sent 20 times alone after start, 20 times after a restart, and 20 times inside 64-input batches under 8 concurrent clients; the largest relative distance and smallest cosine from the reference, per engine, set the canary tolerance with its margin. Cross-engine agreement on 100 pinned sentences (vLLM against SGLang, BF16; llama.cpp Q8_0 against both) is recorded, not required. |
| EM4 | Restart canary: stop and start each deployment; the check compares with the reference and passes, and the event names the verdict. |
| EM5 | Cut requests and gauges: during a 2048-input batch each engine's running gauge reads the request; a client hang-up mid-batch leaves the lease `cancelling` until the next verified stop; 32 cut batches fill the deployment and CapyCTL restarts the instance, journals the restart and settles all 32; the next request is served. |
| EM6 | Host path: host A as a host of a server on the control host; a request through its ingress is validated, relayed and answered, `X-Request-Id` echoed and never seen by the engine. |
| EM7 | Switching: on one GPU, an embeddings deployment and a chat deployment that do not fit together; a request for each in turn switches the pool per SPEC §10 and both answer. |
| EM8 | Size and load: `capyctl-bench`'s embeddings mode for p50 and p95 by batch size (1, 16, 256) and input length (32, 512, 8,192 tokens), inputs per second at 1, 4 and 16 clients, and a retrieval score on a pinned public set; then, on vLLM with [Qwen3-Embedding-4B](https://huggingface.co/Qwen/Qwen3-Embedding-4B) (2560 dimensions), a 2048-input `float` batch is refused `request_too_large` before the engine sees it, and the same batch in `base64` answers. |

## Open questions for the owner

1. **Engine versions.** vLLM 0.30.0 and 0.31.0 and SGLang 0.5.20 and 0.5.21 are also on
   the hosts and also serve embeddings. Options: (a) only the versions EM1 qualifies,
   vLLM 0.32.0, SGLang 0.5.22 and llama.cpp v0.7.0, as floors; (b) every installed
   version, unqualified ones shown as such. Recommendation: (a), as ADR 0030 did for
   decisions; an older version can be added with its own EM1 row.
2. **Parking.** Options: (a) `restart_only` in the first version (§7.2); (b) deep parking
   on vLLM and SGLang now, qualified by a park-and-wake row with the canary. Recommend
   (a): the models start in seconds and neither engine's sleep path has been read with a
   pooling server. An EM row can measure vLLM's sleep levels with `--runner pooling` for a
   later decision, as ADR 0030's amendment did for decisions.
3. **Canary tolerance.** Options: (a) set from EM3's largest distance plus a stated
   margin, nothing committed before; (b) fix 0.02 relative distance now; (c) cosine
   similarity at least 0.999. Recommendation: (a), with 0.02 as the figure EM3 is checked
   against, the way ADR 0033 set LMCache's allowances from LC1.
4. **`dimensions` on llama.cpp,** which ignores it. Options: (a) refuse any value other
   than the native dimension (§3); (b) refuse it always; (c) forward it and let the
   client receive full vectors. Recommendation: (a): a client that asks for the native
   size gets what it asked, and no client silently gets a different length than it asked
   for.
5. **Inputs beyond OpenAI's.** vLLM takes chat `messages` and images; SGLang and
   llama.cpp take multimodal objects; each has extras (truncation, normalization,
   dtypes). Options: (a) OpenAI's text contract only in 0.1.9; (b) add the multimodal
   object shape common to SGLang and llama.cpp; (c) per-engine extras. Recommendation:
   (a); multimodal embeddings can follow with their own contract, body limits and a
   multimodal model row.
6. **Answer limit.** Options: (a) a fixed 64 MiB with the refusal before forwarding
   (§4); (b) the decision limit of 16 MiB; (c) a role setting. Recommendation: (a): 16
   MiB refuses ordinary `float` batches of about 680 1024-dimension vectors, and a
   setting can follow if users need more.
7. **Cut embeddings on SGLang.** ADR 0032's per-request settlement could extend to
   embeddings: CapyCTL sends `rid` as `<request ID>_<i>` per input, aborts by its prefix
   and reads the response. But on 0.5.22 the abort echo carries no `embedding`
   (`managers/tokenizer_manager.py:3624-3640`), so the handler, which reads
   `ret_item["embedding"]` (`serving_embedding.py:292`), fails rather than answering a
   final output [INFERENCE: probably an HTTP 500 from its error path, not run]. Options:
   (a) today's rule (next verified stop, restart when full) for every engine; (b) extend
   ADR 0032 to SGLang embeddings, settling on that error answer after the abort; (c) let
   a cut embedding run to its end and settle on its answer, which an embedding request
   reaches in milliseconds to seconds. Recommendation: (a) for 0.1.9, with EM5 measuring
   how often a restart happens; (c) is worth a decision after EM5, since an embedding
   request is short and bounded, unlike a generation, and it would settle on the
   completion evidence SPEC §10 already accepts.
8. **Custom engines.** Options: (a) the manifest's `api` gains `embeddings` and the
   probe and canary apply as for native engines; (b) custom engines serve chat and
   decisions only. Recommendation: (a): the contract is the endpoint, which ADR 0031
   already makes the manifest's to declare.
9. **Chat checkpoints served as embeddings.** vLLM converts a generative checkpoint with
   `--convert embed` (last-token pooling). Options: (a) allowed, as two revisions of the
   checkpoint (§1); (b) only checkpoints published as embedding models. Recommendation:
   (a); the operator chooses, and recipes list only trained embedding models.
10. **`cache_salt`.** vLLM's embedding request takes it; SGLang's does not. Options: (a)
    refused on embeddings in 0.1.9, and the guide says an embeddings deployment's
    engine prefix cache, where it keeps one, is shared by all its callers, so tenants
    who must not share one get their own deployment; (b) accepted on vLLM as the
    effective digest of SPEC §10 and refused on the others. Recommendation: (a), until a
    row shows whether a pooling server reuses prefixes at all.
11. **Memory defaults.** Options: (a) today's sizing rules, with recipes declaring small
    requests; (b) a default for embeddings deployments from the checkpoint's weights and
    `max_concurrent_requests` full-length inputs of KV. Recommendation: (a) for 0.1.9,
    revisited with EM1's peaks.
