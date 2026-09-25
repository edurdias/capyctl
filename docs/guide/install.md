# Install

You need Linux on x86-64 or ARM64 with an NVIDIA GPU, and a working vLLM or
SGLang installation.

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh
```

This installs one binary, `~/.local/bin/mllm`, after checking it against the
release's checksums. Make sure `~/.local/bin` is on your `PATH`, then check:

```bash
mllm --version
```

Next: the [Quickstart](quickstart.md).

## Upgrade

Run the same command again, then restart mllm. Running models keep serving
through the restart. With
[several machines](several-machines.md), upgrade the server first, then the
GPU machines one at a time.

## Uninstall

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh -s -- --uninstall
```

Your state in `~/.local/state/mllm` is kept.

To install for every user, pin a version or add a systemd service, see
[Installer options](installer.md).
