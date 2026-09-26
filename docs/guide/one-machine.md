# Run on one machine

`mllm start standalone` runs everything on one machine, in one process.

You need mllm ([Install](install.md)), a vLLM or SGLang installation (here
`~/venvs/vllm`) and a model: a checkpoint directory in `~/models` (here
`~/models/Qwen3-4B`), or a Hugging Face repository.

## 1. Add your engine

```bash
mllm engine add ~/venvs/vllm
```

```text
saved to /home/me/.config/mllm/engines.yaml (revision 1); start mllm (`mllm start standalone`) to use it
```

The line comes after a JSON record of the engine. mllm is not running yet, so
it saves the engine for the first start. More in [Add an engine](engines.md).

## 2. Start

```bash
mllm start standalone
```

```text
standalone ready (state_dir /home/me/.local/state/mllm; inference listener 0.0.0.0:8443; credentials /home/me/.local/state/mllm/identity/credentials)
```

mllm reads your GPU, creates `~/models` if it is missing, and writes an API key
to the credentials file. The endpoint listens on port 8443 of every interface
and answers only requests that carry the key. Leave mllm running and open a
second terminal.

## 3. Deploy a model

Save this as `my-model.yaml`:

```yaml title="my-model.yaml"
name: my-model
engine: vllm
model: Qwen3-4B
```

`model` is a directory under `~/models`. To download from Hugging Face
instead, write `model: {hf: Qwen/Qwen3-4B-Instruct-2507}`. mllm sizes the
memory from the checkpoint and your GPU; [Deploy a model](deploy.md) has
the details.

```bash
mllm deploy model --file my-model.yaml --activate --wait
mllm list deployments
```

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
my-model   model   ready     ready   1/1     1          gpu-box
```

The first start of a new checkpoint takes longer: mllm reads the files once
to fingerprint them, then starts the engine.

## 4. Send a request

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

```text
{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"Hello! How can I help you today?","role":"assistant"}}],"created":1790466091,"id":"chatcmpl-1","model":"my-model","object":"chat.completion"}
```

From another computer, use this machine's address instead of `127.0.0.1`,
with the same key.

That's it: one model behind an OpenAI-compatible endpoint.

## Next

- [Make a request](requests.md): streaming, the Python client, other machines.
- [Parking and switching](parking.md): more models than your GPU holds.
- [Run on several machines](several-machines.md).
