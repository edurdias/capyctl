# CapyCTL 0.1.0

The first release of CapyCTL. Control what runs next: run your models on your
own NVIDIA GPUs, and deploy, park, wake and switch them on vLLM and SGLang behind
one OpenAI-compatible endpoint, even when they don't all fit at once. CapyCTL
parks the models nobody is using so their memory is freed, and wakes the one a
request asks for.

## Install

```bash
curl -fsSL https://edurdias.github.io/capyctl/install.sh | sh
capyctl --version
```

One binary for Linux on x86-64 or ARM64. The installer checks every download
against the release's `SHA256SUMS`. Add `-s -- --version v0.1.0` to pin this
release. Bring your own vLLM or SGLang environment; CapyCTL does not install
engines or drivers.

## What is in 0.1.0

- **One machine or several.** Run everything on one GPU machine (standalone),
  or run a server with several GPU hosts that join it over mutual TLS.
- **vLLM and SGLang.** Register an existing engine environment with
  `capyctl engine add <venv>`; a deployment file needs only `name`, `engine` and
  `model`.
- **Parking and switching.** Idle models are parked to free GPU memory and
  woken on the next request. On a discrete card a model can park its weights in
  host RAM, which wakes much faster than reloading from disk.
- **Unified-memory and discrete GPUs.** On a discrete card, CapyCTL counts the
  card's memory apart from host RAM and picks a GPU with room on a machine with
  several. One GPU per model in this release.
- **Models from a directory or Hugging Face.** Models live in `~/models`;
  `model: {hf: <org>/<name>}` downloads into `~/models/sources`.
- **OpenAI-compatible API.** `/v1/models` and `/v1/chat/completions`, with
  streaming and tool calls. The endpoint listens on `0.0.0.0:8443` and requires
  the API key; `--listen` limits it to one address.
- **Settings in one place.** Every setting can come from the YAML document, a
  flag or an environment variable; `capyctl config show` prints each value and
  where it came from.
- **systemd units** for the standalone, server and host roles.
- Commands print readable text by default: tables for lists, and a summary
  with key-value details for single results. Add `--json` for scripts. Roles
  print text in a terminal and JSON lines to the journal and log files.

## Known limits

- Models that span several GPUs (tensor parallelism) are not supported yet.
- No web UI; everything goes through the CLI and the management API.
- Parking uses the engines' development controls. CapyCTL keeps them on loopback
  behind a per-launch key, but they are not production-hardened; a host can opt
  out with `--deep-park off`.
- SGLang ModelOpt (NVFP4) checkpoints cannot park yet; deploy them
  `restart_only`.
- CapyCTL does not terminate TLS. Put a TLS reverse proxy in front before exposing
  the endpoint to the internet.

## Upgrading from a release candidate

Three things to know when upgrading:

- The project is now CapyCTL: the binary is `capyctl`, variables start with
  `CAPYCTL_`, and the services are `capyctl-server`, `capyctl-host` and
  `capyctl-standalone`. Nothing reads the old `mllm` names, and state an
  earlier release wrote cannot be reused, so 0.1.0 starts fresh:
  1. Stop every deployment with the old binary (`mllm stop deployment <name>`),
     so no engine it started keeps running.
  2. Stop and disable the old services: `systemctl --user disable --now
     mllm-server mllm-host mllm-standalone` (without `--user` for a system
     install), then delete their unit files from `~/.config/systemd/user` or
     `/etc/systemd/system`.
  3. Remove the old binary and shared files: `~/.local/bin/mllm` and
     `~/.local/share/mllm`, or `/usr/local/bin/mllm` and
     `/usr/local/share/mllm`. Old state (`~/.local/state/mllm`,
     `~/.config/mllm`, `/etc/mllm`, `/var/lib/mllm`) is no longer read; keep
     it for reference or delete it. Downloaded models in `~/models` stay
     usable; their download records in `~/models/sources/.mllm` do not.
  4. Install 0.1.0 and set it up as a new installation (for a system install,
     create the `capyctl` service user as in the
     [installation guide](install.md#first-installation-system-service)).
     Upgrade the server and every host together: a 0.1.0 server and an
     earlier host cannot talk to each other.
- The inference endpoint of an existing configuration file does not move: a
  file an earlier release generated keeps `127.0.0.1:8443`. New files use
  `0.0.0.0:8443` with the API key. Set the bind or start with
  `--listen 0.0.0.0:8443` to serve the network.
- On a discrete card, standalone replaces its single memory pool with separate
  host-RAM and GPU domains. It stops the engines the earlier release started
  and re-sizes their deployments; the next request starts each one cold.

Upgrade the server before the hosts. The fixed fallback key `capyctl-local` is
gone: if the credentials file cannot be read, standalone refuses to start.

## Documentation

- Getting started: https://edurdias.github.io/capyctl/
- Operations (services, upgrades, rollback): `docs/operations/install.md` in
  the release archive.
