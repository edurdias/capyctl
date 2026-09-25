# Quickstart

One machine runs everything: `mllm start standalone` is the server and the
GPU host in one process, listening on `127.0.0.1` only. You need mllm
([Install](install.md)), a vLLM or SGLang installation, and a model checkpoint
on disk.

## 1. Point mllm at your engine and models

```bash
export MLLM_VLLM_BIN=~/venvs/vllm/bin/vllm      # SGLang: MLLM_SGLANG_BIN=~/venvs/sglang/bin/python3
export MLLM_MODELS_ROOT=~/models                # holds your checkpoint directories
```

## 2. Start

```bash
mllm start standalone
```

Leave it running and open another terminal. The OpenAI-compatible endpoint is
`http://127.0.0.1:8443/v1`.

If the start refuses because a directory above `~/.local/state/mllm` is
writable by other users, fix it with `chmod go-w ~ ~/.local ~/.local/state`.

## 3. Deploy a model

Save this file as `deployment-standalone.yaml`. Set `model.path` to your checkpoint's
directory under `MLLM_MODELS_ROOT`, and `request` to the GPU memory the model
may use.

<!-- include: ../examples/deployment-standalone.yaml -->

```bash
mllm deploy model --file deployment-standalone.yaml --activate --wait
mllm list deployments
```

`STATE` shows `ready` once the model is loaded.

## 4. Send a request

The API key is in the credentials file the first start wrote:

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)
curl http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

## 5. Park and wake

```bash
mllm park deployment my-model
mllm list deployments      # STATE: parked, DESIRED: ready
```

The model's GPU memory is free now. Send the same request again: mllm wakes
the model and then answers, so this first request takes longer.
`mllm list deployments` shows `ready` again.

When a request needs a model that does not fit, mllm parks or stops an idle
one to make room.

## 6. Clean up

```bash
mllm stop deployment my-model              # stops the engine, keeps the deployment
mllm delete deployment my-model --stop     # stops it and removes it
```

Next: [Several machines](several-machines.md).
