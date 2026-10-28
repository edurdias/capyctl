# ADR 0036 — Embedding models (`api: embeddings`, `POST /v1/embeddings`)

**Status:** Accepted (2026-10-27). Proposed 2026-10-24; the owner answered every open
question on 2026-10-27 and named the models to qualify, and the answers are recorded below
("Owner decisions of 2026-10-27"). Four answers differ from the recommendation: an older
vLLM, SGLang or llama.cpp version runs embeddings as an uncertified custom setup instead of
being refused (§1); vLLM and SGLang deployments park `deep` in 0.2.0 (§7.2); the answer is
streamed through to the client, validated as it passes, with no size limit (§4); and a
cut request runs to its end and settles on its answer (§4). Target release 0.2.0
(retargeted from 0.1.9 on 2026-10-27, after 0.1.9 shipped with ADR 0035 only). Nothing is
implemented; the SPEC, guide and operations texts this ADR lists change with the
implementation, as for ADRs 0030 and 0033.
**Amends (with the implementation):** `SPEC.md` §6.1 (readiness of a model that answers
with a vector, and its canary after every wake and restart), §6.2 (an embeddings
deployment parks `deep` on vLLM and SGLang and restarts on llama.cpp), §9.1 and §9.2 (deep
parking of a pooling server), §10 (the inference surface, an answer streamed through and
validated as it passes, request IDs, settlement of a cut request on its answer),
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

Resolution refuses `api: embeddings` with `capability_missing:embeddings` only on an
installation that has no embeddings route to forward to, read from the registered build.
Owner decision 2026-10-27, question 1, with the owner rule of that day: the combinations
qualified live are advertised as certified, and any other version of an engine that serves
embeddings runs as an uncertified custom setup; it is never refused for being uncertified.

- **vLLM:** every version, rendered with `--runner pooling` (§2). 0.32.0 is certified once
  the EM rows pass on it; 0.29.0 to 0.31.0 (0.30.0 and 0.31.0 are also on the hosts and
  serve embeddings) and builds CapyCTL does not know run uncertified.
- **SGLang:** every version, rendered with `--is-embedding`. 0.5.22 is certified once the
  EM rows pass on it; 0.5.20, 0.5.21 and unknown builds run uncertified.
- **llama.cpp:** every build, rendered with `--embeddings`. v0.7.0 (b11669) is certified
  once the EM rows pass on it; every other build runs uncertified (`engine list` already
  shows every llama.cpp build as custom).
- **TensorFold, every version:** refused. Neither 0.6.6 nor 1.1.4 documents an embeddings
  route, so there is nothing to forward to and no option to render; this is a missing
  capability, not a refusal for being uncertified. TensorFold 1.0 is not yet an engine
  CapyCTL runs either.
- **Custom engines (ADR 0031):** a manifest's `api` gains `embeddings`, and a
  deployment's `api` must equal it, as today (owner decision 2026-10-27, question 8). The
  probe of §5 and the canary of §6 apply as for native engines. A custom engine is never
  certified; it runs as an uncertified custom setup.

A build whose engine lacks the rendered option fails its start as any failed start does;
CapyCTL does not refuse it in advance. [INFERENCE: no version below the certified ones
was read or run for this ADR.]

Certification is per engine version and checkpoint revision. The ten models of the live
qualification, each at its pinned revision and on each engine that serves it, are
certified once their rows pass; any other checkpoint runs uncertified, including a chat
checkpoint served as embeddings, which is allowed as two revisions of the checkpoint
(owner decision 2026-10-27, question 9). The combinations the engines' source shows
unsupported are documented (Live qualification) and not refused: the engine fails to load
the checkpoint and the start fails. Status shows an embeddings deployment `certified` or
`uncertified`. The safety refusals of this ADR (the reserved options of §2, the closed
request contract of §3, the residency and topology rules of §7) apply to every version and
checkpoint alike.

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
  (owner decision 2026-10-27, question 4).
- `user`: a string, forwarded as chat forwards it.

Any other field is refused with `400 invalid_request` naming it: `stream`, `cache_salt`
(owner decision 2026-10-27, question 10), vLLM's `messages`, `truncate_prompt_tokens`,
`request_id`, `embed_dtype`, `endianness`, `use_activation`, SGLang's `rid`, `priority`,
`lora_path`, `embed_overrides`, llama.cpp's `content` and `embd_normalize`, and multimodal
input objects (owner decision 2026-10-27, question 5). The router does not tokenize,
truncate, normalize or batch (SPEC §10).

The request goes to the engine's `/v1/embeddings` as one JSON body with `model` replaced
by the served name. The answer is streamed back to the client as it arrives, with `model`
replaced by the public name on the way (§4); nothing else in it is rewritten (vLLM's `id`
and `created` stay, SGLang's and llama.cpp's absence of them stays).

### 4. A streamed answer, its evidence and settlement

**The answer is streamed through** (owner decision 2026-10-27, question 6). A batch of
2048 inputs of 4096 floats is up to about 200 MB as JSON text, so the router neither reads
the answer whole nor caps its size. When the engine answers HTTP 200, the router relays the
answer to the client as its bytes arrive, through an incremental parser that knows one
fixed JSON shape and keeps only its own state, never the answer:

- a top-level object with `object`, `data`, `model` and `usage`, in any order;
- `data`, an array of entry objects, each with `object`, `index` and `embedding`;
- `model`, a string, replaced by the public name on the fly whether it comes before or
  after `data`: the parser drops the engine's value as it passes (SGLang's is its model
  path, not the served name) and writes the public name in its place;
- `usage`, an object whose `prompt_tokens` and `total_tokens`, when present, are
  non-negative integers.

Other members, at the top level or in an entry (vLLM's `id` and `created`, llama.cpp's
per-entry `encoding_format`), are relayed unchanged once they parse as JSON values.
While the bytes pass, the parser checks each entry: its `index` is one of 0 to n−1 and not
seen before (one bit per input); a `float` embedding is an array of finite numbers, of the
first vector's length and, when `dimensions` was asked, of that length; a `base64`
embedding is a string of the base64 alphabet that decodes to a whole number of 4-byte
values, the same number for every entry. A number longer than 64 bytes, a member name
longer than 256 bytes or nesting deeper than 32 levels is malformed, so the parser's state
stays bounded whatever the engine sends. The response to the client carries no
`Content-Length`, since the rewritten `model` and the added `capyctl.metrics` (§9) change
the length.

**Completion.** An answer is completion when it has ended and every check has passed:
`object` is `"list"`, `data` held exactly one entry per input with every index seen, and
the JSON is whole. The router withholds the top-level object's closing brace until then,
and writes `capyctl.metrics` and the brace only after the last check, so a client receives
a whole JSON document only for an answer that passed. An engine 400, 413 or 422 with a
complete JSON body is a rejection, read whole as engine error answers are today and
relayed as `engine_rejected` (SPEC §10): an input longer than the model's limit, a
`dimensions` the model does not support. Anything else leaves the request uncertain, as
for decisions: the engine may have run it, so it is not replayed.

**The consequence of streaming.** A malformed answer is detected only after part of it
has reached the client. The router then stops relaying and ends the response without the
closing brace; the client already has HTTP 200 and receives an incomplete JSON body, which
no JSON parser accepts. The request is uncertain, as before. The same holds when the
engine closes the connection before the answer ends. In exchange, no batch the contract
allows is refused for its answer's size, in `float` or in `base64`, and the router's
memory does not grow with the answer. The proposal's 64 MiB `EMBEDDING_RESPONSE_LIMIT`
and its `413 request_too_large` refusal before forwarding are dropped.

**Request IDs.** The caller's `X-Request-Id` is checked, echoed on the answer or error,
and never forwarded, exactly as for chat and decisions (SPEC §10, note of 2026-10-19).
CapyCTL's own request ID is not sent to the engine, on any engine: SGLang needs one `rid`
per input of a batch, and the settlement below needs none (owner decision 2026-10-27,
question 7). The ingress consumes CapyCTL's request ID header and does not set `rid` on
embeddings.

**Cancellation and settlement** (owner decision 2026-10-27, question 7). A cut embedding
request runs to its end and settles on its answer. A client that hangs up, or a request
cut by its deadline, no longer drops the engine request: the router keeps the engine
connection open and keeps reading and checking the answer to its end, relaying nothing
more to the client. vLLM's `with_cancellation`, SGLang's disconnect check and llama.cpp's
`should_stop` therefore do not fire, and the engine finishes the request. The lease is
`cancelling` meanwhile. When the answer is completion by the rules above, the request
settles on it, the completion evidence SPEC §10 already accepts, and its place and charge
are released then. A complete engine error answer is answered, as today, and settles on
the engine's idle reading. Only a cut request whose answer never completes (the engine
closes the connection, the answer is malformed, or the instance stops first) keeps
today's rule: it settles at its instance's next verified stop (SPEC §10, amendment of
2026-10-21, owner decision 6 of that date), and when such requests alone fill the
deployment's 32 places CapyCTL restarts the instance and journals
`instance_restarted_for_held_requests` (ADR 0032 §7). Park, idle park and switch drains
treat an embedding request as they treat a decision, and a cut one whose answer is still
being read as in flight until it settles.

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
after a restart, after a wake (§7.2), and when a host or a restarted standalone role
re-proves a retained launch; `/health` and `/v1/models` alone are never readiness (SPEC
§6.1), and an embeddings deployment is never sent a chat probe in place of it. The kind
and version are recorded with the reference (§6); any change to the request is a new
version, and a reference recorded by another version is never compared with it.

### 6. The wake canary compares vectors with a tolerance

The first ready answer is recorded as the reference: the dimension and the vector, stored
as little-endian float32 (10 KiB for Qwen3-Embedding-4B's 2560 dimensions) beside the
kind, version and tolerance (a new stored form beside `StoredDecisionCanary`). A later
check passes when the dimension is the same and the relative distance `‖v − r‖ / ‖r‖` is
at most the tolerance; otherwise it fails `wake_mismatch` and the instance is stopped, as
today. For unit vectors a relative distance `d` is a cosine similarity of `1 − d²/2`.

The tolerance is set from row EM3's largest measured distance plus a stated margin, and no
number is committed before that; the figure EM3 is checked against is 0.02 (cosine 0.9998
for unit vectors) (owner decision 2026-10-27, question 3). The probe goes to an engine with
no other traffic at start, after a restart and after a wake; BF16 vectors of one input
move slightly with batching and kernel choice [INFERENCE, not measured], and a model
loaded wrong gives an unrelated vector, far beyond that.

The check runs after every wake of a parked vLLM or SGLang deployment (§7.2), before the
woken instance takes a request, and after every restart of a deployment that has a
reference, on every engine.

### 7. Scope of the first version

1. **One GPU on one machine.** An embeddings deployment with a `topology` is refused, and
   it is not combined with ADR 0035's several GPUs on one host: the embedding models in
   this release fit one GB10 or one discrete card, and group readiness, stall detection
   and the group canary would each need an embedding form.
2. **Deep parking on vLLM and SGLang, restart on llama.cpp** (owner decision 2026-10-27,
   question 2; the owner first answered `restart_only` and then reversed it). A vLLM or
   SGLang embeddings deployment parks `deep` with the calls a chat deployment's `deep`
   park uses (vLLM's sleep and wake, SGLang's release, resume and reload) and their
   protections (ADR 0012), and the canary of §6 runs after every wake. A deployment that
   states no residency resolves `deep` on vLLM and SGLang, on a discrete host too;
   `host_backed` is refused, since row EM9 qualifies `deep` on host A, a unified GB10,
   where `host_backed` does not apply. Neither engine's sleep path has been read or run
   with a pooling server; row EM9 (park and wake with the canary after every wake)
   qualifies it per engine. If EM9 fails on an engine, that engine's embeddings
   deployments resolve `restart_only` in 0.2.0 and a stated `deep` is refused
   `capability_missing:deep_park`, as ADR 0035 §9 gates deep parking per engine: a failed
   row is a known fault, not a missing certification. An uncertified version of vLLM or
   SGLang (§1) parks as the certified one does, and the wake canary checks every wake.
   llama.cpp, which cannot park, and custom engines resolve `restart_only`, and `deep` or
   `host_backed` is refused there as for chat.
3. **Text only.** Multimodal inputs (vLLM's `messages`, SGLang's and llama.cpp's content
   objects) are refused (owner decision 2026-10-27, question 5).

### 8. Accounting

An embedding request generates nothing: no output tokens, no decode. Admission already
reserves no generated tokens, so nothing changes there. Memory is sized as for the same
checkpoint today (weights, KV and margin): the engine holds KV only while it reads each
input, so the KV an embeddings deployment needs is about its concurrent inputs times
their length, small beside a chat model's. For Qwen3-Embedding-8B (36 layers, 8 KV heads
of 128, BF16, per its `config.json`) that is 144 KiB a token, 4.5 GiB for one full
32,768-token input. CapyCTL keeps today's sizing rules and the recipes declare small
requests, revisited with EM1's peaks (owner decision 2026-10-27, question 11).
[INFERENCE: arithmetic, 36 × 8 × 128 × 2 × 2 bytes a token; encoder-only checkpoints may
need no KV at all on vLLM; row EM1 records the peak.]

Waiting accounts a request's body bytes as for chat; an embeddings deployment's requests
count against the same per-deployment bound of 32.

### 9. Metrics

The router records embedding requests in its latency series
(`crates/capyctl-router/src/timing.rs:47-58`, published as JSON at
`GET /management/v1/metrics/latency`) as it records decisions: queue wait, activation
wait, selection, lease grant, pre-forward, first byte and total; not time to first
content or to the last chunk, which have no meaning for an answer that is not a stream of
tokens. The answer carries, written before its closing brace (§4), `capyctl.metrics` with
the figures that describe an embedding, each labelled with its source: `total_ms` and
`time_to_response_ms` (router), `input_tokens` (engine: the answer's
`usage.prompt_tokens`) and `inputs` (router: the number of inputs). A figure
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
same 8 MiB body limit. It relays the answer unchanged as its bytes arrive, without reading
it whole; the router checks it and rewrites `model` (§4).

## Consequences

- Clients that embed documents and questions can use the same CapyCTL endpoint and API
  key as their chat model, and an embedding model shares a GPU with a chat model through
  the ordinary switching of SPEC §10: on vLLM and SGLang it parks `deep` and wakes on
  request, checked by the canary after every wake; on llama.cpp it is stopped and
  restarted.
- Every place that proves readiness gains an embedding branch (adapters, agent,
  controller, the wake path, the stored canary); the fake engine gains an embeddings mode
  with sleep and wake, so CPU tests cover routing, the streamed answer, accounting,
  readiness, parking and the canary. CPU and Fake-engine tests are not qualification.
- A malformed answer is detected only after part of it has reached the client (§4): the
  client has HTTP 200 and an incomplete body that no JSON parser accepts, and the request
  is uncertain, as before. In exchange, no batch the contract allows is refused for the
  size of its answer, and the router's memory does not grow with the answer.
- A cut embedding request keeps running on the engine, which spends that work for nobody;
  the waste is bounded by one request, and the request settles on its answer instead of
  holding its place until the next verified stop. ADR 0032 §7's restart for held requests
  remains only for cut requests whose answer never completes.
- An older engine version, a custom engine or a checkpoint outside the ten runs as an
  uncertified custom setup: CapyCTL serves it with the same safety refusals and checks,
  promises nothing about it, and status shows it `uncertified`. A fault there shows as a
  failed start, an uncertain answer or `wake_mismatch`; it is not prevented in advance.
- The OpenAI Python client sends `base64` when the caller names no format, and decodes it
  as float32 ([`lib/_parsing/_embeddings.py`](https://github.com/openai/openai-python/blob/main/src/openai/lib/_parsing/_embeddings.py),
  lines 34-38 and 55-56, read 2026-10-24). Most clients therefore exercise the `base64`
  path first; `float` batches of any size stream as well.
- Recipes gain an embedding form: one recipe per certified model and engine, pinning the
  checkpoint revision (and, on llama.cpp, the GGUF), with latency at p50 and p95 by batch
  size and input length, inputs per second as users increase, peak memory, start time,
  park and wake times on vLLM and SGLang, and a retrieval score on a pinned public set,
  without tokens per second. The combinations an engine does not support are listed with
  the reason. `capyctl-bench` (in the recipes repository) gains an embeddings mode.
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
- **The qualified versions as floors, older ones refused** (the proposal's
  recommendation). Rejected by the owner (decision 2026-10-27, question 1, and the owner
  rule of that day): an older version that serves embeddings runs uncertified.
- **`restart_only` in the first version** (the proposal's recommendation). Rejected by the
  owner (decision 2026-10-27, question 2): vLLM and SGLang park `deep`, qualified by EM9.
- **Reading the answer whole up to a fixed 64 MiB, and refusing before forwarding a
  request whose answer could exceed it** (the proposal's recommendation). Rejected by the
  owner (decision 2026-10-27, question 6): it refused ordinary `float` batches of large
  vectors (above 680 inputs at 4096 dimensions) and held the whole answer in memory.
  Its one advantage, that a malformed answer never reached the client, is lost (§4).
- **The decision limit of 16 MiB, or a role setting for the limit.** Rejected with the
  limit itself (question 6).
- **Reading an answer of any size whole.** Rejected: the router would hold an unbounded
  answer in memory; streaming it through needs no limit and holds none of it.
- **Settling a cut embedding only at the next verified stop** (the proposal's
  recommendation). Rejected by the owner (decision 2026-10-27, question 7): an embedding
  request is short and bounded, unlike a generation, so running it to its end settles it
  on completion evidence instead of holding it, and restarting, until a stop. It remains
  the fallback for a cut request whose answer never completes.
- **Extending ADR 0032's abort to SGLang embeddings**, with `rid` as `<request ID>_<i>`
  per input. Not taken (question 7): on 0.5.22 the abort echo carries no `embedding`
  (`managers/tokenizer_manager.py:3624-3640`), so the handler, which reads
  `ret_item["embedding"]` (`serving_embedding.py:292`), fails rather than answering a
  final output [INFERENCE: probably an HTTP 500 from its error path, not run].
- **Settling a cut embedding on the engine's idle reading.** Unsound under owner
  decision 6 of 2026-10-21: an idle reading does not show that this request's answer
  ended.

## Implementation plan (target 0.2.0)

1. **Config.** `DeploymentApi::Embeddings` (`crates/capyctl-config/src/effective.rs`), a
   new `embeddings.rs` beside `decisions.rs` with the certified engine versions (for
   status, never a refusal), the TensorFold refusal, the topology refusal and the
   residency rules of §7.2 (`deep` on vLLM and SGLang behind each engine's support data,
   `host_backed` refused, `restart_only` on llama.cpp and custom engines); vLLM
   `--runner`/`--convert` and SGLang `--is-embedding` reserved (`engine_policy.rs`);
   llama.cpp `--pooling` ordinary on embeddings deployments, `none` refused
   (`llamacpp.rs`); LMCache off with `embeddings_unsupported` (`lmcache.rs`); the custom
   manifest's `api` gains `embeddings` (`custom.rs`).
2. **Protocol.** The host capability `embeddings`, the embedding probe requirement and
   `host_capability_missing:embeddings` (`crates/capyctl-protocol/src/capabilities.rs`).
3. **Adapters.** Rendering per §2, with the sleep-mode and memory-saver options a `deep`
   chat deployment renders on vLLM and SGLang; in `forward.rs`,
   `EMBEDDING_REQUEST_FIELDS`, `validate_embedding_request`, `embed()`, the incremental
   answer parser and `model` rewriter of §4, and `embedding_answered`;
   `embedding_probe.rs`; each adapter's initialize and wake choose the probe by API.
4. **Router.** A third `Endpoint` (`endpoint.rs`), the route (`lib.rs`), a streamed path
   beside the collected one of decisions (`chat.rs`) that withholds the closing brace
   until the answer passed, the llama.cpp `dimensions` refusal, a cut request read to its
   end and settled on its answer, and `capyctl.metrics` (`engine_metrics.rs`).
5. **Agent and controller.** The ingress path, its validator and its streamed relay
   (`ingress.rs`); the retained re-proof by API (`native_execution.rs`,
   `local_readiness.rs`); the native dimension recorded with the launch's readiness;
   park and wake of embeddings deployments with the canary after every wake.
6. **Store.** `StoredEmbeddingCanary` (dimension, vector, tolerance, kind, version) and
   its comparison (`crates/capyctl-store/src/wake_canary.rs`,
   `crates/capyctl-controller/src/wake_canary.rs`), with a migration; the settlement of a
   cut embedding request on its answer.
7. **Fake engine.** An embeddings mode in `crates/capyctl-testkit/src/fake_engine.rs`
   returning set vectors in chunks, with `model` before or after `data`, malformed or
   broken answers on demand, sleep and wake, and refusing chat and decisions.
8. **Docs.** The SPEC texts below; `docs/guide/requests.md` ("Embedding requests", the
   incomplete body of an answer that failed its checks), `docs/guide/deploy.md`
   ("Embedding models", parking per engine), `docs/guide/engines.md` (each engine's flags
   and limits, the certified combinations and the documented unsupported ones),
   `docs/guide/errors.md` (`api_mismatch` and `invalid_request` on `/v1/embeddings`),
   `docs/operations/configuration.md` (`api`, the manifest value),
   `docs/operations/network-access.md` (the ingress paths); one change file with the
   release note. In the recipes repository, the recipes and `capyctl-bench` mode of the
   Consequences.

SPEC texts to land with the implementation: §6.1 (an embeddings deployment is ready on
the fixed embedding of ADR 0036 §5, and its canary compares vectors within a tolerance
after every wake and restart); §6.2 (an embeddings deployment parks `deep` on vLLM and
SGLang, `host_backed` refused, `restart_only` on llama.cpp and custom engines); §9.1 and
§9.2 (deep parking of a pooling server); §10 (the inference surface includes non-streaming
`POST /v1/embeddings` for `api: embeddings`, its closed contract, the answer streamed
through with incremental checks and `model` rewritten, with no answer limit, and a cut
request run to its end and settled on its answer, at the next verified stop only when no
complete answer arrives); §15.1 (`api`: `chat`, `decisions` or `embeddings`); §20 (T47).

Tests, tagged **T47 — embedding models**: `api: embeddings` resolves and round-trips
through the frozen revision; it resolves on vLLM, SGLang and llama.cpp at every version,
the certified versions shown certified and the others uncertified, and is refused on
TensorFold (`capability_missing:embeddings`), with a `topology`, with `host_backed`, with
`deep` on llama.cpp and custom engines, with a stated parser and with `lmcache:
required`; a vLLM or SGLang deployment that states no residency resolves `deep`, and
`restart_only` with a stated `deep` refused while that engine's support data has
embeddings parking off; the reserved vLLM and SGLang options are refused on every
deployment and `--pooling none` on llama.cpp; each engine renders its option and
llama.cpp its batch sizes; `POST /v1/embeddings` accepts each input shape and refuses
every other field, empty inputs, mixed arrays, more than 2048 inputs, negative or
oversized token IDs, `bytes` and `stream`; it is routed, admitted and leased like
decisions; the mismatched endpoints are refused before activation; the answer reaches
the client before the engine has finished it; `model` is rewritten when it comes before
`data`, after it and split across chunks, and nothing else is; a 2048-input `float` answer
of 4096-dimension vectors streams through and the router's memory does not grow with it;
an answer with the wrong count, a repeated index, ragged vectors, a non-finite value, the
wrong `dimensions`, an oversized token, invalid JSON or a closed connection is uncertain
and reaches the client without its closing brace; an engine 400 is `engine_rejected`;
llama.cpp refuses a non-native `dimensions`; a cut request keeps its engine connection
open, is read to its end and settles on a complete answer and not before; a cut request
whose answer breaks settles at the verified stop, and a full deployment of them restarts;
the deployment is ready only on a well-formed fixed embedding and never on a chat probe;
a vLLM or SGLang deployment parks and wakes, and the canary runs after every wake and
every restart, passes within the tolerance and fails on a changed dimension or a vector
beyond it; a reference from another kind or version is not compared; an old host never
receives an embeddings deployment (`host_capability_missing:embeddings`);
`capyctl.metrics` holds only measured figures and comes before the closing brace. Tests
also tag T17 (settlement) and T43 (`api_mismatch`) where they apply. CPU and Fake-engine
tests are not qualification.

## Live qualification

Run in a live session the owner schedules (one live session on the hosts at a time), on
host A unless stated, in the approved engine environments as they are
(`~/vllm-0.32.0-venv`, SGLang 0.5.22 and the llama.cpp v0.7.0 build).

**Models** (owner decision 2026-10-27, item 12): the ten open-weight embedding models with
the most weekly tokens on OpenRouter on 2026-10-27, each on every engine that can run it,
at the revision Hugging Face served on 2026-10-27. NVIDIA's Nemotron 3 Embed 1B has
public weights (`nvidia/Nemotron-3-Embed-1B-BF16`: `model.safetensors`, not gated,
OpenMDW-1.1 license, read 2026-10-27), so it is qualified and the owner's fallback,
all-MiniLM-L12-v2, is not needed. The architecture, pooling and dimension are each
checkpoint's `config.json`, `1_Pooling/config.json` and model card. The engine columns
are each engine's source at the certified version, read on GitHub on 2026-10-27, and no
engine was run: "registered" means the architecture is in vLLM's model registry
(`vllm/model_executor/models/registry.py`), among SGLang's model entry classes
(`python/sglang/srt/models/`), or among llama.cpp's converter registrations
(`conversion/`). Whether an engine serves a model correctly is to be established by EM1
in every cell.

| Model | Revision | Architecture, pooling | Dimension | vLLM 0.32.0 | SGLang 0.5.22 | llama.cpp b11669 |
|---|---|---|---|---|---|---|
| [Qwen3-Embedding-8B](https://huggingface.co/Qwen/Qwen3-Embedding-8B) | `1d8ad4c` | `Qwen3ForCausalLM`, last token | 4096 (MRL 32 to 4096) | registered; converted to an embedding model by `--convert auto` | registered; `--is-embedding` | registered; Qwen's [GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-8B-GGUF) Q8_0 |
| [Qwen3-Embedding-4B](https://huggingface.co/Qwen/Qwen3-Embedding-4B) | `5cf2132` | `Qwen3ForCausalLM`, last token | 2560 (MRL 32 to 2560) | registered; as above | registered; SGLang's embedding page uses it as its example | registered; Qwen's [GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-4B-GGUF) Q8_0 |
| [bge-m3](https://huggingface.co/BAAI/bge-m3) | `5617a9f` | `XLMRobertaModel`, CLS | 1024 | registered | registered | registered; converted GGUF |
| [pplx-embed-v1-0.6b](https://huggingface.co/perplexity-ai/pplx-embed-v1-0.6b) | `2c4d510` | `PPLXQwen3Model`, code shipped with the checkpoint (`auto_map`), bidirectional, mean | 1024 (MRL) | not registered; only through the Transformers backend with remote code, to be established by EM1 | not registered; as for vLLM | not supported: the converter registers no `PPLXQwen3Model` |
| [pplx-embed-v1-4b](https://huggingface.co/perplexity-ai/pplx-embed-v1-4b) | `0645649` | as above | 2560 (MRL) | as above | as above | not supported, as above |
| [multilingual-e5-large](https://huggingface.co/intfloat/multilingual-e5-large) | `3d7cfbd` | `XLMRobertaModel`, mean | 1024 | registered | registered | registered; converted GGUF |
| [bge-base-en-v1.5](https://huggingface.co/BAAI/bge-base-en-v1.5) | `a5beb1e` | `BertModel`, CLS | 768 | registered | registered | registered; converted GGUF |
| [all-MiniLM-L6-v2](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) | `1110a24` | `BertModel`, mean | 384 | registered | registered | registered; converted GGUF |
| [all-mpnet-base-v2](https://huggingface.co/sentence-transformers/all-mpnet-base-v2) | `e8c3b32` | `MPNetForMaskedLM`, mean | 768 | not registered; only through the Transformers backend, to be established by EM1 | not registered; as for vLLM | not supported: the converter registers no MPNet architecture |
| [Nemotron-3-Embed-1B-BF16](https://huggingface.co/nvidia/Nemotron-3-Embed-1B-BF16) | `c0c9fea` | `Ministral3Model`, bidirectional, mean | 2048 (truncation from the start, per its card) | not registered (only `Ministral3ForCausalLM`); its card names vLLM 0.20.0 to 0.25.0, not 0.32.0; to be established by EM1 | not registered (only `Ministral3ForCausalLM` is an entry class); to be established by EM1 | not supported: the converter registers `Ministral3ForCausalLM` only |

Notes on the table:

- **Not supported** marks a combination the engine's source cannot run; it is documented
  in `docs/guide/engines.md` and the recipes, and not refused (§1). Each "to be
  established by EM1" cell, and each "registered" cell, becomes certified when its EM1
  passes, or documented unsupported with the engine's error when it fails. Whether vLLM's
  and SGLang's Transformers backends serve the unregistered architectures as embedding
  models, with bidirectional attention and mean pooling where the model needs them, is
  [INFERENCE] until EM1.
- **Remote code.** The pplx-embed checkpoints ship their own Python, so on vLLM and
  SGLang they need a profile added with `--allow-remote-code` (`security.trust_remote_code`,
  ADR 0014 §8) and `engine_config.trust_remote_code: true`; the row runs the pinned
  revision's code. Their card says they produce unnormalized int8-quantized embeddings
  through Sentence Transformers; what the engines answer is recorded by EM1, and the
  canary's relative distance (§6) does not need normalized vectors.
- **GGUFs.** Qwen publishes GGUFs of Qwen3-Embedding-4B and 8B, used at Q8_0. For the
  other models llama.cpp runs a GGUF converted from the pinned revision with b11669's
  `convert_hf_to_gguf.py`, recorded with its digest and the environment the conversion
  ran in [INFERENCE: the converter needs Python with torch and `gguf-py`; no engine
  environment changes].

**Reference model.** Rows EM2 to EM9 run on Qwen3-Embedding-4B (BF16 on vLLM and SGLang,
Qwen's Q8_0 GGUF on llama.cpp; Matryoshka from 32 to 2560 dimensions, per its model card),
which is among the ten and registered on all three engines. The proposal's
Qwen3-Embedding-0.6B is not among the ten and is dropped.

A combination is certified when its EM1 passes and rows EM2 to EM8, and on vLLM and
SGLang EM9, pass on its engine.

| Row | What it shows |
|---|---|
| EM1 | Each model on each engine of the table not marked unsupported: the deployment starts with its rendered option, becomes ready on the fixed embedding with the native dimension of the table recorded, answers a string and a 16-input batch with one vector per input in order, shows `api: embeddings` and `certified` in status and `capyctl.api` in `/v1/models`, and LMCache off with `embeddings_unsupported`; peak memory, start time and the charge recorded, llama.cpp with the rendered batch sizes. A failure is recorded with the engine's error and documents the combination unsupported. |
| EM2 | Contract per engine: a string, an array of strings, a token array and an array of token arrays each answer one vector per input in order; `base64` decodes to the `float` answer's values (within float32 rounding); `dimensions: 256` answers 256-length vectors on vLLM and SGLang (with the Matryoshka override in the arguments if the checkpoint needs it) and is refused on llama.cpp; `stream`, an unknown field and an empty input are refused by CapyCTL; an input over the model's length is the engine's `engine_rejected`; `model` in the answer is the public name, and whether the engine writes `model` before or after `data` is recorded. |
| EM3 | Tolerance: the probe text sent 20 times alone after start, 20 times after a restart, 20 times after a wake on vLLM and SGLang, and 20 times inside 64-input batches under 8 concurrent clients; the largest relative distance and smallest cosine from the reference, per engine, set the canary tolerance with its margin, checked against 0.02. Cross-engine agreement on 100 pinned sentences (vLLM against SGLang, BF16; llama.cpp Q8_0 against both) is recorded, not required. |
| EM4 | Restart canary: stop and start each deployment; the check compares with the reference and passes, and the event names the verdict. |
| EM5 | Cut requests and gauges: during a 2048-input batch each engine's running gauge reads the request; a client hang-up mid-batch and a deadline cut each leave the engine connection open, the engine finishes the request, the lease is `cancelling` until the answer has been read whole, and it then settles on it; the time from the cut to the settlement is recorded; 32 cut batches in a row each settle on their answer and no restart happens; an engine killed mid-answer leaves its cut request to the next verified stop; the next request is served. |
| EM6 | Host path: host A as a host of a server on the control host; a request through its ingress is validated, relayed and answered, the answer relayed as it arrives, `X-Request-Id` echoed and never seen by the engine. |
| EM7 | Switching: on one GPU, an embeddings deployment and a chat deployment that do not fit together; a request for each in turn switches the pool per SPEC §10 and both answer; on vLLM and SGLang the embeddings deployment parks `deep` and its wake passes the canary, on llama.cpp it restarts. |
| EM8 | Size and load: `capyctl-bench`'s embeddings mode for p50 and p95 by batch size (1, 16, 256) and input length (32, 512, 8,192 tokens), inputs per second at 1, 4 and 16 clients, and a retrieval score on a pinned public set; then, on vLLM with Qwen3-Embedding-8B (4096 dimensions), a 2048-input `float` batch and the same batch in `base64` each stream through whole to the client with `model` the public name, and the router's resident memory is recorded before and during each to show it does not hold the answer. |
| EM9 | Park and wake (owner decision 2026-10-27, question 2), on vLLM and SGLang, `deep`: 5 park and wake cycles each; every park settles on the engine's evidence (vLLM: the launch's GPU bytes at or below the parked budget; SGLang: its saver map unmapped) and its residue is recorded; the canary runs after every wake and passes, and a request after every wake is answered; a request that arrives while the deployment is parked wakes it. Park and wake times recorded. |

## Owner decisions of 2026-10-27

The proposal of 2026-10-24 asked eleven questions; the owner answered all of them on
2026-10-27 and named the models to qualify. The decision sections above already read as
decided. The owner's rule of the same day applies throughout: certified engine, version
and checkpoint combinations are advertised as certified, anything else runs as an
uncertified custom setup and is never refused for being uncertified, and the safety
refusals (reserved options, loopback-only controls, malformed requests) are unaffected.

1. **Engine versions (changed from the recommendation, by the owner rule):** vLLM 0.32.0,
   SGLang 0.5.22 and llama.cpp v0.7.0, the latest approved installs, are certified; older
   versions that serve embeddings run uncertified instead of being refused below a floor
   (§1).
2. **Parking (changed from the recommendation):** (b) deep parking in 0.2.0 on vLLM and
   SGLang, qualified by a park-and-wake row with the canary (EM9); llama.cpp stays
   `restart_only`. The owner first answered `restart_only` and then reversed it (§7.2).
3. **Canary tolerance:** (a) set from EM3's largest distance plus a stated margin,
   checked against 0.02, as proposed (§6).
4. **`dimensions` on llama.cpp:** (a) accepted only at the native dimension, as proposed
   (§3).
5. **Inputs beyond OpenAI's:** (a) OpenAI's text contract only, as proposed (§3, §7.3).
6. **Answer limit (a new option):** the answer is streamed through to the client with
   incremental validation and `model` rewritten on the way, with no answer cap; it
   replaces the proposed read-whole 64 MiB limit and its refusal before forwarding. A
   malformed answer is then detected after part of it has reached the client, and the
   request is uncertain as before (§4).
7. **Cut embeddings (changed from the recommendation):** (c) a cut embedding request runs
   to its end and settles on its answer, SPEC §10's completion evidence; today's rule
   remains only for a cut request whose answer never completes (§4, EM5).
8. **Custom engines:** (a) a custom engine's manifest may declare `api: embeddings`, as
   proposed (§1).
9. **Chat checkpoints served as embeddings:** (a) allowed, as proposed, as two revisions
   of the checkpoint; they run uncertified (§1).
10. **`cache_salt`:** (a) refused on embeddings for now, as proposed, and the guide says
    an embeddings deployment's engine prefix cache, where it keeps one, is shared by all
    its callers (§3).
11. **Memory defaults:** (a) today's sizing rules, with recipes declaring requests,
    revisited with EM1's peaks, as proposed (§8).
12. **Models:** qualify the ten open-weight embedding models with the most weekly tokens
    on OpenRouter on 2026-10-27: Qwen3-Embedding-8B, Qwen3-Embedding-4B, BAAI/bge-m3,
    perplexity-ai/pplx-embed-v1-0.6b, perplexity-ai/pplx-embed-v1-4b,
    intfloat/multilingual-e5-large, BAAI/bge-base-en-v1.5,
    sentence-transformers/all-MiniLM-L6-v2, sentence-transformers/all-mpnet-base-v2, and
    NVIDIA Nemotron 3 Embed 1B, or all-MiniLM-L12-v2 if its weights are not public (they
    are). Each runs on every engine that can run it, and the unsupported combinations are
    documented (Live qualification).
