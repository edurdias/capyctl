# Quickstart: one machine

This runs mllm standalone: the server and one host in a single process, with
every listener on loopback. You need Linux with an NVIDIA GPU, a working vLLM
or SGLang installation, and a model checkpoint on disk.

## 1. Install

Follow [Install](../operations/install.md#installing-with-installsh), then check:

```bash
mllm --version
```

## 2. Start standalone

Point mllm at your engine and your models directory, then start it:

```bash
export MLLM_VLLM_BIN=/path/to/venv/bin/vllm   # or MLLM_SGLANG_BIN for SGLang
export MLLM_MODELS_ROOT=/srv/models
mllm start standalone
```

The OpenAI-compatible endpoint is `http://127.0.0.1:8443/v1`. The first start
writes its configuration and credentials under `~/.local/state/mllm`. Leave it
running and use another shell for the rest.

## 3. Deploy a model

Write `deployment.yaml` for a checkpoint under `MLLM_MODELS_ROOT`. Start from
the [single-host example](configuration.md#deployment-on-one-host); in
standalone the engine installation (`runtime_profile`) is called `local`.

```bash
mllm validate config --file deployment.yaml
mllm deploy model --file deployment.yaml --activate --wait
mllm list deployments
```

## 4. Send a request

The inference API key is in the credentials file the first start wrote:

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)
curl http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "<route>", "messages": [{"role": "user", "content": "Hello"}]}'
```

`<route>` is one of the `routes` in your deployment document.

## 5. Park and wake

Park the model. With `residency: deep` its weights and KV cache leave GPU memory:

```bash
mllm park deployment <name>
mllm status deployment <name>      # state: parked
```

Send the same request again. mllm wakes the model and answers; the request
waits while it wakes. `mllm status deployment <name>` shows `ready` again.

## 6. Clean up

```bash
mllm stop deployment <name>             # stops the engine, keeps the deployment
mllm delete deployment <name> --stop    # stops and removes it
```

Next: [Concepts](concepts.md), or add machines with
[Multiple machines](multiple-machines.md).
