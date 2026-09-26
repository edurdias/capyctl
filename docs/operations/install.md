# Installing and operating mllm as a service

This guide covers installing a release with `install.sh`, running each role
under systemd, upgrading, and rolling back. It follows SPEC §3.3 (one
executable per OS/architecture, ADR 0001), §4.3 (foreground roles, OS service
definitions, restart distinct from drain) and §13.3 (owner-only runtime
files).

What this guide does not establish: the unit files, the tarball and the
installer are checked locally (`scripts/verify-packaging.sh`), not on a GPU
host. Running a unit is
not evidence that an engine recipe works; engine qualification stays with the
live runbooks.

Every setting mllm reads, with its YAML field, flag and environment variable,
is listed in the [settings reference](configuration.md).

## Restart is not drain

Read this before anything else.

| Action | What happens to engines | Deployments |
|---|---|---|
| `systemctl stop` / `restart` of `mllm-host` or `mllm-standalone` | Keep running in their own process groups; the next start re-attaches them. | Kept. |
| `systemctl stop` / `restart` of `mllm-server` | Keep running on their hosts; the next start reconciles them. | Kept. |
| Host crash, `Restart=on-failure` restart | Keep running; re-attached. | Kept. |
| `mllm drain host <name>` (server running) | Stopped, with verified cleanup. | Kept, eligible for on-demand activation. |
| `mllm drain standalone` (standalone running) | Stopped, with verified cleanup. | Kept, eligible for on-demand activation. |
| `mllm delete deployment <id> --stop` | That deployment's engines stopped. | Deleted. |
| `mllm revoke host <name>` (the host role exits with code 14, not restarted) | Keep running, owned and charged; dispatch to them is closed. `join host --recover` re-proves them. | Kept. |

A signal (SIGTERM, SIGINT) to any role closes admission, lets admitted
requests finish within the role document's `shutdown.drain_timeout` (30 s by
default, 0 s to 600 s), cancels what is still streaming, and exits without
touching an engine (`crates/mllm-cli/src/shutdown.rs`). A second signal cuts
the wait short; engines are still retained.

The units therefore have no draining `ExecStop=`. To take engines down, drain
first, then stop the unit:

```bash
# Remote host: from the server, as the service user.
sudo -u mllm mllm drain host gpu-box --config /etc/mllm/server.yaml
sudo systemctl stop mllm-host           # on gpu-box

# Standalone.
sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/standalone \
  mllm drain standalone
sudo systemctl stop mllm-standalone
```

`mllm drain host` of an offline host returns at once with `stops: "pending"`;
the Stops complete when the host reconnects within the drain window, and
`--wait` waits for them.

Do not use `systemctl kill` on the host or standalone unit: by default it
signals every process in the unit's cgroup, engines included.

### Why `KillMode=process`

Engines are launched into their own process groups, but they stay in the
unit's cgroup. systemd's default `KillMode=control-group`, and `mixed`, signal
every process in that cgroup on stop, which would kill every engine at each
restart and turn an ordinary restart into an unaccounted termination.
`KillMode=process` sends the stop signal, and the final SIGKILL if
`TimeoutStopSec=` expires, to the mllm process only. systemd then logs that
processes remain in the stopped unit, which is expected. At system shutdown the
remaining engines are terminated with everything else; the host reconciles on
boot.

The server unit uses `KillMode=mixed`, because the server launches no engines.

Consequences of the engines sharing the host unit's cgroup:

- Resource limits on the unit apply to the engines: the units set
  `TasksMax=infinity`, `LimitNOFILE=1048576` and `LimitMEMLOCK=infinity`, and
  set no `MemoryMax=`. Do not add a memory limit in a drop-in without
  accounting for every engine's memory.
- `OOMPolicy=continue`: an engine the kernel OOM-kills does not stop the
  agent; the host observes the loss and reconciles it.

### `TimeoutStopSec` and `drain_timeout`

`TimeoutStopSec=90s` covers the default 30 s drain, the 2 s cancellation grace
and teardown. Keep `TimeoutStopSec` at least `drain_timeout + 60s`. When a role
document raises `shutdown.drain_timeout`, raise the unit's timeout with it:

```bash
sudo systemctl edit mllm-host
# [Service]
# TimeoutStopSec=11min        # for drain_timeout: "600s"
```

If the timeout expires, systemd kills the mllm process only; engines survive,
as with any other restart.

### Exit codes and restarts

The units restart a role that fails (`Restart=on-failure`) except on exit
codes that restarting cannot heal (`RestartPreventExitStatus=`). The table
lists those codes, plus CLI exit codes an operator is likely to meet; the
"Units" column says which units, if any, refuse to restart on each. The codes
are defined in `crates/mllm-cli/src/output.rs`.

| Exit | Meaning | Units | What heals it |
|---|---|---|---|
| 2 | Invalid configuration | all | Fix the role document (`mllm validate config`). |
| 3 | Unauthorized | all | Fix the identity or credentials. |
| 4 | Insufficient resources, including a GPU that cannot hold the deployment (`insufficient_device_memory`) or has no fresh reading (`device_unobserved`) | none: a CLI command's exit | Free memory, use a smaller or quantized checkpoint, or wait for the GPU to be observed. |
| 5 | Unsupported, including state written by a newer mllm (`store_from_newer_version`), and on GPUs `unsupported_gpu_topology`, `multi_gpu_unsupported` and `host_backed_unavailable` | all | The newer binary or a restored backup (see "State and migrations"); for the GPU codes, see "Discrete NVIDIA GPUs". |
| 14 | The controller revoked this host (`host_revoked`) | host | Recovery under the same identity (below). |
| 15 | No allowed host is eligible for placement (`host_ineligible`) | none: a CLI command's exit (`start`), never a role's, so no unit lists it | Upgrade, undrain, reconnect or re-enroll the host the message names, then start again. |
| 16 | The path holds no `vllm` or `sglang` package (`engine_not_found`) | none: `mllm engine` exits, never a role's | Name the venv, its `bin/vllm` or its `bin/python3`, or scan more with `mllm engine detect --path DIR`. |
| 17 | The package is not a supported engine (`engine_unsupported`) | none | Register a vLLM or SGLang installation. |
| 18 | The version check failed or timed out; nothing is written (`engine_version_failed`) | none | Repair the installation until its version check succeeds and matches its package metadata, then add it again. |
| 19 | The profile name is taken (`profile_exists`) | none | Use `--name`, or remove the existing profile first. |
| 20 | Removal or replacement would affect the listed deployments (`profile_in_use`) | none | Stop them, or rerun with `--drain`. |
| 21 | The server refused the re-published document (`publish_rejected`); its reason follows | none | Fix what the reason names. The profile stays in `engines.yaml`, shown as not published. |
| 22 | A role is running but its control socket did not take or answer the request (`agent_unreachable`) | none | On `add`, `engines.yaml` is written and takes effect when the role restarts. On `remove` with no role listening, nothing is written: start the role and retry. If the message says the outcome is unknown (the role took the request, then closed the connection or did not answer in time), run `mllm engine list`, then `mllm engine remove` again; a retry resumes the same removal. |
| 23 | `engine add` without a path needs a terminal (`not_interactive`) | none | Name the installation, or run it at a terminal to pick one. |
| 24 | No allowed host publishes the deployment's runtime profile (`profile_not_published`); nothing was stored, and the message lists each host with the profiles it publishes | none: a CLI command's exit (`deploy`), never a role's | Register the profile on a host with `mllm engine add <path> --name <profile>`, then deploy again. A deployment is never re-resolved after `engine add`. |
| 25 | The deployment is still stopping (`still_stopping`): a `start` sent right after a `stop` arrived before the stop's cleanup was verified; nothing was started | none: a CLI command's exit (`start`), never a role's | Retry in a moment, or run `mllm start deployment <name> --wait`, which waits for the stop to finish and then starts. |

**A revoked host (14).** After `mllm revoke host <name|id>`, the controller
answers the host's control session, over its mutual-TLS channel, that its
certificate is revoked. The host logs one line and exits with code 14 instead
of retrying:

```
error [host_revoked]: Host <host id> is revoked; its engines keep running. To recover the same identity, run `mllm invite host <host id> --recover --output FILE` on the server for a new recovery invitation, then `mllm join host --join-file FILE --recover` on this host, and start the host again
```

Its engines are neither stopped nor signalled, and its state directory and
journal are untouched. After `join host --recover` (ADR 0016) and
`systemctl start mllm-host`, the host reconnects under the same host id and
each engine is re-proven by a fresh probe, not relaunched. Only that exact,
authenticated answer from the controller stops the host: an unreachable or
restarting server, a version refusal and any other refusal keep it
reconnecting with its backoff. The standalone role has no enrolled host to
revoke and never exits with 14; the server unit does not either.

## Release contents

A release (decided 2026-09-24: one self-contained binary, GitHub
Releases and `install.sh`; Homebrew is deferred) carries these assets:

| Asset | Holds |
|---|---|
| `mllm-<version>-linux-x86_64.tar.gz` | The x86-64 build. |
| `mllm-<version>-linux-aarch64.tar.gz` | The ARM64 build. |
| `install.sh` | The installer (POSIX `sh`). |
| `SHA256SUMS` | SHA-256 of every tarball and of `install.sh`. |

Each tarball holds one directory:

```text
mllm-<version>-linux-<arch>/
  bin/mllm                  stripped release binary: every role, the CLI and
                            mllm's Python runtime helpers (embedded)
  packaging/systemd/system/ system units
  packaging/systemd/user/   user units
  docs/examples/            example role and deployment documents
  docs/operations/install.md
  BUILDINFO                 version, commit, dirty flag, toolchain, source date,
                            embedded runtime manifest digest
  SHA256SUMS                digest of every file in the directory
```

There is no `runtime/` directory in the release. mllm's Python helpers (the
vLLM guard and entry, the SGLang entry and its modules, the capability
probes) are compiled into `bin/mllm` with a manifest of their SHA-256 digests
(`crates/mllm-agent/build.rs`), and each role that launches engines writes
them to its own state directory; see "The managed runtime directory".

Engines, engine Python environments, model weights and GPU drivers are not in
the release and are never installed by it (SPEC §4.2, §15.2).

### Building a release

`packaging/release.sh [OUT_DIR]` (default `dist/`) builds the tarball for the
machine it runs on, from a clean, committed tree: it builds with `--locked`,
runs `scripts/check-release-clean.sh` (no test engine may reach the shipped
binary), ships only files tracked by git, refuses untracked files under
`runtime/` (the build embeds every `runtime/*.py` it finds), and stamps
entries with the commit time so a rebuild of the same commit and toolchain
gives the same archive. It copies `install.sh` beside the tarball and writes
`SHA256SUMS`. Build on each architecture, gather the tarballs in one
directory and run `packaging/release.sh --sums DIR` there to write the
release's `SHA256SUMS`. `scripts/verify-packaging.sh` checks the units, the
tarball and the installer locally.

## Installing with install.sh

Release candidates are published as pre-releases on the GitHub Releases page
of the private repository `edurdias/mllm` (decided
2026-09-24; the repository opens later). Two things follow:

- **You need a credential.** Run `gh auth login` first (preferred), or export
  `GITHUB_TOKEN` with read access to the repository. Without one, neither the
  download of `install.sh` nor the installer itself can reach the release.
- **You must pass `--version`.** GitHub's "latest release" never resolves to a
  pre-release or a draft, so while only release candidates exist the
  installer cannot find one on its own. Name it, for example
  `--version v0.1.0-rc.4` (the leading `v` is optional). Without it the
  installer stops and says so.

```bash
# As yourself: ~/.local/bin/mllm (add ~/.local/bin to PATH).
gh auth login                                   # once, or export GITHUB_TOKEN
gh release download v0.1.0-rc.4 -R edurdias/mllm -p install.sh
sh install.sh --version v0.1.0-rc.4

# Also install a user unit for a role (installed, not enabled).
sh install.sh --version 0.1.0-rc.4 --systemd standalone

# For every user: /usr/local/bin/mllm and system units.
sudo sh install.sh --system --version 0.1.0-rc.4 --systemd host
```

Without `--version` the latest published full release is installed; a draft
or a pre-release is installed only by naming it. The installer:

1. detects the OS (Linux) and architecture (`x86_64`, `aarch64`);
2. downloads the tarball and `SHA256SUMS` with `gh release download`, else
   through the GitHub API with `GITHUB_TOKEN`, else from the public download
   URL (`MLLM_INSTALL_BASE_URL` names a mirror directory, `https://` or
   `file://`, instead);
3. refuses to install unless the tarball's SHA-256 matches `SHA256SUMS`, every
   file in it matches the archive's own `SHA256SUMS`, and the binary reports
   the requested version;
4. replaces `<prefix>/bin/mllm` atomically (a running role keeps its open
   executable) and keeps the units, examples and this guide under
   `<prefix>/share/mllm/`;
5. with `--systemd <server|host|standalone>`, writes that role's unit to
   `~/.config/systemd/user/` (or `/etc/systemd/system/` with `--system`),
   pointed at the installed binary, and runs `systemctl daemon-reload`. It
   never enables or starts a unit, creates users or touches state, except
   that a user unit gets an empty `~/.local/state/mllm` (0700) if there is
   none (see "User services").

`sh install.sh --uninstall [--system]` removes the binary, `<prefix>/share/mllm`
and the units the installer wrote. State directories are kept.

To install from a downloaded tarball by hand instead:

```bash
V=0.1.0-rc.4; A=$(uname -m)
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf mllm-$V-linux-$A.tar.gz
(cd mllm-$V-linux-$A && sha256sum -c --quiet SHA256SUMS)
sudo install -m 0755 mllm-$V-linux-$A/bin/mllm /usr/local/bin/mllm
```

## Layout

| Path | Owner, mode | Holds |
|---|---|---|
| `/usr/local/bin/mllm` (`~/.local/bin/mllm`) | root (you), 0755 | The binary. |
| `/usr/local/share/mllm/` (`~/.local/share/mllm/`) | root (you), 0755 | Units, examples, this guide, `BUILDINFO` of the installed release. |
| `/etc/mllm/<role>.yaml` | root:mllm, 0640 | Role documents (operator configuration). |
| `/etc/mllm/<role>.env` | root:mllm, 0640 | Optional environment for the unit. |
| `/var/lib/mllm/` | `mllm:mllm`, 0700 | State root (`StateDirectory=`); holds `server/`, `host/`, `standalone/`, `tmp/`. |
| `/var/lib/mllm/host/runtime/` | `mllm:mllm`, 0700 / files 0600 | The managed runtime directory of the host role (standalone: `/var/lib/mllm/standalone/runtime/`). Written by mllm. |
| model store (`/srv/models`) | readable by `mllm` | Checkpoints. Read-only to the host unit by default. |

### The managed runtime directory

The engine imports mllm's own Python from the runtime directory, so a module
another account can rewrite runs as the engine behind the controls it is meant
to guard (SPEC §9.1, §13.3). The binary therefore writes that directory
itself:

- **Where.** A host whose document does not name `runtime_dir` uses
  `<state_dir>/runtime`. Standalone uses `<state root>/runtime` unless
  `MLLM_RUNTIME_DIR` is set. The server launches no engine and has none.
- **When.** `mllm init host` writes it; every `mllm start host` and
  `mllm start standalone` checks it before anything can launch.
- **How.** A 0700 directory owned by the service user, each module 0600,
  and a marker file `.mllm-managed-runtime` naming the embedded manifest. It
  is built in a sibling directory and renamed into place, so a launch never
  sees a partial tree.
- **Upgrade.** A binary with a different embedded manifest replaces the tree
  at start and logs `runtime directory ... refreshed`. Engines already running
  keep the modules they imported; the new ones apply to launches from then on.
- **Tampering.** A managed tree whose modules, modes or entries differ from
  the manifest (an edited module, a `__pycache__`, a loosened mode, a deleted
  file) is restored from the embedded copy at start, with a warning naming
  what differed (never contents).
- **Not mllm's.** A directory at that path without the marker is refused, not
  overwritten: remove it, or name it as `runtime_dir`. A directory named by
  `runtime_dir` or `MLLM_RUNTIME_DIR` is never written; mllm only checks it.

Every launch still passes the integrity check
(`crates/mllm-agent/src/runtime_integrity.rs`,
`crates/mllm-adapters/src/owner_only.rs`): the directory, every subdirectory
and every `.py` module owned by the service user, nothing writable by other,
group write only through the owner's private group, no symlinks, nothing
importable besides `.py` source, and every ancestor of the SGLang entry owned
by root or the service user and not writable by others, on a canonical path.
A `runtime_dir` you maintain yourself must meet the same rule. Never run
`python` against a runtime directory without `-B`; the managed one is repaired
at the next start, one of your own is refused until the `__pycache__` is
removed.

State directories are stricter still: `state_dir`, its `identity`,
`observation` and `rendezvous` directories must be canonical paths, owned by
the service user, mode 0700, and every ancestor of the identity directory must
be owned by root or the service user with no group or other write at all
(`crates/mllm-agent/src/identity_storage.rs`; `init` otherwise fails with
"Role identity is unsafe or already in use"). mllm creates the directories
itself; do not place `/var/lib/mllm` behind a symlink or under a
group-writable directory.

## First installation (system service)

Run as root on each machine.

```bash
# Service user with a private group; its home is the state root, so engine
# caches under $HOME (triton, flashinfer, ...) land in private state.
useradd --system --user-group --home-dir /var/lib/mllm --shell /usr/sbin/nologin mllm
install -d -o mllm -g mllm -m 0700 /var/lib/mllm

# The binary and the role's unit (host shown; server and standalone alike).
sh install.sh --system --version 0.1.0-rc.4 --systemd host
install -d -m 0750 -g mllm /etc/mllm
```

The service user also needs read access to the engine installations named in
the host document and to the model store, and access to the GPU device nodes
(on most NVIDIA installs they are world-accessible; otherwise add
`SupplementaryGroups=` in a drop-in). The units set `NoNewPrivileges=`, so a
setuid helper such as `nvidia-modprobe` cannot create missing device nodes from
inside the service; make sure the driver's device nodes exist at boot
(for example with `nvidia-persistenced`).

### Server

```bash
sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/server \
  mllm init server --output /var/lib/mllm/server/config/server.yaml
install -m 0640 -o root -g mllm /var/lib/mllm/server/config/server.yaml /etc/mllm/server.yaml
# Edit /etc/mllm/server.yaml: bootstrap and control listeners, enrollment
# addresses, shutdown.drain_timeout (see docs/examples/server.yaml).
mllm validate config --file /etc/mllm/server.yaml
systemctl enable --now mllm-server
```

`init` creates the server identity and credentials under the state directory
(owner-only); it prints file locations, never secrets. Client commands on the
server machine run as the service user with the same document, for example
`sudo -u mllm mllm list hosts --config /etc/mllm/server.yaml`.

### Host

```bash
sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/host \
  mllm init host --output /var/lib/mllm/host/config/host.yaml
install -m 0640 -o root -g mllm /var/lib/mllm/host/config/host.yaml /etc/mllm/host.yaml
# The generated document validates as written: its resource_policy is derived
# from this machine's memory and GPUs as standalone derives its own, and models
# live in ~/models of the service user (downloads in ~/models/sources) unless
# model_store names another directory (see "Models and downloads").
# Edit /etc/mllm/host.yaml for name, ingress (the address the server forwards
# inference to) and, if you like, the limits (see docs/examples/host.yaml).
# Leave runtime_dir out.
mllm validate config --file /etc/mllm/host.yaml

# Enroll with an invitation created on the server (`mllm invite host`).
sudo -u mllm mllm join host --join-file gpu-box.join --config /etc/mllm/host.yaml
systemctl enable --now mllm-host
```

`init host` prints the managed `runtime_dir` it wrote
(`/var/lib/mllm/host/runtime`). Name `runtime_dir` in the document only to run
from a directory you maintain yourself.

### Standalone

Without `--config`, `mllm start standalone` loads its role document from
`<MLLM_STATE_DIR>/config/standalone.yaml`, generating it (and the protected
credentials) on first start, and writes the managed runtime to
`<MLLM_STATE_DIR>/runtime`. Its engine installation comes from a flag, the
environment or the document's `host.local_engine` (see the
[settings reference](configuration.md#engine-installation)); with the
packaged unit the environment is simplest. Put it in
`/etc/mllm/standalone.env`:

```bash
# /etc/mllm/standalone.env (root:mllm 0640)
MLLM_VLLM_BIN=/opt/vllm/bin/vllm
# Optional: models are in ~/models of the service user unless named here.
MLLM_MODELS_ROOT=/srv/models
```

```bash
systemctl enable --now mllm-standalone
```

The unit sets `MLLM_STATE_DIR=/var/lib/mllm/standalone`. Operator commands must
use the same state directory (and the same `MLLM_MANAGEMENT_ADDR`, if the unit
moves the management listener with the variable rather than the document): `sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/standalone mllm status deployment <id>`.
`MLLM_RUNTIME_DIR` (development) makes standalone run from that directory
instead of the managed one.

#### Explicit standalone document

`mllm start standalone --config <file>` uses `<file>` as the role document
instead (SPEC §15.2, R13). A missing or invalid explicit file refuses the
start with exit code 2; it is never replaced by the generated default, and
nothing is written under `<MLLM_STATE_DIR>/config`. On a state root that has
never served, the first start creates the protected credentials there, as a
first implicit start would; a state root that has served and lost its
credentials refuses instead.

Where each setting comes from, highest precedence first:

| Setting | Source |
|---|---|
| Role document | `--config <file>`, else `$MLLM_CONFIG`, else `<state root>/config/standalone.yaml`, else generated there. |
| Registered engines (`engines.yaml`) | Beside the document named by `--config` or `$MLLM_CONFIG`, else `$XDG_CONFIG_HOME/mllm/engines.yaml` (`~/.config/mllm/engines.yaml`). `mllm engine` uses the same rule, so it and the running role read the same file. A host follows the same rule. |
| State root | `--state-dir`, else `MLLM_STATE_DIR`, else the top-level `state_dir` of the document named by `--config` or `$MLLM_CONFIG`, else `$XDG_STATE_HOME/mllm`, else `~/.local/state/mllm`. The document may state `server.state_dir` and `host.state_dir` only as `<state root>/server` and `<state root>/host` (relative paths resolve against the document's directory); any other value is refused. |
| Listener addresses | Inference: `--listen`, else `MLLM_INFERENCE_ADDR`, else `server.listeners.inference.bind`, else `0.0.0.0:8443`. Management: `--management-listen`, else `MLLM_MANAGEMENT_ADDR` (`MLLM_STANDALONE_MANAGEMENT_ADDR` is still read, with a warning), else `server.listeners.management.bind`, else `127.0.0.1:7443`; loopback only. |
| Engine installation | `--vllm-bin` / `--sglang-bin` and the other engine flags, else `MLLM_VLLM_BIN` / `MLLM_SGLANG_BIN` and the other variables, else `host.local_engine`, `host.runtime_dir` and `host.resource_policy.endpoint_port_range`; plus engines registered with `mllm engine add`. See the [settings reference](configuration.md#engine-installation). |
| Models directory | `--models-root`, else `MLLM_MODELS_ROOT`, else `host.model_store.path`, else `~/models` (created). See "Models and downloads". |
| Model downloads | `--model-sources` / `--model-sources-max`, else `MLLM_MODEL_SOURCES` / `MLLM_MODEL_SOURCES_MAX`, else `host.model_sources`, else allowed with a 500 GiB cap. |
| Any other setting of the document | `--set <path>=<value>`, else `MLLM_SET__<PATH>`, else the document, else its default. `mllm config show` prints every effective value and where it came from. See the [settings reference](configuration.md#any-setting-by-its-path). |
| Drain bound, switching, observability | The role document in use. |

The packaged units start standalone without `--config`, so an upgrade that
reinstalls them never depends on a file the operator has not written. To keep
the document under `/etc/mllm` instead, write it (a generated one is a good
start), validate it, and override `ExecStart=` in a drop-in:

```bash
install -m 0640 -o root -g mllm /var/lib/mllm/standalone/config/standalone.yaml /etc/mllm/standalone.yaml
mllm validate config --file /etc/mllm/standalone.yaml
systemctl edit mllm-standalone
#   [Service]
#   ExecStart=
#   ExecStart=mllm start standalone --config /etc/mllm/standalone.yaml
systemctl restart mllm-standalone
```

### Models and downloads

Standalone and enrolled hosts resolve the same two settings by the same rule,
highest precedence first: the flag on `mllm start standalone` or
`mllm start host`, then the environment, then the YAML document (the host
document, or the `host:` block of the standalone document), then the default.

| Setting | Flag | Variable | YAML | Default |
|---|---|---|---|---|
| Models directory (relative model paths resolve here) | `--models-root <dir>` | `MLLM_MODELS_ROOT` | `model_store.path` | `~/models` |
| Hugging Face and HTTP downloads | `--model-sources allowed\|disabled` | `MLLM_MODEL_SOURCES` | `model_sources.huggingface`, `model_sources.http` | `allowed` |
| Cap on all downloaded models | `--model-sources-max <size>` | `MLLM_MODEL_SOURCES_MAX` | `model_sources.max_bytes` | `500GiB` |
| Where downloads are kept | `--model-sources-path <dir>` | `MLLM_MODEL_SOURCES_PATH` | `model_sources.path` | the models directory (`~/models/sources`) |
| Hugging Face endpoint | `--hf-endpoint <url>` | `MLLM_HF_ENDPOINT`, else `HF_ENDPOINT` | `model_sources.huggingface_endpoint` | `https://huggingface.co` |
| Hugging Face token for a source that names none (never a flag) | | `MLLM_HF_TOKEN`, else `HF_TOKEN` | `model_sources.huggingface_token_file` | none |

A deployment that names a pinned Hugging Face revision or an HTTP URL with its
SHA-256 is downloaded by the host it is placed on, into
`<downloads>/sources/...`, verified, and then started like a local checkpoint;
`deploy model --activate --wait` waits for the download. Before a byte is
written the host reserves the download's full size against the cap and against
the free space of the filesystem, keeping 1 GiB free; a download that does not
fit is refused (`too_large` or `insufficient_space`, shown by
`mllm status deployment`). A document that states `huggingface: disabled` (or
`denied`) keeps that kind off; `allowed_hosts` and `huggingface_endpoint` still
narrow where downloads may come from. The role writes the values it resolved
into the host document it publishes, so the server plans against exactly what
the host enforces. `mllm prune sources --host-config <host.yaml>` reclaims
downloads no deployment references.

### User services

`packaging/systemd/user/` holds the same three roles for the per-user manager,
for a single operator account without a dedicated service user. They run
`~/.local/bin/mllm`, read `~/.config/mllm/<role>.yaml` and `<role>.env`, and
keep state, including the managed runtime directory, under
`~/.local/state/mllm`. The ancestor rules above apply: with a umask of 002,
`~/.local` and `~/.local/state` are created group-writable and must be fixed
first (`chmod go-w ~ ~/.local ~/.local/state`). The standalone user unit
leaves `MLLM_STATE_DIR` unset, so it and your shell both use
`~/.local/state/mllm`.

```bash
sh install.sh --version 0.1.0-rc.4 --systemd host
systemctl --user enable --now mllm-host
loginctl enable-linger "$USER"   # keep it running after logout
```

The state root must exist as a real directory before the unit first starts.
systemd 254 and later, finding `~/.local/state/mllm` missing while
`~/.config/mllm` (where the units read `<role>.yaml` and `<role>.env`)
exists, assumes its pre-254 layout and makes `~/.local/state/mllm` a symlink
to `~/.config/mllm` ("creating compatibility symlink" in the journal). State
would then land in the configuration directory, behind a symlink the roles'
identity rules refuse. `install.sh --systemd <role>` creates the empty
directory for you and warns if the link already exists; to repair a link,
stop the unit, `rm ~/.local/state/mllm` (the link only), move anything mllm
wrote under `~/.config/mllm` back out, and reinstall. Observed with systemd
255.

User units carry no file-system sandboxing: `ProtectSystem=` and similar need
privileges the per-user manager lacks (systemd.exec(5)). Prefer the system
units on shared machines.

## Discrete NVIDIA GPUs

mllm runs on machines whose GPU has its own memory (a GeForce, RTX or data
center card) as well as on unified-memory machines such as the GB10, where the
GPU and the CPU share one pool. The rules are in
ADR 0019 (`docs/design/adr/0019-discrete-gpu-and-network-endpoint.md` in the source repository).

**Requirements.** The NVIDIA driver with `nvidia-smi` at `/usr/bin/nvidia-smi`
(or `/bin/nvidia-smi`), which every driver package installs. mllm runs it with
a cleared environment and a 3 s bound to read each GPU's index, UUID, PCI
address and memory; it needs no other library. A machine that mixes an
integrated and a discrete GPU is refused at start (`unsupported_gpu_topology`,
exit 5).

**What standalone detects.** At start, standalone reads `nvidia-smi`. A GPU
that reports no memory of its own is integrated, and the machine keeps the
single `unified` domain. Otherwise it publishes two kinds of memory domain:

| Domain | Memory | Managed limit | Free reserve | Parked limit |
|---|---|---|---|---|
| `system` (`memory: distinct`) | host RAM | 50 % of RAM | 20 % of RAM | 25 % of RAM (host-KV 10 %) |
| `gpuN` (`memory: device`), one per GPU | the card | total − reserve | the larger of 1 GiB and 8 % of the card | the smaller of 8 GiB and 25 % of the card |

The reserve on the card leaves room for a desktop session on a workstation
GPU. Memory other programs already hold on the card lowers what mllm sees as
available; it is never hidden. An enrolled host states the same shape in its
document ([`examples/host-discrete.yaml`](../examples/host-discrete.yaml)), and
the host refuses to start if a `device` domain does not match the GPU it
observes (`device_policy_mismatch`, exit 2).

**Both domains are charged.** Every deployment on a discrete GPU is charged on
the card (weights, KV cache, and the engine's CUDA context and graphs, 1.25 GiB
until measured) and in host RAM (the engine process itself, 4 GiB until
measured). A deployment that states no memory is sized from its checkpoint:
`weights × 1.10` plus a KV cache of `min(4 GiB, 25 % of the card's managed
limit)`. A vLLM deployment asks for at least 75 % of the card, because vLLM
0.29 with CUDA graphs does not start a 4B model on a 16 GB card below
`--gpu-memory-utilization 0.75`; for the same reason vLLM needs a card of
about 10 GiB or more. On a smaller card the host still starts, and each vLLM
deployment is refused with `insufficient_device_memory` and its numbers. A deployment that lists `resources:` itself must
name the `system` domain as well as the GPU's (`missing_system_allocation`
otherwise); leaving them out and letting mllm derive them is the portable form.

**One GPU per model; mllm picks it.** On a machine with several GPUs, each GPU
is its own domain. mllm places a new instance on the GPU where it fits with the
most room, and when none has room it parks or stops models on the GPU where the
fewest need to go. A stopped instance returns to its last GPU when it fits
there. To pin a GPU, name it in the deployment:

```yaml
devices: [{id: gpu1}]
```

The claim takes the sharing the host states for that GPU; add
`sharing: exclusive` (or `shared`) to state it yourself.

`gpuN` is the driver's index at start (`nvidia-smi -L`). The engine is started
with only that GPU visible: `CUDA_VISIBLE_DEVICES` set to the GPU's UUID, or to
its index with `CUDA_DEVICE_ORDER=PCI_BUS_ID` when no UUID is known. A
deployment that names two GPUs, or asks for tensor parallelism, is refused
(`multi_gpu_unsupported`, exit 5): one GPU per model in 0.1.0. An instance
started through a path that does not place it runs on the lowest-index GPU.

**Residency tiers.** A deployment parks in one of three ways (its
`residency`):

| Tier | Park | Wake | Host RAM while parked |
|---|---|---|---|
| `host_backed` | the weights are copied to pinned host RAM | copied back to the card | the weights copy |
| `deep` | the weights are dropped | reloaded from disk | the engine process only |
| `restart_only` | never parks: the engine is stopped | a cold start | none |

`host_backed` is the default on a discrete GPU, because a wake from host RAM is
several times faster than a reload from disk. mllm chooses it when the copy
plus the engine process fits the `system` domain's parked limit, and `deep`
otherwise (also while a downloaded model's size is not known yet);
`restart_only` when the engine's deep parking is off. The copy is charged in
host RAM at 1.5 times the weights (pinned memory is rounded up; vLLM 0.29
measured 1.37 times), for the engine's whole life: SGLang keeps its backup,
and vLLM keeps the pinned memory after a wake. When a model must make room and
its copy no longer fits in host RAM (or a `deep` model's parked residue no
longer fits on the card beside the model being started), it is stopped rather
than parked, and the switch record says `released: stopped (no room to park)`; a park the switch
planned that host memory cannot take when it is sent (other programs hold the
RAM) is refused and the model is stopped instead. An engine build without parking support
refuses a `host_backed` or `deep` launch with `capability_missing:deep_park`,
and so does an SGLang ModelOpt (NVFP4) checkpoint: its wake is not proven yet,
so choose `restart_only` for it. On a unified machine `host_backed` is refused
(`host_backed_unavailable`, exit 5): the copy would come out of the same
memory it is supposed to free.

**`insufficient_device_memory`** (exit 4) means the GPU cannot hold the
deployment's card allocation plus its reserve: at deploy, when the size mllm
derived from the checkpoint is larger than the card's managed limit, or at
launch, when the card has less free memory than the allocation needs (another
program may be holding it). Use a smaller or quantized checkpoint, a smaller KV
cache, or free the card. **`device_unobserved`** (exit 4) means `nvidia-smi`
gave no fresh reading for that GPU; nothing new starts on it until it does, and
running models keep their accounting.

An enrolled host with a discrete GPU needs this release on both the server
and the host. The host declares the capability `device_memory_domains`; the
server never places a deployment charged on a GPU's domain on a host that does
not, and reports `host_capability_missing:device_memory_domains` instead.
Unified-memory hosts need nothing new.

## Network access

The inference endpoint listens on `0.0.0.0:8443` and requires the API key.
[Network access](network-access.md) shows where the key is, how to narrow the
address to loopback or a Tailscale address, how to turn the key off (and why
not to), and how to put a TLS reverse proxy in front for the internet.

## Command output

Commands that read records print an aligned table by default, whether or not
the output is a terminal: `list hosts`, `list deployments`, `list engines`,
`status deployment`, `engine list` and `engine detect`. Hosts appear by name
(by id when they have none), memory in GiB and timeouts in seconds. Nested
detail (latency distributions, installation fingerprints, development-control
marks) is only in the JSON.

    $ mllm list engines --config server.yaml
    HOST      PROFILE   ENGINE   VERSION   CUSTOM   DEEP PARK   STATE    DEPLOYMENTS
    gpu-box   vllm      vllm     0.11.0    no       enabled     online   qwen3-8b
    gpu-box   sglang    sglang   0.5.3     no       enabled     online   -

Scripts pass `--format json` (or `--json`): the command then prints its JSON
result, the same document earlier releases printed, and reports errors as JSON
on stderr. `--output json` is still accepted and means the same. `--format
table` asks for the default explicitly. Commands that change something
(`deploy`, `start`, `stop`, `drain`, `revoke`, `engine add`, ...) and
`inspect`, `validate` and `prune` print JSON as before. Exit codes do not
depend on the format.

## Registering engines

mllm uses engines you install yourself. Register them on the machine that runs them:

    mllm engine detect [--path DIR]        # lists vLLM/SGLang environments; runs nothing
    mllm engine add ~/venvs/vllm           # or its bin/vllm, or bin/python3 for SGLang
    mllm engine add ~/sglang/bin/python3 --name sglang-patched --drift refuse
    mllm engine list
    mllm engine remove vllm [--drain]
    mllm list engines --config server.yaml # on the server: every host's engines

`detect` looks in PATH environments, conda, `~/venvs`, `~/.venv`,
`~/.virtualenvs`, uv and pipx tool environments, `/opt`, and any venv directly
in your home directory (for example `~/mllm-vllm-venv2`).

`engine add` runs the installation only after you name or pick it (a bounded
version check, the installation fingerprint and the deep-park probe), writes
the profile into `engines.yaml`, and asks the running role to publish it
without a restart. mllm never rewrites `host.yaml` or `standalone.yaml`.
`engines.yaml` sits beside the role's configuration file (`--config
dir/host.yaml` means `dir/engines.yaml`); without `--config` it is
`~/.config/mllm/engines.yaml`, for a host and for standalone alike. The role
merges it with its own document at start; a profile name declared in both is
refused. Its first line records its revision (`# mllm-document-revision: N`).
The running role listens on `<state_dir>/control.sock` (mode 0600; only the
role's own user and root are served) for these commands. Only the `mllm engine`
command writes `engines.yaml`; the running role only reads it.

`engine remove` removes only profiles `engine add` registered; one you wrote
into `host.yaml` stays yours to edit. It is refused while a deployment on this
machine uses the profile (`profile_in_use`); `--drain` stops those deployments
through the ordinary stop path first. The command asks the running role to
retire the profile, waits until the server confirms their stop evidence, then
rewrites `engines.yaml` without it and asks the role to publish the removal. A
role that is not running cannot remove a published profile
(`agent_unreachable`); start it and retry. If a removal is interrupted after
the confirmation (the command was killed, the connection dropped, or the
publication failed), the profile stays out of placement on that machine; run
`mllm engine remove <name>` again, which resumes the same removal and finishes
it.

A deployment naming a runtime profile that no allowed host publishes is refused
at `deploy` (`profile_not_published`), naming the profile and each host; run
`mllm engine add <path> --name <profile>` on a host, then deploy again.

In standalone, `MLLM_VLLM_BIN` or `MLLM_SGLANG_BIN` alone gives the profile
`local`; both give `local-vllm` and `local-sglang`. Profiles you add coexist
with them.

**CUDA toolkit and compile jobs.** Engines compile some GPU kernels the first
time they start. `mllm engine add` records the CUDA toolkit as the profile's
`cuda_home`: `CUDA_HOME` if it holds `bin/nvcc`, otherwise `/usr/local/cuda`
if that holds it. For the role's own installation (`--vllm-bin`,
`MLLM_VLLM_BIN` or `local_engine`, on a host or standalone), state it with
`--cuda-home`, `MLLM_CUDA_HOME` or `local_engine.cuda_home`; nothing is
detected for it. The engine then gets `<cuda_home>/bin` on its PATH and
`CUDA_HOME` set; vLLM uses FlashInfer only when `nvcc` is found. Without
`cuda_home`, the engine PATH has only the engine's own `bin` and the system
directories.

Each compile job can take several GB. mllm sets `MAX_JOBS` to the free memory
at launch divided by 8 GiB, at most the CPU count, and
`FLASHINFER_NVCC_THREADS=1`. The host log prints the chosen value at every
launch. To choose other limits, put `MAX_JOBS` or `FLASHINFER_NVCC_THREADS`
(positive integers) in the profile's `env`.

**With the system units, run `mllm engine` as root with the unit's
`--config`.** The host unit reads `/etc/mllm/host.yaml`, so its `engines.yaml`
is `/etc/mllm/engines.yaml`. `/etc/mllm` is root's (mode 0750, group `mllm`),
and the unit makes `/etc` read-only to the role (`ProtectSystem=strict`), so
the role never writes there; `mllm engine` does, and only root can:

    sudo mllm engine add /opt/venvs/vllm --config /etc/mllm/host.yaml
    sudo mllm engine list --config /etc/mllm/host.yaml
    sudo mllm engine remove vllm --drain --config /etc/mllm/host.yaml

Run as root, the command keeps an existing `engines.yaml`'s owner and mode, and
creates a new one (and its lock) owned by the role's service user (the owner of
the host's `state_dir`, `mllm`), mode 0600, so the role can read it. It talks to
the role over `<state_dir>/control.sock`, which serves root as well as the
service user. Root also runs the named installation's version check and
deep-park probe, so name only an installation you trust. Keep the `--config`:
`mllm engine`, `mllm list engines`, and the role itself all resolve
`engines.yaml` by the same rule (`--config`, then `$MLLM_CONFIG`, then
`~/.config/mllm/engines.yaml`), and without it root's `~/.config` is a
different file than the one the role reads. The packaged standalone unit
starts without `--config`, so its `engines.yaml` is the service user's
`/var/lib/mllm/.config/mllm/engines.yaml`, which the service user can write:
`sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/standalone mllm engine add …`.

`engine add` also works before any role has ever started — the first-run path
of adding an engine, then starting the role for the first time (standalone
refuses to start with no engine). It creates the state directory the role will
use (owner-only, mode 0700) if it does not exist yet, writes `engines.yaml`, and
exits 0 with `published: role_not_running` and the line `saved to
<engines.yaml> (revision N); start mllm (…) to use it`: no role is running (no
control socket, or a stale one nobody listens on), so the profile takes effect
at the role's first start. Only a role that is running but does not take or
answer the request exits 22 (`agent_unreachable`).

`--config` and `$MLLM_CONFIG` may be relative: every command and role resolves
them against its working directory first, so `mllm engine add … --config
host.yaml` run beside `host.yaml` writes the `engines.yaml` next to it and asks
the running role to publish it.

## Hardening in the units

The server unit takes the strict profile: read-only system, no home, private
`/tmp` and devices, no capabilities, `MemoryDenyWriteExecute=`, a
`@system-service` system-call filter.

The host and standalone units cannot: engines are their children and inherit
the sandbox. They set `NoNewPrivileges=`, `ProtectSystem=strict`,
`ProtectHome=read-only`, kernel and control-group protections, no
capabilities, and `UMask=0077`. They deliberately omit:

- `PrivateDevices=`, `DeviceAllow=` and `ProtectClock=` (the last implies a
  device allow-list), which would hide `/dev/nvidia*`;
- `MemoryDenyWriteExecute=` and `SystemCallFilter=`, which break Python, CUDA
  and JIT compilers;
- `PrivateTmp=`: with `KillMode=process` engines outlive a stop, and systemd
  removes a unit's private `/tmp` when the unit stops, underneath running
  engines; the restarted host would also see a different `/tmp` than the
  engines it re-attaches. Instead `TMPDIR=/var/lib/mllm/tmp` keeps temporary
  files private, and `/tmp`, `/var/tmp` stay writable for engine code that
  ignores `TMPDIR`.

Writable paths are the state root and `/tmp`, `/var/tmp`. If a recipe must
write into the model store, add `ReadWritePaths=` for it in a drop-in. These
restrictions have not been verified under a live engine run yet; the first
live run under the units is the check.

## Upgrade

A restart re-attaches running engines, so an upgrade does not need a drain.

**Order: the server first, then the hosts one at a time** (ADR 0017). The
server judges each host's release against its own when the host connects:

| Host release, relative to the server | `compatibility` | What the server does |
|---|---|---|
| Same `major.minor` (any patch, pre-release or build) | `supported` | Everything. |
| One minor release behind (N-1) | `upgrade_recommended` | Everything; upgrade the host soon. |
| Older than N-1, another major, or no version | `upgrade_required` | Drain-only: stop, drain, revoke, probe and inspect only; no new placement, start, wake or park. |
| Newer than the server | `refused` | Refuses the session; the host logs "upgrade the server first" and retries. |

Upgrading the server first therefore never leaves a host refused: every host
is at worst drain-only until its own upgrade, and its Ready engines keep
serving. Hosts running a release that predates this policy report no version,
so after the first server upgrade to a release that has it they are
drain-only until they are upgraded too. Check the verdicts with
`mllm list hosts` (its `VERSION` and `COMPATIBILITY` columns; with
`--format json`, `server_version`, and per host `binary_version`,
`compatibility`, `compatibility_reason`); `mllm status deployment <id>
--format json` shows the same per allowed host. A host listing a `capabilities_missing` entry that a
launch needs is left out of placement; the operation it lacks is refused as
`host_capability_missing:<name>`.

Release rule: a change that affects the protocol or durable state ships only
in a minor (or major) release; a patch release never changes the protocol,
so patch releases of server and hosts mix freely.

```bash
systemctl stop mllm-host                       # engines keep serving

# Back up state (see "State and migrations").
tar -C /var/lib/mllm -czf /var/backups/mllm-host-$(date +%Y%m%d%H%M).tar.gz \
  --warning=no-file-ignored host

# Replace the binary (and refresh the installed unit).
sh install.sh --system --version 0.2.0 --systemd host
mllm validate config --file /etc/mllm/host.yaml

systemctl start mllm-host                      # re-attaches running engines
```

The start refreshes the managed runtime directory from the new binary and
logs `runtime directory ... refreshed`. A host whose document names its own
`runtime_dir` must update that directory itself. Do the same for the server
(first) and standalone roles. The skew policy above is the only compatibility
asserted between different releases of server and host.

Running engines imported the previous runtime helpers when they launched; the
new helpers apply to launches from the restart on. If a release's notes say
its helpers or the engine control contract changed incompatibly, drain the
host before upgrading instead.

After a restart, check that each deployment is back to its prior state
(`mllm status deployment <id>`). If one is not, read its status reason before
acting on it.

### Upgrading to 0.1.0

The inference endpoint now listens on all interfaces, `0.0.0.0:8443`, and
still requires the API key. At the first start of 0.1.0, a server or
standalone document whose inference bind is exactly `127.0.0.1:8443` (the
default earlier releases generated) is updated once to `0.0.0.0:8443`. The
original is kept beside it as `<file>.pre-0.1.0`, with the same mode, and the
start prints:

```
NOTICE: mllm 0.1.0 serves inference on all interfaces: 0.0.0.0:8443 (was 127.0.0.1:8443).
The API key is still required. Configuration updated: <path> (previous copy: <path>.pre-0.1.0).
To keep inference local, start with --listen 127.0.0.1:8443 or set listeners.inference.bind.
```

The marker `<state dir>/migrations/inference-bind-v1` records that this ran, so
it never runs again; any other address, and the authentication setting, are
never touched. A document the role cannot rewrite (a read-only `/etc/mllm`,
for example, or one where the address appears more than once) is left
unchanged; the role serves on `0.0.0.0:8443` and prints
`config_migration_failed` with the line to edit. The migration then stays
pending: every later start that still finds `127.0.0.1:8443` in the unchanged
document serves on `0.0.0.0:8443` again and prints the warning, until the
document states another address (or can be rewritten). The system units mount
`/etc` read-only, so a server whose document is `/etc/mllm/server.yaml` takes
this path; to keep it local, set another address there or start with
`--listen 127.0.0.1:8443`.

To narrow the address again, pick one:

- keep inference on the machine: `--listen 127.0.0.1:8443`, or
  `MLLM_INFERENCE_ADDR=127.0.0.1:8443` in the unit's `.env` file, or
  `listeners.inference.bind: "127.0.0.1:8443"` in the document (under
  `server:` in standalone);
- limit it to your tailnet: the same, with the machine's Tailscale address
  (`tailscale ip -4`).

See [Network access](network-access.md) for the client key and a TLS reverse
proxy. Two other defaults changed in 0.1.0 for every host and standalone:
Hugging Face and HTTP model downloads are allowed (500 GiB cap, see "Models and
downloads"), and relative model paths resolve under `~/models` unless the
document or `MLLM_MODELS_ROOT` names a models directory. On a discrete GPU,
deployments that state no residency now default to `host_backed` (see
"Discrete NVIDIA GPUs").

## Rollback

```bash
systemctl stop mllm-host
sh install.sh --system --version <previous> --systemd host
systemctl start mllm-host
```

The previous binary rewrites the managed runtime directory with its own
embedded helpers at start. Rolling back the binary alone is safe only if the
newer release did not migrate the state store (see below).

## State and migrations

The server store is SQLite with forward-only migrations
(`crates/mllm-store/src/migrations.rs`): a new release upgrades the store on
first start and no release migrates it back. An older binary refuses a store
a newer one migrated (SPEC §13.2, T33): the role exits with code 5 and error
`store_from_newer_version`, naming the store's schema version and the newest
one the binary supports, and writes nothing. The host journal is refused the
same way. The packaged units do not restart on exit code 5; the fix is the
newer binary or a restored backup. Releases before this guard did not refuse,
so rolling back to one of them runs silently against a schema it does not
know. So:

- Back up each role's state directory before an upgrade, with the role
  stopped (the tar above). Engines keep running meanwhile; the backup is
  consistent because mllm is not writing.
- Roll back across a schema change only by restoring that backup together
  with the old binary. A restored store does not know about engines launched
  after the backup was taken; drain the affected hosts first (`mllm drain
  host`), then stop the role, restore, and start it.
- Never copy state between hosts or reuse a host's state under a different
  name: identities are bound to it (SPEC §4.1). Losing a host's identity
  requires re-enrollment, not a copied directory.

Role documents under `/etc/mllm` are operator configuration; mllm never
rewrites them (SPEC §15.1), with one exception: the one-time inference
listener update of 0.1.0 described in "Upgrading to 0.1.0". Validate them with
the new binary (`mllm validate config`) before restarting.
