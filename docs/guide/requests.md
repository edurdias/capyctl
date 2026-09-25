# Make a request

mllm serves the OpenAI chat API. On one machine the endpoint is
`http://127.0.0.1:8443/v1`. Send the deployment's `name` as `model`.

## The API key

The first start wrote it to the credentials file:

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)
```

On several machines the key is on the server machine, in
`~/.local/state/mllm/identity/server-credentials.json`:

```bash
KEY=$(sed -n 's/.*"api_key": *"\([^"]*\)".*/\1/p' ~/.local/state/mllm/identity/server-credentials.json)
```

## List models

```bash
curl -s http://127.0.0.1:8443/v1/models -H "Authorization: Bearer $KEY"
```

```text
{"data":[{"id":"my-model","object":"model"}],"object":"list"}
```

Listing never wakes a model.

## Chat

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

```text
{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"Hello! How can I help you today?","role":"assistant"}}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion"}
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
data: {"choices":[{"delta":{"content":"","role":"assistant"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":"Hello!"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" How"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" can"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" I"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" help"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" you"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{"content":" today?"},"finish_reason":null,"index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion.chunk"}

data: [DONE]
```

## Python

With the `openai` package (`pip install openai`):

```python title="client.py"
from openai import OpenAI
from pathlib import Path

key = next(line.split(": ", 1)[1] for line in
           (Path.home() / ".local/state/mllm/identity/credentials").read_text().splitlines()
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

A request for a parked model waits while mllm wakes it, then answers. It
takes longer than usual; nothing else changes for the client. See
[Parking and switching](parking.md).

## From another machine

The endpoint listens on `127.0.0.1` of the machine that runs mllm (the
server, with several machines). To use it from another computer, forward the
port, for example over SSH:

```bash
ssh -N -L 8443:127.0.0.1:8443 you@mllm-server
```

Then use `http://127.0.0.1:8443/v1` on that computer.
