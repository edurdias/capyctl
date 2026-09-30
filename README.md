# mllm

A model manager for vLLM and SGLang on your own GPUs.

Most GPU machines can hold one or two models at a time. mllm runs your
inference engines for you, parks the models nobody is using so their GPU
memory is released, and wakes or switches to the one a request asks for. It
puts one OpenAI-compatible endpoint in front of every machine you enroll, and
it decides where each model runs while tracking the memory it hands out.
Engines listen on loopback only; hosts talk to the server over gRPC with
mutual TLS. [How it works](docs/guide/how-it-works.md) has a picture.

## Status

0.1.0 is the first release. It is early software: expect rough edges and
breaking changes between minor versions while the version starts with 0.

What works in 0.1.0:

- vLLM and SGLang engines, one GPU per model, on unified-memory machines and
  on discrete NVIDIA cards, where mllm counts the card's memory apart from
  host RAM and picks the GPU on a machine with several.
- One machine (standalone) or a server with several GPU hosts.
- Parking, waking and switching models under a memory budget; on a discrete
  card, parking into host RAM.
- An OpenAI-compatible API (`/v1/models`, `/v1/chat/completions`), with
  streaming and tool calls, on the network with an API key.
- Linux on x86-64 and ARM64, NVIDIA GPUs.

Not there yet:

- Multi-GPU models (tensor parallelism across GPUs or machines) are designed
  but parked until after 0.1.0.
- No web UI; everything goes through the CLI and the management API.
- Other GPU vendors and operating systems are not supported.
- Parking relies on engine development controls. mllm keeps them on loopback
  behind a per-launch key, but they are not production-hardened; a host can
  opt out (`--deep-park off`, see the
  [settings reference](docs/operations/configuration.md#engine-installation)).

mllm does not install engines or GPU drivers. Bring your own vLLM or SGLang
environment; models come from a directory or from Hugging Face.

## Install

A release is one self-contained binary per architecture (Linux x86-64 and
ARM64). Install the latest release:

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh
mllm --version
```

The installer checks every download against the release's `SHA256SUMS` and
refuses on a mismatch. To install a particular release, add
`-s -- --version v0.1.0`. See [`docs/guide/install.md`](docs/guide/install.md)
to get started, and [`docs/operations/install.md`](docs/operations/install.md)
for services, upgrades and rollback. Building from source needs stable Rust
and `protoc` (`sudo apt install protobuf-compiler`), on ARM64 too:
`cargo build --release --locked -p mllm-cli`.

## Quickstart: one machine

Standalone runs the server and one host in a single process. Register the
engine you have, then start:

```bash
mllm engine add ~/venvs/vllm   # a vLLM or SGLang environment
mllm start standalone          # inference on 0.0.0.0:8443, API key required
```

```text
mllm 0.1.0 standalone ready

  Inference     0.0.0.0:8443 (API key required)
  Management    127.0.0.1:7443
  State         /home/me/.local/state/mllm
  Credentials   /home/me/.local/state/mllm/identity/credentials
```

The first start writes its configuration and an API key under
`~/.local/state/mllm`, and reads the GPU. Models live in `~/models`. Every
setting can be given in the YAML document, as a flag or as an environment
variable (a flag wins over a variable, which wins over the document);
`--set path=value` changes any setting for one run, and `mllm config show`
prints each value and where it came from
([`docs/operations/configuration.md`](docs/operations/configuration.md)).

In another shell, write a deployment. Three fields are enough:

```yaml
name: my-model
engine: vllm                 # or sglang, or a name from `mllm engine list`
model: Qwen3-4B              # a directory under ~/models, an absolute path,
                             # or {hf: Qwen/Qwen3-4B-Instruct-2507}
```

mllm fills in the rest: the route is the name, the engine's memory is sized
from the checkpoint and the GPU, the GPU is picked, and the park tier follows
the hardware. A Hugging Face repository is pinned to the commit it names when
you deploy, then downloaded into `~/models/sources` (500 GiB cap for all
downloads). Every other field of
[`docs/examples/deployment-single.yaml`](docs/examples/deployment-single.yaml)
may be added to override a default.

```bash
mllm deploy model --file my-model.yaml --activate --wait
mllm list deployments
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

NAME       STATE   READY   REVISION   HOSTS
my-model   ready   1/1     1          gpu-box
```

Commands print text: a table for `list`, `status`, `engine list`,
`engine detect` and `config show`, and a short summary for commands that change
something. On a terminal, `mllm start` prints text too. For scripts,
`--json` (or `--format json`) prints the full JSON result instead, and makes
errors JSON too; a role's output is JSON whenever it is not a terminal.

Send a request with the API key:

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)
curl http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

From another machine, use this machine's name or Tailscale address instead
of `127.0.0.1`. To keep inference on this machine only, start with
`--listen 127.0.0.1:8443`; to narrow it to a tailnet or put it behind a TLS
proxy, see [`docs/operations/network-access.md`](docs/operations/network-access.md).

`mllm park deployment my-model` frees its GPU memory and the next request
wakes it; `mllm stop deployment my-model` stops it;
`mllm delete deployment my-model --stop` removes it.

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
mllm invite host gpu-box --output gpu-box.join
```

Copy `gpu-box.join` to the GPU machine (`scp -p` keeps its 0600 mode), then:

```bash
mllm init host --output host.yaml
# host.yaml validates as written (limits derived from this machine, models
# in ~/models); edit its name and ingress (docs/examples/host.yaml)
mllm validate config --file host.yaml
mllm join host --join-file gpu-box.join --config host.yaml
mllm start host --config host.yaml
mllm engine add ~/venvs/vllm   # in another shell
```

Back on the server machine, deploy through the server. A command run on a
machine uses the role running there, with no `--config`: on the server machine
the server, on a standalone machine the standalone role. On a host machine the
host's own commands (`mllm engine`, `mllm config show`) use the host, and a
command that needs the server says to run it on the server (see
[`docs/operations/configuration.md`](docs/operations/configuration.md#which-role-a-command-uses)).

```bash
mllm list hosts
mllm deploy model --file deployment.yaml --activate --wait
mllm list deployments
mllm park deployment <name>
```

```text
$ mllm list hosts
NAME      STATE    ELIGIBLE   VERSION   COMPATIBILITY   MEMORY (FREE / TOTAL)   ENGINES
gpu-box   online   yes        0.1.0     supported       46.5 GiB / 77.2 GiB     vllm

$ mllm list deployments
NAME           STATE    READY   REVISION   HOSTS
qwen3-8b       ready    1/1     1          gpu-box
llama-3.1-8b   parked   0/1     2          gpu-box
```

A parked deployment releases its GPU memory and wakes on the next request for
its route. When a request needs a model and
there is no room, mllm parks or stops an idle one to make space.

Upgrade the server first, then the hosts one at a time.

## Documentation

- [`docs/operations/install.md`](docs/operations/install.md): install,
  services, upgrades, rollback and exit codes.
- [`docs/operations/configuration.md`](docs/operations/configuration.md):
  every setting, with its YAML field, flag and environment variable, and the
  one precedence rule (`--set`, then `MLLM_SET__…`, then the named flag, then
  the named variable, then YAML, then the default).
- [`docs/operations/network-access.md`](docs/operations/network-access.md):
  the inference endpoint on your network, its API key, and a TLS proxy.
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

Start with [CONTRIBUTING.md](CONTRIBUTING.md) for setup, tests and pull requests.
Use [SUPPORT.md](SUPPORT.md) for questions, bug reports and feature feedback, and
[SECURITY.md](SECURITY.md) for private vulnerability reporting. Everyone
participating follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Licensed under [Apache-2.0](LICENSE); see [ADR 0006](docs/design/adr/0006-license.md).
