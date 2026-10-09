# Make a request

CapyCTL serves the OpenAI chat API on port 8443 of the machine that runs it (the
server, with several machines), on every interface, with an API key. On that
machine the endpoint is `http://127.0.0.1:8443/v1`. Send the deployment's
`name` as `model`.

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
{"data":[{"id":"my-model","object":"model"}],"object":"list"}
```

Listing never wakes a model. A request without the key, or with a wrong one,
is answered `401`.

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

| Field | Where | vLLM 0.29, 0.30 | SGLang 0.5.18–0.5.21 | TensorFold 0.6.x | llama.cpp v0.6.0 |
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
was confirmed live, and the other listed versions are assumed to match until
run against them.

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

| Figure | vLLM 0.30 | SGLang 0.5.21 | TensorFold 0.6.x | llama.cpp v0.6.0 |
|---|---|---|---|---|
| `ttft_ms` | engine: queue plus prompt | router | engine, from when it received the request | router |
| `queue_ms` | engine | — | — | — |
| `prefill_ms` | engine, from when it scheduled the request | — | engine, from when it queued the request | engine (`timings.prompt_ms`, from when a slot took the request) |
| `decode_tokens_per_second` | engine, from its mean time between tokens | router | engine; router when it timed no decode | engine (`timings.predicted_per_second`); router when it timed no decode |
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
begins the same way. When several tenants share one model, give each its own
`cache_salt`: a request then reuses cached prefixes only from requests that
carry the same salt.

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "cache_salt": "tenant-a",
       "messages": [{"role": "user", "content": "Hello"}]}'
```

CapyCTL passes the salt to vLLM and SGLang unchanged. It must be a non-empty
string of at most 1024 bytes; anything else is refused with
`400 invalid_request`.

TensorFold ignores the field, so a request with a `cache_salt` for a model that
runs on TensorFold is refused rather than served without the isolation it asks
for. The answer is `400` with code `cache_salt_unsupported` (on a stream, an
error event with that code), and nothing reaches the engine. The same request
without `cache_salt` is served.

```text
{"code":"cache_salt_unsupported","message":"this deployment's engine does not partition its prefix cache by cache_salt"}
```

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
