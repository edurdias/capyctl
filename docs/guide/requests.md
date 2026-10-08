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
{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"Hello! How can I help you today?","role":"assistant"}}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion"}
```

The answer text comes from your model; this one is an example.

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

data: [DONE]
```

### Streamed fields

CapyCTL relays each chunk unchanged except `model`, and checks it first: a
`delta` key not in this table ends the stream as unverified, so an untested
field never reaches a client. Fields beside `delta` in a
choice, and `usage` with its details, are relayed as the engine sent them.

| Field | Where | vLLM 0.29, 0.30 | SGLang 0.5.18–0.5.21 | TensorFold 0.6.x |
|---|---|---|---|---|
| `role` | delta | yes | yes (may be `null`) | yes |
| `content` | delta | yes | yes | yes |
| `reasoning` | delta | yes | — | — |
| `reasoning_content` | delta | — | yes | yes |
| `tool_calls` | delta | yes | yes | yes |
| `index`, `finish_reason`, `logprobs` | choice | yes | yes | `index`, `finish_reason` |
| `stop_reason`, `token_ids` | choice | yes | — | — |
| `matched_stop` | choice | — | yes | — |
| `prompt_tokens`, `completion_tokens`, `total_tokens` | usage | yes | yes | yes |
| `prompt_tokens_details.cached_tokens` | usage | yes | yes (`--enable-cache-report`) | yes |
| `completion_tokens_details.reasoning_tokens` | usage | yes | — | yes |
| `reasoning_tokens` | usage | — | yes | — |
| `exact_mode`, `tensorfold`, `speculative` | chunk | — | — | yes (finish chunk) |

Not relayed: SGLang's `hidden_states` delta (sent only for
`return_hidden_states`, a request field CapyCTL refuses) and vLLM's `citations`
delta (its Cohere endpoint only). The table was read from the vLLM 0.29.0,
SGLang 0.5.20 and TensorFold 0.6.0 and 0.6.3 sources; vLLM 0.30's `reasoning`
was confirmed live, and the other listed versions are assumed to match until
run against them.

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
