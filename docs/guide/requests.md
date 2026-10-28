# Make a request

CapyCTL serves the OpenAI chat API on port 8443 of the machine that runs it (the
server, with several machines), on every interface, with an API key. On that
machine the endpoint is `http://127.0.0.1:8443/v1`. Send the deployment's
`name` as `model`. A decision model answers
[decision requests](#decision-requests) on the same port instead of chat, and
an embedding model answers [embedding requests](#embedding-requests).

## The API key

The first start wrote it to the credentials file:

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/capyctl/identity/credentials)
```

On several machines the key is on the server machine, in
`~/.local/state/capyctl/identity/server-credentials.json`:

```bash
KEY=$(sed -n 's/.*"api_key": *"\([^"]*\)".*/\1/p' ~/.local/state/capyctl/identity/server-credentials.json)
```

## List models

```bash
curl -s http://127.0.0.1:8443/v1/models -H "Authorization: Bearer $KEY"
```

```text
{"data":[{"capyctl":{"api":"chat"},"id":"my-model","object":"model"}],"object":"list"}
```

Listing never wakes a model. A request without the key, or with a wrong one,
is answered `401`. `capyctl.api` is the endpoint the model answers: `chat`
for `/v1/chat/completions`, `decisions` for `/v1/systemone`, `embeddings` for
`/v1/embeddings`.

## Chat

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

```text
{"capyctl":{"metrics":{"ttft_ms":{"source":"router","value":212.4}}},"choices":[{"finish_reason":"stop","index":0,"message":{"content":"Hello! How can I help you today?","role":"assistant"}}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion"}
```

The answer text comes from your model; this one is an example. `capyctl`
holds the request's [metrics](#per-request-metrics).

## Streaming

Add `"stream": true`:

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}], "stream": true}'
```

```text
data: {"choices":[{"delta":{"content":"","role":"assistant"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":"Hello!"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" How"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" can"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" I"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" help"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" you"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" today?"},"finish_reason":null,"index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

: x-capyctl-metrics {"ttft_ms":{"source":"router","value":212.4}}

data: [DONE]
```

### Streamed fields

CapyCTL relays each chunk unchanged except `model`, and checks it first: a
`delta` key not in this table ends the stream as unverified, so an untested
field never reaches a client. Fields beside `delta` in a
choice, and `usage` with its details, are relayed as the engine sent them.

| Field | Where | vLLM 0.29–0.31 | SGLang 0.5.18–0.5.21 | TensorFold 0.6.x | llama.cpp v0.6.0 |
|---|---|---|---|---|---|
| `role` | delta | yes | yes (may be `null`) | yes | yes (opening chunk) |
| `content` | delta | yes | yes | yes | yes (`null` on the opening chunk) |
| `reasoning` | delta | yes | — | — | — |
| `reasoning_content` | delta | — | yes | yes | yes |
| `tool_calls` | delta | yes | yes | yes | yes |
| `index`, `finish_reason`, `logprobs` | choice | yes | yes | `index`, `finish_reason` | yes |
| `stop_reason`, `token_ids` | choice | yes | — | — | — |
| `matched_stop` | choice | — | yes | — | — |
| `prompt_tokens`, `completion_tokens`, `total_tokens` | usage | yes | yes | yes | yes |
| `prompt_tokens_details.cached_tokens` | usage | yes (`--enable-prompt-tokens-details`) | yes (when any were cached) | yes | yes |
| `completion_tokens_details.reasoning_tokens` | usage | yes | — | yes | — |
| `reasoning_tokens` | usage | — | yes | — | — |
| `metrics` | chunk | yes (usage chunk) | — | — | — |
| `exact_mode`, `tensorfold`, `speculative` | chunk | — | — | yes (finish chunk) | — |
| `system_fingerprint`, `timings` | chunk | — | — | — | yes (`timings` on the last chunk) |

Not relayed: SGLang's `hidden_states` delta (sent only for
`return_hidden_states`, a request field CapyCTL refuses) and vLLM's `citations`
delta (its Cohere endpoint only). The table was read from the vLLM 0.29.0,
SGLang 0.5.20 and TensorFold 0.6.0 and 0.6.3 sources; vLLM 0.30's `reasoning`
was confirmed live, vLLM 0.31.0's streamed answers were relayed live, and the
other listed versions are assumed to match until run against them.

### When a request is cut short

If your client hangs up, or a request runs past its deadline (no answer
within `queue.request_deadline`, or a stream that goes quiet for
`queue.stream_idle_timeout`), CapyCTL closes the engine connection, which
asks the engine to stop. That holds for streamed and non-streamed chat
requests and for decision requests alike, and for a model served on a host,
whose ingress closes its own engine connection when CapyCTL's goes away.
Embedding requests are the exception: CapyCTL lets them run to their end
([Embedding requests](#embedding-requests)).
(Before 0.1.7, a non-streamed request whose client hung up ran on to its
end, and a stream whose client hung up before its first token kept its
engine connection until the engine produced that token.) Closing a
connection does not prove the engine stopped, so the request
stays counted against its model's memory until CapyCTL sees the engine stop:

- If the engine had already answered (it produced part of the answer, or
  answered with an error), CapyCTL counts the request finished the first time
  the engine reports no running and no waiting requests afterwards.
- If it had not (the cut came before the engine's first token, streamed or
  not; a chunk that only names the assistant role does not count, and a
  decision request has no tokens before its whole answer), the engine may not
  have started the request yet and could still run it after reporting itself
  idle. The request stays counted until the engine's next stop or restart.
  Stopping or switching does not wait for it, and a model that would park
  stops instead.

On SGLang 0.5.22 started with one tokenizer worker (the default), CapyCTL
does more for a chat request. It sends the engine its own request ID, the
`X-CapyCTL-Request-Id` value (never your `X-Request-Id`), as SGLang's `rid`,
so the answer's `id` is that same ID. When the request is cut, CapyCTL asks
SGLang to abort that ID and keeps reading the engine's answer, which no
longer reaches you, until SGLang ends it. That end shows the engine finished
the request, so CapyCTL stops counting it at once, whatever else the engine
is running, instead of waiting for the engine to read idle or for its next
stop. If the end does not come within `queue.stream_idle_timeout` of the
abort, the rules above apply. A model on a host works the same way: its
ingress gives the engine the ID, and the host's agent sends the abort on the
host. Decision requests are not covered: SGLang's decision endpoint takes no
request ID.

Requests that wait for the engine's next stop can add up. When they are the
only requests a model holds and fill its 32 places, CapyCTL restarts the
instance holding them instead of answering new requests `429 queue_full`:
the restart's stop settles them, the journal records
`instance_restarted_for_held_requests` with the number of requests it
settled, and requests waiting for a place wait through the restart as they
would through a start. A request CapyCTL could not account for (`backend
completion unverified`) keeps this from happening, since nothing shows the
engine finished it.

`capyctl status deployment <name>` shows, under the instance table, how many
cancelled requests each instance still counts and how each will be settled.

#### How quickly each engine stops

Measured on an NVIDIA GB10 on 2026-10-22, through CapyCTL built from `main`
at `466b6fa8`: four long generations at once (`max_tokens` 4096, a prompt
that keeps the model counting), all four clients killed once the engine
reported four running requests (and, for a stream, once each client had read
generated text), then the engine's own count of running and waiting requests
read every 100 ms until both were 0. Each row is three repeats; the figures
are seconds from the hang-up to the first reading of 0, median and maximum.
At one reading every 100 ms, 0.1 s is the smallest figure this can show.

| Engine | Model | Streamed | Non-streamed | Count read |
|---|---|---|---|---|
| SGLang 0.5.22 | Qwen3-4B-Instruct-2507, BF16 | 0.22 / 0.22 | 0.10 / 0.10 | `sglang:num_running_reqs`, `sglang:num_queue_reqs` |
| vLLM 0.32.0 | Qwen3-4B-Instruct-2507, BF16 | 0.10 / 0.10 | 0.10 / 0.10 | `vllm:num_requests_running`, `vllm:num_requests_waiting` |
| llama.cpp v0.7.0 | Qwen3-4B, Q4_K_M GGUF | 0.10 / 0.10 | 0.10 / 0.10 | `llamacpp:requests_processing`, `llamacpp:requests_deferred` |
| TensorFold 0.6.6 | FrogNano-4B-2609, NVFP4 | 0.10 / 0.10 | 0.10 / 0.10 | `/health`: `requests_running`, `busy` |

All four requests stopped together on every engine; none started again
within 5 s. CapyCTL asks the engine for a stream even when your client did
not, and collects the answer itself, so a non-streamed request is stopped the
way a stream is. That matters for llama.cpp: asked directly, without a
stream, llama.cpp v0.7.0 does not stop when its client goes away, and the
four requests ran on until their count reached 0 after 65 s (three
repeats, 64.6 to 65.9 s). Asked directly with a stream, it stopped within
0.1 s, as TensorFold did both ways. SGLang and vLLM answer only CapyCTL,
so they were measured only through it.

Decision requests behaved differently on SGLang 0.5.22 (Clef-Flash, four
decisions with an input of about 15,600 tokens each). Its count read 0 for
the first 4 s, while it answered the first of them. It then worked on one at
a time, and closing their connections did not stop the ones it already had:
the three left ran to their ends, about 3.4 s each, and its count read 0
only 10.3 s after the hang-up. In a run where the clients hung up 1.5 s
after sending (about 12,500 tokens each), the count read 0 at the hang-up;
two of those decisions then showed in SGLang's queue and ran within the next
7 s. This is the case the rule above is for: a cancelled decision stays
counted until the engine's next stop or restart. On SGLang 0.5.22 an
abandoned decision therefore still uses the engine until it ends: its
decision endpoint neither stops on a closed connection nor takes a request
ID CapyCTL could cancel
([sgl-project/sglang#45842](https://github.com/sgl-project/sglang/issues/45842)).

## Per-request metrics

Every completed chat answer carries the engine's figures for that request,
under the same names whichever engine serves the model. A non-streaming
answer has them in its body under `capyctl.metrics`; here from vLLM:

```text
"capyctl":{"metrics":{"decode_tokens_per_second":{"source":"engine","value":100.0},"prefill_ms":{"source":"engine","value":41.5},"queue_ms":{"source":"engine","value":0.75},"ttft_ms":{"source":"engine","value":42.25}}}
```

A stream carries them as one comment line just before `data: [DONE]`, here
from SGLang:

```text
: x-capyctl-metrics {"cached_tokens":{"source":"engine","value":8},"decode_tokens_per_second":{"source":"router","value":61.7},"ttft_ms":{"source":"router","value":63.1}}
```

Streaming clients, the `openai` package among them, skip comment lines. To
see it, read the raw stream, for example with
`curl -sN ... | grep '^: x-capyctl-metrics'`.

| Figure | What it measures |
|---|---|
| `ttft_ms` | Time to the first generated token, in milliseconds |
| `queue_ms` | Time the request waited in the engine's queue |
| `prefill_ms` | Time the engine spent on the prompt |
| `decode_tokens_per_second` | Generation speed after the first token |
| `cached_tokens` | Prompt tokens the engine took from its prefix cache |

Each figure is `{"value": ..., "source": ...}`. The source is `engine` when
the engine measured it, and `router` when the engine did not and CapyCTL
measured it on its own clock. A router `ttft_ms` runs from when CapyCTL
sent the request to the engine to the first chunk with generated text (an
answer or reasoning), so it includes the network hop, the engine's queue and
the prompt. A router `decode_tokens_per_second` is the completion tokens
after the first, over the time from that first text to the last chunk. A
figure nobody measured is left out, never shown as zero.

| Figure | vLLM 0.30, 0.31 | SGLang 0.5.21 | TensorFold 0.6.x | llama.cpp v0.6.0 |
|---|---|---|---|---|
| `ttft_ms` | engine: queue plus prompt | router | engine, from when it received the request; router on CUDA Qwen models | router |
| `queue_ms` | engine | — | — | — |
| `prefill_ms` | engine, from when it scheduled the request | — | engine, from when it queued the request (CUDA Qwen models: `prefill_s`, from when it started the request) | engine (`timings.prompt_ms`, from when a slot took the request) |
| `decode_tokens_per_second` | engine, from its mean time between tokens | router | engine; router when it timed no decode, and on CUDA Qwen models | engine (`timings.predicted_per_second`); router when it timed no decode |
| `cached_tokens` | engine, only with `--enable-prompt-tokens-details` | engine, only when some were cached | engine | engine |

CapyCTL starts vLLM with `--enable-per-request-metrics` and SGLang with
`--enable-cache-report`, and a deployment cannot turn either off (see
[Engine options](engine-flags.md)). The engine's own fields stay as the
engine sent them: vLLM's `metrics` object, TensorFold's `tensorfold` object
and llama.cpp's `timings` object reach you unchanged. llama.cpp puts `timings`
on a stream's last chunk: the usage chunk when the request asked for usage,
else the finish chunk.

For a non-streaming answer CapyCTL asks the engine for usage itself. A
stream from vLLM or SGLang carries usage, vLLM's `metrics` and the cached
count only when the request sets `"stream_options": {"include_usage": true}`.
Without it the comment has the router's `ttft_ms` and little else. TensorFold
sends its statistics and usage on every stream.

## Python

With the `openai` package (`pip install openai`):

```python title="client.py"
from openai import OpenAI
from pathlib import Path

key = next(line.split(": ", 1)[1] for line in
           (Path.home() / ".local/state/capyctl/identity/credentials").read_text().splitlines()
           if line.startswith("api_key: "))
client = OpenAI(base_url="http://127.0.0.1:8443/v1", api_key=key)

reply = client.chat.completions.create(
    model="my-model",
    messages=[{"role": "user", "content": "Hello"}],
)
print(reply.choices[0].message.content)

for chunk in client.chat.completions.create(
    model="my-model",
    messages=[{"role": "user", "content": "Hello"}],
    stream=True,
):
    print(chunk.choices[0].delta.content or "", end="", flush=True)
print()
```

```text
Hello! How can I help you today?
Hello! How can I help you today?
```

Any OpenAI-compatible client works the same way: set its base URL to the
endpoint and its API key to `$KEY`.

## Keep tenants' prompt caches apart

vLLM and SGLang reuse the cached start of an earlier prompt when a new one
begins the same way, and so does a deployment's
[LMCache](engines.md#lmcache) server, which keeps those prefixes in host RAM
across a park or a restart. When several tenants share one model, give each
its own `cache_salt`: a request then reuses cached prefixes, in the engine and
in LMCache, only from requests that carry the same salt.

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "cache_salt": "tenant-a",
       "messages": [{"role": "user", "content": "Hello"}]}'
```

The salt must be a non-empty string of at most 1024 bytes; anything else is
refused with `400 invalid_request`. CapyCTL does not pass it on unchanged: it
sends vLLM and SGLang the effective salt, a 64-character SHA-256 digest of
the deployment, the revision serving the request and your salt, and the engine
passes the same value to LMCache. So:

- the same salt shares a cache only within one deployment and revision; a
  new revision, or another deployment, starts apart;
- different salts never share;
- a request without `cache_salt` is sent without one, and shares the cache
  with every other unsalted request to that deployment;
- a salt partitions a cache; it authorizes nothing. Anyone with the API key
  may send any salt.

CapyCTL never logs or stores the effective salt.

TensorFold and llama.cpp ignore the field, so a request with a `cache_salt` for a
model that runs on either is refused rather than served without the isolation it asks
for. The answer is `400` with code `cache_salt_unsupported` (on a stream, an
error event with that code), and nothing reaches the engine. The same request
without `cache_salt` is served.

```text
{"code":"cache_salt_unsupported","message":"this deployment's engine does not partition its prefix cache by cache_salt"}
```

## Decision requests

A deployment with `api: decisions` ([Decision models](deploy.md#decision-models))
answers typed questions about an input instead of generating text, on
`POST /v1/systemone` (the System One API). It answers nothing else, and a chat
model does not answer it.

```bash
curl -s http://127.0.0.1:8443/v1/systemone \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-decider",
       "state": "I was charged twice for my order last week.",
       "questions": {
         "route": {"type": "choice", "instructions": "Which team should handle this?",
                   "criteria": {"billing": "payments and refunds", "shipping": null, "technical": null}},
         "angry": {"type": "noul", "instructions": "Is the customer angry?"},
         "urgency": {"type": "score", "instructions": "How urgent is this?",
                     "criteria": ["can wait", "this week", "today"]}}}'
```

```text
{"answers":{"angry":{"noul":0.6226,"type":"noul"},"route":{"choice":"billing","confidence":0.9255,"probabilities":{"billing":0.9503,"shipping":0.0157,"technical":0.0339},"type":"choice"},"urgency":{"confidence":0.48,"legend":{"0":"can wait","1":"this week","2":"today"},"probabilities":{"0":0.08,"1":0.71,"2":0.21},"score":1.1224,"type":"score"}},"capyctl":{"metrics":{"input_tokens":{"source":"engine","value":87},"time_to_response_ms":{"source":"router","value":41.2},"total_ms":{"source":"router","value":43.9}}},"model":"my-decider","usage":{"input_tokens":87,"output_tokens":0}}
```

The probabilities come from your model; these are an example, and they can
move slightly between identical requests. The request takes these fields and
no others:

- `model`: the deployment's name.
- `state`: the input, as text, a JSON object, or an array of chat messages.
- `questions`: one or more questions, keyed by an id you choose. Each has a
  `type` (`noul` for yes or no, `choice` for one of named options, `score`
  for an ordered scale), `instructions`, and `criteria`: for `choice` an
  object of option to description, for `noul` optional `true` and `false`
  descriptions, for `score` a list of levels from low to high.
- `images`, `files`: lists of strings (data URIs), for models that take them.
  With several machines the whole body must fit in 8 MiB.

Any other field (an engine's own extension included) is answered
`400 invalid_request` before the model is woken; the engine judges the rest
(how many questions or options, required `instructions`) and its refusal comes
back as `engine_rejected` with its status. A decision is never streamed. The
answer is the engine's, with `model` set to the name you sent and its `usage`
as the engine reported it, plus the request's figures in `capyctl.metrics`
(see [Request IDs and metrics](#request-ids-and-metrics)). A parked or stopped
decision model wakes or starts for the request, as a chat model does.

A decision request carries no `cache_salt`. On SGLang a decision model runs
without [LMCache](engines.md#lmcache) for now, and SGLang keeps no prefix
cache for it either (it disables its radix cache for these models), so a
repeated state is computed again
([sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929)).

Sent to the other endpoint, a request is answered `400 api_mismatch` without
waking anything; the message names the endpoint to use:

```text
{"code":"api_mismatch","message":"model my-decider serves the decisions API, which /v1/chat/completions does not answer; send its requests to POST /v1/systemone"}
```

## Embedding requests

A deployment with `api: embeddings` ([Embedding models](deploy.md#embedding-models))
turns text into vectors, on `POST /v1/embeddings` (OpenAI's Embeddings API).
It answers nothing else, and a chat or decision model does not answer it.

```bash
curl -s http://127.0.0.1:8443/v1/embeddings \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-embedder", "input": ["a quiet library", "tall shelves of old books"]}'
```

```text
{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.0213,-0.0087,0.0441]},{"object":"embedding","index":1,"embedding":[0.0179,-0.0122,0.0398]}],"model":"my-embedder","usage":{"prompt_tokens":9,"total_tokens":9},"capyctl":{"metrics":{"inputs":{"source":"router","value":2},"input_tokens":{"source":"engine","value":9},"time_to_response_ms":{"source":"router","value":18.6},"total_ms":{"source":"router","value":20.1}}}}
```

The vectors come from your model; these are shortened to three numbers, and a
real one has the model's dimension (2,560 for Qwen3-Embedding-4B). The request
takes these fields and no others:

- `model`: the deployment's name.
- `input`: a non-empty string, an array of 1 to 2048 non-empty strings, an
  array of token IDs, or an array of 1 to 2048 non-empty arrays of token IDs.
  A token ID is a whole number from 0 to 2147483647. An array of strings and
  token arrays mixed together is refused.
- `encoding_format`: `float` (the default) or `base64`. With `base64` each
  vector is a string of little-endian float32 values; the OpenAI Python
  client asks for it when you name no format and decodes it for you.
- `dimensions`: a positive whole number, for a model that can shorten its
  vectors. vLLM and SGLang judge it against the model; llama.cpp always
  answers the full vector, so on llama.cpp CapyCTL refuses any `dimensions`
  other than the model's own.
- `user`: a string, passed to the engine.

Any other field is answered `400 invalid_request` naming it, before the model
is woken: `stream`, `cache_salt`, and each engine's own extensions (vLLM's
`messages` or `truncate_prompt_tokens`, SGLang's `rid`, llama.cpp's
`content` or `embd_normalize`, images). For example:

```text
{"code":"invalid_request","message":"embeddings request field `stream` is not accepted"}
{"code":"invalid_request","message":"malformed embeddings request: input holds more than 2048 inputs"}
{"code":"invalid_request","message":"dimensions must be 2560: llama.cpp returns full vectors of the model's native dimension"}
```

CapyCTL does not tokenize, truncate or normalize your inputs. The engine
judges the rest, such as an input longer than the model takes or a
`dimensions` the model does not support, and its refusal comes back as
`engine_rejected` with its status. A parked or stopped embedding model wakes
or starts for the request, as a chat model does.

An embedding request carries no `cache_salt`. Where the engine keeps a prefix
cache for an embedding model, every caller of that deployment shares it.

Sent to another endpoint, a request is answered `400 api_mismatch` without
waking anything; the message names the endpoint to use:

```text
{"code":"api_mismatch","message":"model my-embedder serves the embeddings API, which /v1/chat/completions does not answer; send its requests to POST /v1/embeddings"}
```

### The answer arrives as it is made

A batch of 2048 long vectors can be hundreds of megabytes of JSON, so CapyCTL
does not wait for the whole answer and sets no limit on its size. It sends
the engine's answer on to you as it arrives, without a `Content-Length`, and
checks it on the way: one entry per input, each index once, every vector the
same length (and the length you asked for in `dimensions`), only finite
numbers, and well-formed `base64`. It replaces the `model` the engine wrote
with the name you sent; nothing else is changed, so vLLM's `id` and
`created` stay and the other engines' answers have neither. The answer's
closing brace, after `capyctl.metrics`, is written only once the last check
has passed.

So a complete JSON document always means an answer that passed. If the
engine's answer turns out malformed, or the engine closes the connection
before it ends, CapyCTL stops sending and ends the response there: you have
already received `200` and part of the body, and the body is incomplete JSON
that no JSON parser accepts. Treat that as a failed request. CapyCTL never
sends it again on its own, since the engine may have run it. An engine that
refuses the request before answering is the ordinary `engine_rejected` or
`engine_error`, with no partial body.

### When an embedding request is cut short

An embedding request is short and bounded, so a cut one is not stopped. If
your client hangs up, or the request runs past its deadline, CapyCTL keeps
the engine connection open, lets the engine finish, and reads and checks the
rest of the answer without sending it anywhere. The request counts against
its model's memory and its 32 places until then, and is finished as soon as
a complete answer has been read. Only a cut request whose answer never
completes (the engine closes the connection, the answer is malformed, or the
instance stops first) waits for the instance's next stop, as described in
[When a request is cut short](#when-a-request-is-cut-short), including the
restart when such requests fill the model's 32 places. CapyCTL never sends
its own request ID to the engine for an embedding request, on any engine.

## Request IDs and metrics

Every answer from `/v1/chat/completions`, `/v1/systemone` and
`/v1/embeddings`, success or error, carries CapyCTL's ID for the request in
the `X-CapyCTL-Request-Id`
header. It is the ID of the record CapyCTL keeps of the request while an
engine works on it, so it is the ID to quote when you look
for the request in CapyCTL's logs. A request refused before it got that far
(a bad key, an invalid body, an unknown model, a full queue) gets a fresh
one instead, so every answer has one.

You can send your own ID in `X-Request-Id` and CapyCTL hands it back on the
answer, in the response headers of a stream too:

```bash
curl -si http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -H 'X-Request-Id: checkout-7f3a' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}' \
  | grep -i 'request-id'
```

```text
x-capyctl-request-id: 01K7ZQ3M4X9V2B8N6C5D1F0G7H
x-request-id: checkout-7f3a
```

Your ID must be 1 to 128 characters of printable ASCII with no spaces;
anything else, or the header given twice, is answered `400 invalid_request`
before the model is woken. CapyCTL writes it beside its own ID in its logs
for the request and nowhere else: it is never sent to the engine, and never
stored in CapyCTL's database or events. The same holds when the model runs on
another machine behind its host's ingress. In the default text output a
router line reads

```text
14:02:11 request 01K7ZQ3M4X9V2B8N6C5D1F0G7H (caller checkout-7f3a) 01K7ZPXQ2M8A -> gpu-box instance 0
```

and in JSON output the `router_selection` and `router_failover` events carry
`capyctl_request_id` and `caller_request_id`.
A request without your ID shows only CapyCTL's.

A decision's answer carries its figures in `capyctl.metrics`, in the same
`{"value": ..., "source": ...}` form as a chat answer's
([Per-request metrics](#per-request-metrics)):

| Figure | What it measures |
|---|---|
| `total_ms` | router: from when CapyCTL received the request to the whole answer, waiting and waking included |
| `time_to_response_ms` | router: from when CapyCTL sent the request to the engine to the whole answer |
| `input_tokens` | engine: the answer's `usage.input_tokens` |
| `queue_ms`, `prefill_ms`, `cached_tokens` | engine, when the engine reported them, read as for chat |

A decision generates no text, so it has no `ttft_ms` or
`decode_tokens_per_second`. A figure nobody measured is left out.

An embedding answer carries its figures in `capyctl.metrics` too, just
before its closing brace:

| Figure | What it measures |
|---|---|
| `total_ms` | router: from when CapyCTL received the request to the whole answer, waiting and waking included |
| `time_to_response_ms` | router: from when CapyCTL sent the request to the engine to the whole answer |
| `input_tokens` | engine: the answer's `usage.prompt_tokens`, when the engine sent it |
| `inputs` | router: the number of inputs in the request |

An embedding generates no text, so it has no `ttft_ms`, decode speed,
`queue_ms`, `prefill_ms` or `cached_tokens`. An answer that failed its checks
has no `capyctl.metrics`, since it never reaches its closing brace.

## When the model is parked

A request for a parked model waits while CapyCTL wakes it, then answers. It
takes longer than usual; nothing else changes for the client. See
[How requests wait and models switch](parking.md#how-requests-wait-and-models-switch).

## The first request after a new engine install

The first request an engine serves after it is installed or upgraded can take
a minute or more, even when the deployment is `ready`. SGLang, in particular,
compiles some GPU kernels the first time they run (one laptop RTX 4090 took
97 s for its first request, then under 0.3 s). The engine caches the build, so
later requests and later launches are fast. Use a client timeout of a few
minutes for that first request.

## From another machine

Use the machine's name, LAN address or Tailscale address instead of
`127.0.0.1`, with the same key:

```bash
curl -s http://gpu-box:8443/v1/models -H "Authorization: Bearer $KEY"
```

In an OpenAI client, set the base URL to `http://gpu-box:8443/v1`.

To keep the endpoint on the machine itself, start CapyCTL with
`--listen 127.0.0.1:8443`; to limit it to your tailnet, use the machine's
Tailscale address there. The endpoint is plain HTTP: for the internet, put a
TLS reverse proxy in front. [Network access](../operations/network-access.md)
covers each of these.
