# Run on one machine

`capyctl start standalone` runs everything on one machine, in one process.

You need CapyCTL ([Install](install.md)), a vLLM, SGLang, TensorFold or llama.cpp installation (here
`~/venvs/vllm`; [Install an engine](install-engines.md) shows how) and a model: a checkpoint directory in `~/models` (here
`~/models/Qwen3-4B`), or a Hugging Face repository.

## 1. Add your engine

```bash
capyctl engine add ~/venvs/vllm
```

```text
Registered vllm (vllm 0.29.0)

  Executable     /home/me/venvs/vllm/bin/vllm
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 1)
  Published      when capyctl starts
saved to /home/me/.config/capyctl/engines.yaml (revision 1); start capyctl (`capyctl start standalone`) to use it
no capyctl answered on /home/me/.local/state/capyctl/control.sock; if it is already running with another --state-dir or --config, restart it to publish this profile, and pass the same option to `capyctl engine`
```

CapyCTL is not running yet, so it saves the engine for the first start. More in
[Add an engine](engines.md).

## 2. Start

```bash
capyctl start standalone
```

```text
capyctl 0.1.2 standalone ready

  Inference     0.0.0.0:8443 (API key required)
  Management    127.0.0.1:7443
  Engines       vllm
  State         /home/me/.local/state/capyctl
  Credentials   /home/me/.local/state/capyctl/identity/credentials
```

On a terminal CapyCTL prints this text. Started as a service, or with its output
piped to a file, it prints one JSON object per line instead; add `--format
text` or `--format json` to choose.

CapyCTL reads your GPU, creates `~/models` if it is missing, and writes an API key
to the credentials file. The endpoint listens on port 8443 of every interface
and answers only requests that carry the key. Leave CapyCTL running and open a
second terminal.

## 3. Deploy a model

Save this as `my-model.yaml`:

```yaml title="my-model.yaml"
name: my-model
engine: vllm
model: Qwen3-4B
```

`model` is a directory under `~/models`. To download from Hugging Face
instead, write `model: {hf: Qwen/Qwen3-4B-Instruct-2507}`. CapyCTL sizes the
memory from the checkpoint and your GPU; [Deploy a model](deploy.md) has
the details.

```bash
capyctl deploy model --file my-model.yaml --activate --wait
```

```text
Request identity: 01M3R78B47ANFBFVFY40HATPYJ (reuse --request-id 01M3R78B47ANFBFVFY40HATPYJ to recover this command)
Waiting for the checkpoint digest of my-model to be measured (at most 900s)
Deployed my-model: ready

  Revision    1
  Hosts       gpu-box
  Ready       1/1
  Startup     28.5 GiB
  Context     26752 tokens
  Operation   initialize succeeded
```

```bash
capyctl list deployments
```

```text
NAME       STATE   READY   REVISION   HOSTS
my-model   ready   1/1     1          gpu-box
```

The first start of a new checkpoint takes longer: CapyCTL reads the files once
to fingerprint them, then starts the engine.

## 4. Send a request

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/capyctl/identity/credentials)
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

## See also

- [First model on each engine](engines.md#first-model-on-each-engine): vLLM, SGLang and TensorFold, with their deployment files.
- [Make a request](requests.md): streaming, the Python client, other machines.
- [Parking and switching](parking.md): more models than your GPU holds.
- [Run on several machines](several-machines.md).
