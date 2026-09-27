# Install

You need Linux on x86-64 or ARM64, an NVIDIA GPU with its driver
(`nvidia-smi` works), and a vLLM or SGLang installation. The GPU can be a
discrete card or a unified-memory machine; the steps are the same.

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh
```

To install a particular release, name it (optional; without it you get the
latest release):

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh -s -- --version v<version>
```

This installs one binary, `~/.local/bin/mllm`, after checking it against the
release's checksums. Make sure `~/.local/bin` is on your `PATH`, then check:

```bash
mllm --version
```

Next: [Run on one machine](one-machine.md).

## Upgrade

Run the same command with the new version, then restart mllm. Running models
keep serving through the restart. With
[several machines](several-machines.md), upgrade the server first, then the
GPU machines one at a time.

Upgrading from an earlier release candidate to 0.1.0 changes two things once,
at the first start:

- The endpoint moves from `127.0.0.1:8443` to `0.0.0.0:8443`, still with the
  API key. mllm rewrites that line of its configuration file, keeps the old
  file as `<file>.pre-0.1.0`, and prints a notice. To keep it on the machine,
  start with `--listen 127.0.0.1:8443`.
- On a discrete card, mllm now counts the card's memory apart from host RAM.
  The first start stops the engines the earlier release started, re-sizes
  each deployment for the card (its revision goes up by one) and prints which
  ones. The next request for each starts it from scratch. This happens once;
  unified-memory machines are not affected.

## Uninstall

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh -s -- --uninstall
```

Your state in `~/.local/state/mllm` and your models are kept.

## Build from source

You need stable Rust and the Protocol Buffers compiler, `protoc`, on your
`PATH`, on ARM64 as on x86-64 (for example
`sudo apt install protobuf-compiler`). Then, in a clone of the repository:

```bash
cargo build --release --locked -p mllm-cli
```

The binary is `target/release/mllm`.

To install for every user, pin a version or add a systemd service, see
[Installer options](installer.md).
