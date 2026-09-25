# Installer options

Pass options to the installer after `sh -s --`:

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh -s -- --version v<version> --systemd standalone
```

| Option | What it does |
|---|---|
| `--version V` | Install release `V` (for example `v<version>`). Without it, the latest release. A release candidate is only installed when you name it. |
| `--system` | Install for every user: `/usr/local/bin/mllm` and system units. Run it with `sudo`. |
| `--systemd ROLE` | Also install the systemd unit for `standalone`, `server` or `host`. The unit is not enabled or started. |
| `--repo OWNER/NAME` | Download from another GitHub repository. |
| `--uninstall` | Remove the binary and the units the installer wrote. State is kept. |

## Run standalone as a service

```bash
curl -fsSL https://edurdias.github.io/mllm/install.sh | sh -s -- --systemd standalone
mkdir -p ~/.config/mllm
printf 'MLLM_VLLM_BIN=%s\nMLLM_MODELS_ROOT=%s\n' ~/venvs/vllm/bin/vllm ~/models > ~/.config/mllm/standalone.env
systemctl --user enable --now mllm-standalone
loginctl enable-linger "$USER"    # keep it running after you log out
```

Stopping or restarting the service leaves running models alone; the next
start picks them up again.

## Private repository or mirror

The installer downloads with `curl` from the public release URL. For a
private repository it uses the GitHub CLI when `gh auth login` has been run,
or the GitHub API when `GITHUB_TOKEN` is set. To install from a mirror, set
`MLLM_INSTALL_BASE_URL` to a directory holding the release files (`https://`
or `file://`) and pass `--version`.

Service users, hardening, backups and rollback are covered in the
[operations guide](../operations/install.md) in the repository.
