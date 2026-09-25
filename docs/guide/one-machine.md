# Run on one machine

`mllm start standalone` runs everything on one machine, in one process. It
listens on `127.0.0.1` only.

You need mllm ([Install](install.md)), a vLLM or SGLang installation (here
`~/venvs/vllm`) and a model checkpoint in a models directory (here
`~/models/Qwen3-4B`).

## 1. Add your engine

```bash
mllm engine add ~/venvs/vllm
```

```text
error [agent_unreachable]: /home/me/.local/state/mllm/control.sock: No such file or directory (os error 2); /home/me/.config/mllm/engines.yaml is written (revision 1); it takes effect when the role starts
```

mllm is not running yet, so it saves the engine for the next start. More in
[Add an engine](engines.md).

## 2. Start

```bash
MLLM_MODELS_ROOT=~/models mllm start standalone
```

```text
standalone ready (state_dir /home/me/.local/state/mllm; inference listener 127.0.0.1:8443)
```

`MLLM_MODELS_ROOT` is the directory that holds your checkpoints. Leave mllm
running and open a second terminal.

## 3. Deploy a model

Save this as `my-model.yaml`. Set `model.path` to your checkpoint's directory
under `~/models` and `request` to the GPU memory the model may use.
[Deploy a model](deploy.md) explains each field.

```yaml title="my-model.yaml"
schema_version: 1
kind: deployment
name: my-model
routes: ["my-model"]
runtime_profile: vllm
runtime_profile_revision: 1
recipe: standard
residency: deep
recovery: reconcile
model:
  path: Qwen3-4B
  content_fingerprint: "qwen3-4b-1"
  revision: "1"
devices:
  - id: gpu0
    sharing: shared
engine_config:
  memory:
    request: "16GiB"
```

```bash
mllm deploy model --file my-model.yaml
mllm start deployment my-model --wait
```

If the start says `checkpoint_digest_pending`, mllm is still reading the
checkpoint for the first time; run the start again in a moment.

```bash
mllm list deployments
```

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
my-model   model   ready     ready   1/1     1          gpu-box
```

## 4. Send a request

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

```text
{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"Hello! How can I help you today?","role":"assistant"}}],"created":1790354734,"id":"chatcmpl-1","model":"my-model","object":"chat.completion"}
```

That's it: one model behind an OpenAI-compatible endpoint.

## Next

- [Make a request](requests.md): streaming and the Python client.
- [Parking and switching](parking.md): more models than your GPU holds.
- [Run on several machines](several-machines.md).
