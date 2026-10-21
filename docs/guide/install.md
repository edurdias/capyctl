# Install

You need Linux on x86-64 or ARM64, an NVIDIA GPU with its driver
(`nvidia-smi` works), and a vLLM, SGLang, TensorFold or llama.cpp installation,
or another inference server you run as a [custom engine](engines.md#custom-engines).
The GPU can be a discrete card or a unified-memory machine; the steps are the same.

```bash
curl -fsSL https://edurdias.github.io/capyctl/install.sh | sh
```

To install a particular release, name it (optional; without it you get the
latest release):

```bash
curl -fsSL https://edurdias.github.io/capyctl/install.sh | sh -s -- --version v<version>
```

This installs one binary, `~/.local/bin/capyctl`, after checking it against the
release's checksums. Make sure `~/.local/bin` is on your `PATH`, then check:

```bash
capyctl --version
```

It prints the name and the version. `capyctl version` prints the same line and the
commit the release was built from, which is worth including in a bug report.

Next: if you have no engine yet, [Install an engine](install-engines.md);
then [Run on one machine](one-machine.md).

## Upgrade

Run the same command with the new version, then restart CapyCTL. Running models
keep serving through the restart. With
[several machines](several-machines.md), upgrade the server first, then the
GPU machines one at a time.

Two things to know when upgrading from an earlier release candidate to 0.1.0:

- Nothing about the endpoint changes by itself. New configuration files
  listen on `0.0.0.0:8443` with the API key, but a file an earlier release
  generated keeps `127.0.0.1:8443`. To open it to the network, set
  `0.0.0.0:8443` in the file or start with `--listen 0.0.0.0:8443`.
- On a discrete card, CapyCTL now counts the card's memory apart from host RAM.
  The first start stops the engines the earlier release started, re-sizes
  each deployment for the card (its revision goes up by one) and prints which
  ones. The next request for each starts it from scratch. This happens once;
  unified-memory machines are not affected.

## In a container

A host or standalone role can also run as the main process of a container,
with the engines and models mounted in. CapyCTL publishes no image; the
[operations guide](../operations/install.md#running-in-a-container) shows
what to put in one, which mounts it needs, and how to stop it cleanly. So far
this has run with Docker on a unified-memory machine and on a discrete card;
Podman and a host joined to a server have not run in a container yet.

## Uninstall

```bash
curl -fsSL https://edurdias.github.io/capyctl/install.sh | sh -s -- --uninstall
```

Your state in `~/.local/state/capyctl` and your models are kept.

## Build from source

You need stable Rust and the Protocol Buffers compiler, `protoc`, on your
`PATH`, on ARM64 as on x86-64 (for example
`sudo apt install protobuf-compiler`). Then, in a clone of the repository:

```bash
cargo build --release --locked -p capyctl-cli
```

The binary is `target/release/capyctl`.

To install for every user, pin a version or add a systemd service, see
[Installer options](installer.md).
