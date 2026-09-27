# mllm 0.1.0

The first release of mllm, a model manager for vLLM and SGLang on your own
NVIDIA GPUs. mllm runs your inference engines, parks the models nobody is using
so their memory is freed, and wakes or switches to the one a request asks for,
behind one OpenAI-compatible endpoint.

## Install

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh
mllm --version
```

One binary for Linux on x86-64 or ARM64. The installer checks every download
against the release's `SHA256SUMS`. Add `-s -- --version v0.1.0` to pin this
release. Bring your own vLLM or SGLang environment; mllm does not install
engines or drivers.

## What is in 0.1.0

- **One machine or several.** Run everything on one GPU machine (standalone),
  or run a server with several GPU hosts that join it over mutual TLS.
- **vLLM and SGLang.** Register an existing engine environment with
  `mllm engine add <venv>`; a deployment file needs only `name`, `engine` and
  `model`.
- **Parking and switching.** Idle models are parked to free GPU memory and
  woken on the next request. On a discrete card a model can park its weights in
  host RAM, which wakes much faster than reloading from disk.
- **Unified-memory and discrete GPUs.** On a discrete card, mllm counts the
  card's memory apart from host RAM and picks a GPU with room on a machine with
  several. One GPU per model in this release.
- **Models from a directory or Hugging Face.** Models live in `~/models`;
  `model: {hf: <org>/<name>}` downloads into `~/models/sources`.
- **OpenAI-compatible API.** `/v1/models` and `/v1/chat/completions`, with
  streaming and tool calls. The endpoint listens on `0.0.0.0:8443` and requires
  the API key; `--listen` limits it to one address.
- **Settings in one place.** Every setting can come from the YAML document, a
  flag or an environment variable; `mllm config show` prints each value and
  where it came from.
- **systemd units** for the standalone, server and host roles.

## Known limits

- Models that span several GPUs (tensor parallelism) are not supported yet.
- No web UI; everything goes through the CLI and the management API.
- Parking uses the engines' development controls. mllm keeps them on loopback
  behind a per-launch key, but they are not production-hardened; a host can opt
  out with `--deep-park off`.
- SGLang ModelOpt (NVFP4) checkpoints cannot park yet; deploy them
  `restart_only`.
- mllm does not terminate TLS. Put a TLS reverse proxy in front before exposing
  the endpoint to the internet.

## Upgrading from a release candidate

The first start of 0.1.0 changes two things once:

- An inference endpoint on the old default `127.0.0.1:8443` moves to
  `0.0.0.0:8443`, still with the API key. The old configuration file is kept as
  `<file>.pre-0.1.0` and a notice is printed. Start with
  `--listen 127.0.0.1:8443` to keep it on the machine.
- On a discrete card, standalone replaces its single memory pool with separate
  host-RAM and GPU domains. It stops the engines the earlier release started
  and re-sizes their deployments; the next request starts each one cold.

Upgrade the server before the hosts. The fixed fallback key `mllm-local` is
gone: if the credentials file cannot be read, standalone refuses to start.

## Documentation

- Getting started: https://edurdias.github.io/mllm/
- Operations (services, upgrades, rollback): `docs/operations/install.md` in
  the release archive.
