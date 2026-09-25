# mllm

A model manager for vLLM and SGLang on your own GPUs.

Most GPU machines can hold one or two models at a time. mllm runs your
inference engines for you, parks the models nobody is using so their GPU
memory is released, and wakes or switches to the one a request asks for. It
puts one OpenAI-compatible endpoint in front of every machine you enroll, and
it decides where each model runs while tracking the memory it hands out.
Engines listen on loopback only; hosts talk to the server over gRPC with
mutual TLS.

## Status

0.1.0 is at the release-candidate stage. Release candidates are published as
GitHub pre-releases; expect breaking changes before 0.1.0.

What works in the release candidates:

- vLLM and SGLang engines, one GPU per model.
- One machine (standalone) or a server with several GPU hosts.
- Parking, waking and switching models under a memory budget.
- An OpenAI-compatible API (`/v1/models`, `/v1/chat/completions`), with
  streaming and tool calls.
- Linux on x86-64 and ARM64, NVIDIA GPUs.

Not there yet:

- Multi-GPU models (tensor parallelism across GPUs or machines) are designed
  but parked until after 0.1.0.
- Downloading model sources (Hugging Face, HTTP) works on enrolled hosts but
  not in standalone.
- No web UI; everything goes through the CLI and the management API.
- Other GPU vendors and operating systems are not supported.
- Deep parking relies on engine development controls. mllm keeps them on
  loopback behind a per-launch key, but they are not production-hardened;
  a host can opt out (see SPEC §9.1 and ADR 0012).

mllm does not install engines, drivers or model weights. Bring your own vLLM
or SGLang environment and your own checkpoints.

## Install

A release is one self-contained binary per architecture (Linux x86-64 and
ARM64); mllm's Python runtime helpers are compiled into it. The repository is
private for now, so use a logged-in `gh` (`gh auth login`) or set
`GITHUB_TOKEN`. Release candidates are pre-releases, which GitHub's "latest
release" skips, so always pass `--version`:

```bash
gh release download v0.1.0-rc.4 -R edurdias/mllm -p install.sh
sh install.sh --version v0.1.0-rc.4         # installs ~/.local/bin/mllm
# sudo sh install.sh --system ...           # /usr/local/bin/mllm
# add --systemd <server|host|standalone> to install that role's unit
```

The installer checks every download against the release's `SHA256SUMS` and
refuses on a mismatch. See
[`docs/operations/install.md`](docs/operations/install.md) for services,
upgrades and rollback.

## Quickstart: one machine

Standalone runs the server and one host in a single process, with every
listener on loopback. It uses one engine installation, vLLM or SGLang, taken
from the environment:

```bash
export MLLM_VLLM_BIN=/path/to/venv/bin/vllm   # or MLLM_SGLANG_BIN
export MLLM_MODELS_ROOT=/srv/models
mllm start standalone                          # inference on 127.0.0.1:8443
```

The first start writes its role document and credentials under
`~/.local/state/mllm`. In another shell, write a deployment document for a
model under `MLLM_MODELS_ROOT` (start from
[`docs/examples/deployment-single.yaml`](docs/examples/deployment-single.yaml);
in standalone the engine installation, `runtime_profile`, is named `local`),
then deploy it and check on it:

```bash
mllm validate config --file deployment.yaml
mllm deploy model --file deployment.yaml --activate --wait
mllm list deployments
mllm status deployment <name>
```

Commands that read records (`list`, `status`, `engine list`, `engine detect`)
print an aligned table, whether or not the output is a terminal:

```text
$ mllm list deployments
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
qwen3-8b   model   ready     ready   1/1     1          workstation
```

For scripts, `--format json` (or `--json`) prints the full JSON result
instead, including the detail a table leaves out, and makes errors JSON too:

```bash
mllm list deployments --format json | jq -r '.[].name'
```

Send a request to the route the deployment names. The inference API key is in
the credentials file the first start wrote:

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)
curl http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "<route>", "messages": [{"role": "user", "content": "Hello"}]}'
```

`mllm stop deployment <name>` stops the model and releases its memory;
`mllm start deployment <name>` brings it back. `mllm delete deployment <name> --stop`
stops and removes it.

## Several machines

One machine runs the server; each GPU machine runs a host. The outline below
uses `gpu-box` as the host name. The full setup, including system services
and file locations, is in
[`docs/operations/install.md`](docs/operations/install.md).

On the server machine:

```bash
mllm init server --output server.yaml
# edit server.yaml: listeners and enrollment addresses (docs/examples/server.yaml)
mllm validate config --file server.yaml
mllm start server --config server.yaml
mllm invite host gpu-box --output gpu-box.join --config server.yaml
```

Copy `gpu-box.join` to the GPU machine, then:

```bash
mllm init host --output host.yaml
# edit host.yaml: name, model store, resource policy and engine
# installations (docs/examples/host.yaml)
mllm validate config --file host.yaml
mllm join host --join-file gpu-box.join --config host.yaml
mllm start host --config host.yaml
```

Back on the server machine, deploy through the server:

```bash
mllm list hosts --config server.yaml
mllm deploy model --file deployment.yaml --activate --wait --config server.yaml
mllm list deployments --config server.yaml
mllm park deployment <name> --config server.yaml
```

```text
$ mllm list hosts --config server.yaml
NAME      STATE    ELIGIBLE   VERSION      COMPATIBILITY   MEMORY (FREE / TOTAL)   ENGINES
gpu-box   online   yes        0.1.0-rc.4   supported       88.3 GiB / 119.7 GiB    vllm,sglang

$ mllm list deployments --config server.yaml
NAME           KIND    DESIRED   STATE    READY   REVISION   HOSTS
qwen3-8b       model   ready     ready    1/1     1          gpu-box
llama-3.1-8b   model   parked    parked   0/1     2          gpu-box
```

A parked deployment releases GPU memory (its weights and KV cache with
`residency: deep`) and wakes on the next request for its route. When a request needs a model and
there is no room, mllm parks or stops an idle one to make space.

Upgrade the server first, then the hosts one at a time.

## Documentation

- [`docs/operations/install.md`](docs/operations/install.md): install,
  services, upgrades, rollback and exit codes.
- [`docs/examples/`](docs/examples/): example server, host, standalone and
  deployment documents. A test checks that `mllm validate config` accepts
  them; they show the schema and are not tested engine recipes.
- [`site/`](site/): the project website, built from these documents.
- [`docs/SPEC.md`](docs/SPEC.md): the authoritative requirements and
  architecture.
- [`docs/design/adr/`](docs/design/adr/): architecture decision records.
- [`docs/runbooks/f2-current-status.md`](docs/runbooks/f2-current-status.md):
  the contributors' record of what is done and what is pending.

## Contributing

Read [`AGENTS.md`](AGENTS.md) first. It is the working agreement for human and
coding-agent contributors: which documents are authoritative, the
verification a change must pass, and the documentation rules.

## License

Apache-2.0 (see [ADR 0006](docs/design/adr/0006-license.md)). The `LICENSE`
file is added before the repository is published.
