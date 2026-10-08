# Installing and operating CapyCTL as a service

This guide covers installing a release with `install.sh`, running each role
under systemd, upgrading, and rolling back. CapyCTL is one executable per OS and
architecture; each role runs in the foreground under a service manager.

Every setting CapyCTL reads, with its YAML field, flag and environment variable,
is listed in the [settings reference](configuration.md).

## Restart is not drain

Read this before anything else.

| Action | What happens to engines | Deployments |
|---|---|---|
| `systemctl stop` / `restart` of `capyctl-host` or `capyctl-standalone` | Keep running in their own process groups; the next start re-attaches them. | Kept. |
| `systemctl stop` / `restart` of `capyctl-server` | Keep running on their hosts; the next start reconciles them. | Kept. |
| Host crash, `Restart=on-failure` restart | Keep running; re-attached. | Kept. |
| `capyctl drain host <name>` (server running) | Stopped, with verified cleanup. | Kept, eligible for on-demand activation. |
| `capyctl drain standalone` (standalone running) | Stopped, with verified cleanup. | Kept, eligible for on-demand activation. |
| `capyctl delete deployment <id> --stop` | That deployment's engines stopped. | Deleted. |
| `capyctl revoke host <name>` (the host role exits with code 14, not restarted) | Keep running, owned and charged; dispatch to them is closed. `join host --recover` re-proves them. | Kept. |

A signal (SIGTERM, SIGINT) to any role closes admission, lets admitted
requests finish within the role document's `shutdown.drain_timeout` (30 s by
default, 0 s to 600 s), cancels what is still streaming, and exits without
touching an engine. A second signal cuts
the wait short; engines are still retained.

The units therefore have no draining `ExecStop=`. To take engines down, drain
first, then stop the unit:

```bash
# Remote host: from the server, as the service user.
sudo -u capyctl capyctl drain host gpu-box
sudo systemctl stop capyctl-host           # on gpu-box

# Standalone.
sudo -u capyctl env CAPYCTL_STATE_DIR=/var/lib/capyctl/standalone \
  capyctl drain standalone
sudo systemctl stop capyctl-standalone
```

`capyctl drain host` of an offline host returns at once with `stops: "pending"`;
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
`TimeoutStopSec=` expires, to the CapyCTL process only. systemd then logs that
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
sudo systemctl edit capyctl-host
# [Service]
# TimeoutStopSec=11min        # for drain_timeout: "600s"
```

If the timeout expires, systemd kills the CapyCTL process only; engines survive,
as with any other restart.

### Exit codes and restarts

The units restart a role that fails (`Restart=on-failure`) except on exit
codes that restarting cannot heal (`RestartPreventExitStatus=`). The table
lists those codes, plus CLI exit codes an operator is likely to meet; the
"Units" column says which units, if any, refuse to restart on each.

| Exit | Meaning | Units | What heals it |
|---|---|---|---|
| 2 | Invalid configuration | all | Fix the role document (`capyctl validate config`). |
| 3 | Unauthorized | all | Fix the identity or credentials. |
| 4 | Insufficient resources, including a GPU that cannot hold the deployment (`insufficient_device_memory`) or has no fresh reading (`device_unobserved`) | none: a CLI command's exit | Free memory, use a smaller or quantized checkpoint, or wait for the GPU to be observed. |
| 5 | Unsupported, including state written by a newer CapyCTL (`store_from_newer_version`), and on GPUs `unsupported_gpu_topology`, `multi_gpu_unsupported` and `host_backed_unavailable` | all | The newer binary or a restored backup (see "State and migrations"); for the GPU codes, see "Discrete NVIDIA GPUs". |
| 14 | The controller revoked this host (`host_revoked`) | host | Recovery under the same identity (below). |
| 15 | No allowed host is eligible for placement (`host_ineligible`) | none: a CLI command's exit (`start`), never a role's, so no unit lists it | Upgrade, undrain, reconnect or re-enroll the host the message names, then start again. |
| 16 | The path holds no `vllm`, `sglang` or `tensorfold` package (`engine_not_found`) | none: `capyctl engine` exits, never a role's | Name the venv, its `bin/vllm`, `bin/tensorfold` or its `bin/python3`, or scan more with `capyctl engine detect --path DIR`. |
| 17 | The package is not a supported engine (`engine_unsupported`) | none | Register a vLLM, SGLang or TensorFold installation. |
| 18 | The version check failed or timed out; nothing is written (`engine_version_failed`) | none | Repair the installation until its version check succeeds and matches its package metadata, then add it again. |
| 19 | The profile name is taken (`profile_exists`) | none | Use `--name`, or remove the existing profile first. |
| 20 | Removal or replacement would affect the listed deployments (`profile_in_use`) | none | Stop them, or rerun with `--drain`. |
| 21 | The server refused the re-published document (`publish_rejected`); its reason follows | none | Fix what the reason names. The profile stays in `engines.yaml`, shown as not published. |
| 22 | A role is running but its control socket did not take or answer the request (`agent_unreachable`) | none | On `add`, `engines.yaml` is written and takes effect when the role restarts. On `remove` with no role listening, the profile is removed from `engines.yaml` (exit 0, `published: role_not_running`) and the role publishes the removal when it starts. If the message says the outcome is unknown (the role took the request, then closed the connection or did not answer in time), run `capyctl engine list`, then `capyctl engine remove` again; a retry resumes the same removal. |
| 23 | `engine add` without a path needs a terminal (`not_interactive`) | none | Name the installation, or run it at a terminal to pick one. |
| 24 | No allowed host publishes the deployment's runtime profile (`profile_not_published`); nothing was stored, and the message lists each host with the profiles it publishes | none: a CLI command's exit (`deploy`), never a role's | Register the profile on a host with `capyctl engine add <path> --name <profile>`, then deploy again. A deployment is never re-resolved after `engine add`. |
| 25 | The deployment is still stopping (`still_stopping`): a `start` sent right after a `stop` arrived before the stop's cleanup was verified, or while CapyCTL was still confirming that a slow stop's engine exited; nothing was started | none: a CLI command's exit (`start`), never a role's | Retry in a moment, or run `capyctl start deployment <name> --wait`, which waits for the stop to finish and then starts. |

**A revoked host (14).** After `capyctl revoke host <name|id>`, the controller
answers the host's control session, over its mutual-TLS channel, that its
certificate is revoked. The host logs one line and exits with code 14 instead
of retrying:

```
error [host_revoked]: Host <host id> is revoked; its engines keep running. To recover the same identity, run `capyctl invite host <host id> --recover --output FILE` on the server for a new recovery invitation, then `capyctl join host --join-file FILE --recover` on this host, and start the host again
```

Its engines are neither stopped nor signalled, and its state directory and
journal are untouched. After `join host --recover` and
`systemctl start capyctl-host`, the host reconnects under the same host id and
each engine is re-proven by a fresh probe, not relaunched. Only that exact,
authenticated answer from the controller stops the host: an unreachable or
restarting server, a version refusal and any other refusal keep it
reconnecting with its backoff. The standalone role has no enrolled host to
revoke and never exits with 14; the server unit does not either.

## Release contents

A release is one self-contained binary per architecture, published on GitHub
Releases with `install.sh`. It carries these assets:

| Asset | Holds |
|---|---|
| `capyctl-<version>-linux-x86_64.tar.gz` | The x86-64 build. |
| `capyctl-<version>-linux-aarch64.tar.gz` | The ARM64 build. |
| `install.sh` | The installer (POSIX `sh`). |
| `SHA256SUMS` | SHA-256 of every tarball and of `install.sh`. |

Each tarball holds one directory:

```text
capyctl-<version>-linux-<arch>/
  bin/capyctl                  stripped release binary: every role, the CLI and
                            capyctl's Python runtime helpers (embedded)
  packaging/systemd/system/ system units
  packaging/systemd/user/   user units
  LICENSE                   Apache-2.0 license
  BUILDINFO                 version, commit, dirty flag, toolchain, source date,
                            embedded runtime manifest digest
  SHA256SUMS                digest of every file in the directory
```

There is no `runtime/` directory in the release. CapyCTL's Python helpers (the
vLLM guard and entry, the SGLang entry and its modules, the capability
probes) are compiled into `bin/capyctl` with a manifest of their SHA-256 digests, and each role that launches engines writes
them to its own state directory; see "The managed runtime directory".

Engines, engine Python environments, model weights and GPU drivers are not in
the release and are never installed by it. To install an engine yourself, see
[Install an engine](../guide/install-engines.md).

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

Building needs stable Rust and the Protocol Buffers compiler `protoc` on
`PATH`, on ARM64 as on x86-64 (`sudo apt install protobuf-compiler`, or your
distribution's package). A build over non-interactive SSH must see the same
`PATH` as your login shell.

## Installing with install.sh

The project site serves the installer; each release also publishes a copy
beside its tarballs. It downloads the release from GitHub with `curl`.

```bash
# As yourself: ~/.local/bin/capyctl (add ~/.local/bin to PATH).
curl -fsSL https://edurdias.github.io/capyctl/install.sh | sh

# A named release, and a user unit for a role (installed, not enabled).
curl -fsSL https://edurdias.github.io/capyctl/install.sh | sh -s -- --version <version> --systemd standalone

# For every user: /usr/local/bin/capyctl and system units.
curl -fsSL https://edurdias.github.io/capyctl/install.sh | sudo sh -s -- --system --version <version> --systemd host
```

A downloaded copy takes the same options: `sh install.sh --version <version>`.

**Choosing a release.** `--version` is optional (for example
`--version v0.1.2`; the leading `v` is optional). GitHub's "latest release"
never resolves to a pre-release or a draft, so a release candidate is
installed only by naming it.

**Private repository.** The public download needs no credential. While the
repository is private, log in with `gh auth login` first, or export
`GITHUB_TOKEN` with read access, and fetch the installer itself the same way
(`gh release download <tag> -R <owner>/capyctl -p install.sh`).

Without `--version` the latest published full release is installed; a draft
or a pre-release is installed only by naming it. The installer:

1. detects the OS (Linux) and architecture (`x86_64`, `aarch64`);
2. downloads the tarball and `SHA256SUMS` from the public download URL with
   `curl`; when `gh` is logged in it uses `gh release download`, and when
   `GITHUB_TOKEN` is set the GitHub API, which also work for a private
   repository (`CAPYCTL_INSTALL_BASE_URL` names a mirror directory, `https://` or
   `file://`, instead);
3. refuses to install unless the tarball's SHA-256 matches `SHA256SUMS`, every
   file in it matches the archive's own `SHA256SUMS`, and the binary reports
   the requested version;
4. replaces `<prefix>/bin/capyctl` atomically (a running role keeps its open
   executable) and keeps the units, `LICENSE` and `BUILDINFO` under
   `<prefix>/share/capyctl/`;
5. with `--systemd <server|host|standalone>`, writes that role's unit to
   `~/.config/systemd/user/` (or `/etc/systemd/system/` with `--system`),
   pointed at the installed binary, and runs `systemctl daemon-reload`. It
   never enables or starts a unit, creates users or touches state, except
   that a user unit gets an empty `~/.local/state/capyctl` (0700) if there is
   none (see "User services").

`sh install.sh --uninstall [--system]` removes the binary, `<prefix>/share/capyctl`
and the units the installer wrote. State directories are kept.

To install from a downloaded tarball by hand instead:

```bash
V=x.y.z; A=$(uname -m)   # the version you downloaded
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf capyctl-$V-linux-$A.tar.gz
(cd capyctl-$V-linux-$A && sha256sum -c --quiet SHA256SUMS)
sudo install -m 0755 capyctl-$V-linux-$A/bin/capyctl /usr/local/bin/capyctl
```

## Layout

| Path | Owner, mode | Holds |
|---|---|---|
| `/usr/local/bin/capyctl` (`~/.local/bin/capyctl`) | root (you), 0755 | The binary. |
| `/usr/local/share/capyctl/` (`~/.local/share/capyctl/`) | root (you), 0755 | Units, `LICENSE`, `BUILDINFO` of the installed release. |
| `/etc/capyctl/<role>.yaml` | root:CapyCTL, 0640 | Role documents (operator configuration). |
| `/etc/capyctl/<role>.env` | root:CapyCTL, 0640 | Optional environment for the unit. |
| `/var/lib/capyctl/` | `capyctl:capyctl`, 0700 | State root (`StateDirectory=`); holds `server/`, `host/`, `standalone/`, `tmp/`. |
| `/var/lib/capyctl/host/runtime/` | `capyctl:capyctl`, 0700 / files 0600 | The managed runtime directory of the host role (standalone: `/var/lib/capyctl/standalone/runtime/`). Written by CapyCTL. |
| model store (`/srv/models`) | readable by `capyctl` | Checkpoints. Read-only to the host unit by default. |

The state directory and every directory above it must be owned by root or the
user the role runs as, and must not be group- or world-writable. A role refuses
any other location with `unsafe controller lock path`, by design: another user
who can write there could replace the lock. That rules out `/tmp`; for a test
run, use a directory under your home, such as `mkdir -m 700 ~/capyctl-test` with
`--state-dir ~/capyctl-test/state`.

### The managed runtime directory

The engine imports CapyCTL's own Python from the runtime directory, so a module
another account can rewrite runs as the engine behind the controls it is meant
to guard. The binary therefore writes that directory
itself:

- **Where.** A host whose document does not name `runtime_dir` uses
  `<state_dir>/runtime`. Standalone uses `<state root>/runtime` unless
  `CAPYCTL_RUNTIME_DIR` is set. The server launches no engine and has none.
- **When.** `capyctl init host` writes it; every `capyctl start host` and
  `capyctl start standalone` checks it before anything can launch.
- **How.** A 0700 directory owned by the service user, each module 0600,
  and a marker file `.capyctl-managed-runtime` naming the embedded manifest. It
  is built in a sibling directory and renamed into place, so a launch never
  sees a partial tree.
- **Upgrade.** A binary with a different embedded manifest replaces the tree
  at start and logs `runtime directory ... refreshed`. Engines already running
  keep the modules they imported; the new ones apply to launches from then on.
- **Tampering.** A managed tree whose modules, modes or entries differ from
  the manifest (an edited module, a `__pycache__`, a loosened mode, a deleted
  file) is restored from the embedded copy at start, with a warning naming
  what differed (never contents).
- **Not CapyCTL's.** A directory at that path without the marker is refused, not
  overwritten: remove it, or name it as `runtime_dir`. A directory named by
  `runtime_dir` or `CAPYCTL_RUNTIME_DIR` is never written; CapyCTL only checks it.

Every launch still passes the integrity check: the directory, every subdirectory
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
(`init` otherwise fails with "Role identity refused:" and names the path
and the check that failed, for example a parent other users can write, as
anywhere under `/tmp`). CapyCTL creates the directories
itself; do not place `/var/lib/capyctl` behind a symlink or under a
group-writable directory.

## First installation (system service)

Run as root on each machine.

```bash
# Service user with a private group; its home is the state root, so engine
# caches under $HOME (triton, flashinfer, ...) land in private state.
useradd --system --user-group --home-dir /var/lib/capyctl --shell /usr/sbin/nologin capyctl
install -d -o capyctl -g capyctl -m 0700 /var/lib/capyctl

# The binary and the role's unit (host shown; server and standalone alike).
sh install.sh --system --version <version> --systemd host
install -d -m 0750 -g capyctl /etc/capyctl
```

The service user also needs read access to the engine installations named in
the host document and to the model store, and access to the GPU device nodes
(on most NVIDIA installs they are world-accessible; otherwise add
`SupplementaryGroups=` in a drop-in). The units set `NoNewPrivileges=`, so a
setuid helper such as `nvidia-modprobe` cannot create missing device nodes from
inside the service; make sure the driver's device nodes exist at boot
(for example with `nvidia-persistenced`).

Services log one JSON object per line, because their output is not a
terminal. Read them as they come with `journalctl -u capyctl-host -o cat`, or
pretty-printed with `journalctl -u capyctl-host -o cat | jq`. For text in the
journal, add `--format text` to `ExecStart=` in a drop-in.

### Server

```bash
sudo -u capyctl env CAPYCTL_STATE_DIR=/var/lib/capyctl/server \
  capyctl init server --output /var/lib/capyctl/server/config/server.yaml
install -m 0640 -o root -g capyctl /var/lib/capyctl/server/config/server.yaml /etc/capyctl/server.yaml
# Edit /etc/capyctl/server.yaml: bootstrap and control listeners, enrollment
# addresses, shutdown.drain_timeout (see docs/examples/server.yaml).
capyctl validate config --file /etc/capyctl/server.yaml
systemctl enable --now capyctl-server
```

`init` creates the server identity and credentials under the state directory
(owner-only); it prints file locations, never secrets. Client commands on the
server machine run as the service user and need no `--config`: the running
server records the document it was started with, and they use it (see
[Which role a command uses](configuration.md#which-role-a-command-uses)):

```bash
sudo -u capyctl capyctl list hosts
sudo -u capyctl capyctl invite host gpu-box --output gpu-box.join
```

### Host

```bash
sudo -u capyctl env CAPYCTL_STATE_DIR=/var/lib/capyctl/host \
  capyctl init host --output /var/lib/capyctl/host/config/host.yaml
install -m 0640 -o root -g capyctl /var/lib/capyctl/host/config/host.yaml /etc/capyctl/host.yaml
# The generated document validates as written: its resource_policy is derived
# from this machine's memory and GPUs as standalone derives its own, and models
# live in ~/models of the service user (downloads in ~/models/sources) unless
# model_store names another directory (see "Models and downloads").
# Edit /etc/capyctl/host.yaml for name, ingress (the address the server forwards
# inference to) and, if you like, the limits (see docs/examples/host.yaml).
# Leave runtime_dir out.
capyctl validate config --file /etc/capyctl/host.yaml

# Enroll with an invitation created on the server (`capyctl invite host`).
sudo -u capyctl capyctl join host --join-file gpu-box.join --config /etc/capyctl/host.yaml
systemctl enable --now capyctl-host
```

`init host` prints the managed `runtime_dir` it wrote
(`/var/lib/capyctl/host/runtime`). Name `runtime_dir` in the document only to run
from a directory you maintain yourself.

### Standalone

Without `--config`, `capyctl start standalone` loads its role document from
`<CAPYCTL_STATE_DIR>/config/standalone.yaml`, generating it (and the protected
credentials) on first start, and writes the managed runtime to
`<CAPYCTL_STATE_DIR>/runtime`. Its engine installation comes from a flag, the
environment or the document's `host.local_engine` (see the
[settings reference](configuration.md#engine-installation)); with the
packaged unit the environment is simplest. Put it in
`/etc/capyctl/standalone.env`:

```bash
# /etc/capyctl/standalone.env (root:capyctl 0640)
CAPYCTL_VLLM_BIN=/opt/vllm/bin/vllm
# Optional: models are in ~/models of the service user unless named here.
CAPYCTL_MODELS_ROOT=/srv/models
```

```bash
systemctl enable --now capyctl-standalone
```

The unit sets `CAPYCTL_STATE_DIR=/var/lib/capyctl/standalone`. Operator commands must
use the same state directory (and the same `CAPYCTL_MANAGEMENT_ADDR`, if the unit
moves the management listener with the variable rather than the document): `sudo -u capyctl env CAPYCTL_STATE_DIR=/var/lib/capyctl/standalone capyctl status deployment <id>`.
`CAPYCTL_RUNTIME_DIR` (development) makes standalone run from that directory
instead of the managed one.

#### Explicit standalone document

`capyctl start standalone --config <file>` uses `<file>` as the role document
instead. A missing or invalid explicit file refuses the
start with exit code 2; it is never replaced by the generated default, and
nothing is written under `<CAPYCTL_STATE_DIR>/config`. On a state root that has
never served, the first start creates the protected credentials there, as a
first implicit start would; a state root that has served and lost its
credentials refuses instead.

Where each setting comes from, highest precedence first:

| Setting | Source |
|---|---|
| Role document | `--config <file>`, else `$CAPYCTL_CONFIG`, else `<state root>/config/standalone.yaml`, else generated there. |
| Registered engines (`engines.yaml`) | Beside the document named by `--config` or `$CAPYCTL_CONFIG`, else `$XDG_CONFIG_HOME/capyctl/engines.yaml` (`~/.config/capyctl/engines.yaml`). `capyctl engine` uses the same rule, so it and the running role read the same file. A host follows the same rule. |
| State root | `--state-dir`, else `CAPYCTL_STATE_DIR`, else the top-level `state_dir` of the document named by `--config` or `$CAPYCTL_CONFIG`, else `$XDG_STATE_HOME/capyctl`, else `~/.local/state/capyctl`. The document may state `server.state_dir` and `host.state_dir` only as `<state root>/server` and `<state root>/host` (relative paths resolve against the document's directory); any other value is refused. |
| Listener addresses | Inference: `--listen`, else `CAPYCTL_INFERENCE_ADDR`, else `server.listeners.inference.bind`, else `0.0.0.0:8443`. Management: `--management-listen`, else `CAPYCTL_MANAGEMENT_ADDR` (`CAPYCTL_STANDALONE_MANAGEMENT_ADDR` is still read, with a warning), else `server.listeners.management.bind`, else `127.0.0.1:7443`; loopback only. |
| Engine installation | `--vllm-bin` / `--sglang-bin` and the other engine flags, else `CAPYCTL_VLLM_BIN` / `CAPYCTL_SGLANG_BIN` and the other variables, else `host.local_engine`, `host.runtime_dir` and `host.resource_policy.endpoint_port_range`; plus engines registered with `capyctl engine add`. See the [settings reference](configuration.md#engine-installation). |
| Models directory | `--models-root`, else `CAPYCTL_MODELS_ROOT`, else `host.model_store.path`, else `~/models` (created). See "Models and downloads". |
| Model downloads | `--model-sources` / `--model-sources-max`, else `CAPYCTL_MODEL_SOURCES` / `CAPYCTL_MODEL_SOURCES_MAX`, else `host.model_sources`, else allowed with a 500 GiB cap. |
| Any other setting of the document | `--set <path>=<value>`, else `CAPYCTL_SET__<PATH>`, else the document, else its default. `capyctl config show` prints every effective value and where it came from. See the [settings reference](configuration.md#any-setting-by-its-path). |
| Drain bound, switching, observability | The role document in use. |

The packaged units start standalone without `--config`, so an upgrade that
reinstalls them never depends on a file the operator has not written. To keep
the document under `/etc/capyctl` instead, write it (a generated one is a good
start), validate it, and override `ExecStart=` in a drop-in:

```bash
install -m 0640 -o root -g capyctl /var/lib/capyctl/standalone/config/standalone.yaml /etc/capyctl/standalone.yaml
capyctl validate config --file /etc/capyctl/standalone.yaml
systemctl edit capyctl-standalone
#   [Service]
#   ExecStart=
#   ExecStart=capyctl start standalone --config /etc/capyctl/standalone.yaml
systemctl restart capyctl-standalone
```

### Models and downloads

Standalone and enrolled hosts resolve the same two settings by the same rule,
highest precedence first: the flag on `capyctl start standalone` or
`capyctl start host`, then the environment, then the YAML document (the host
document, or the `host:` block of the standalone document), then the default.

| Setting | Flag | Variable | YAML | Default |
|---|---|---|---|---|
| Models directory (relative model paths resolve here) | `--models-root <dir>` | `CAPYCTL_MODELS_ROOT` | `model_store.path` | `~/models` |
| Hugging Face and HTTP downloads | `--model-sources allowed\|disabled` | `CAPYCTL_MODEL_SOURCES` | `model_sources.huggingface`, `model_sources.http` | `allowed` |
| Cap on all downloaded models | `--model-sources-max <size>` | `CAPYCTL_MODEL_SOURCES_MAX` | `model_sources.max_bytes` | `500GiB` |
| Where downloads are kept | `--model-sources-path <dir>` | `CAPYCTL_MODEL_SOURCES_PATH` | `model_sources.path` | the models directory (`~/models/sources`) |
| Hugging Face endpoint | `--hf-endpoint <url>` | `CAPYCTL_HF_ENDPOINT`, else `HF_ENDPOINT` | `model_sources.huggingface_endpoint` | `https://huggingface.co` |
| Hugging Face token for a source that names none (never a flag) | | `CAPYCTL_HF_TOKEN`, else `HF_TOKEN` | `model_sources.huggingface_token_file` | none |

A deployment that names a pinned Hugging Face revision or an HTTP URL with its
SHA-256 is downloaded by the host it is placed on, into
`<downloads>/sources/...`, verified, and then started like a local checkpoint;
`deploy model --activate --wait` waits for the download. Before a byte is
written the host reserves the download's full size against the cap and against
the free space of the filesystem, keeping 1 GiB free; a download that does not
fit is refused (`too_large` or `insufficient_space`, shown by
`capyctl status deployment`). A document that states `huggingface: disabled` (or
`denied`) keeps that kind off; `allowed_hosts` and `huggingface_endpoint` still
narrow where downloads may come from. The role writes the values it resolved
into the host document it publishes, so the server plans against exactly what
the host enforces. `capyctl prune sources --host-config <host.yaml>` reclaims
downloads no deployment references.

### User services

`packaging/systemd/user/` holds the same three roles for the per-user manager,
for a single operator account without a dedicated service user. They run
`~/.local/bin/capyctl`, read `~/.config/capyctl/<role>.yaml` and `<role>.env`, and
keep state, including the managed runtime directory, under
`~/.local/state/capyctl`. The ancestor rules above apply: with a umask of 002,
`~/.local` and `~/.local/state` are created group-writable and must be fixed
first (`chmod go-w ~ ~/.local ~/.local/state`). The standalone user unit
leaves `CAPYCTL_STATE_DIR` unset, so it and your shell both use
`~/.local/state/capyctl`.

```bash
sh install.sh --version <version> --systemd host
systemctl --user enable --now capyctl-host
loginctl enable-linger "$USER"   # keep it running after logout
```

The state root must exist as a real directory before the unit first starts.
systemd 254 and later, finding `~/.local/state/capyctl` missing while
`~/.config/capyctl` (where the units read `<role>.yaml` and `<role>.env`)
exists, assumes its pre-254 layout and makes `~/.local/state/capyctl` a symlink
to `~/.config/capyctl` ("creating compatibility symlink" in the journal). State
would then land in the configuration directory, behind a symlink the roles'
identity rules refuse. `install.sh --systemd <role>` creates the empty
directory for you and warns if the link already exists; to repair a link,
stop the unit, `rm ~/.local/state/capyctl` (the link only), move anything CapyCTL
wrote under `~/.config/capyctl` back out, and reinstall. Observed with systemd
255.

User units carry no file-system sandboxing: `ProtectSystem=` and similar need
privileges the per-user manager lacks (systemd.exec(5)). Prefer the system
units on shared machines.

## Discrete NVIDIA GPUs

CapyCTL runs on machines whose GPU has its own memory (a GeForce, RTX or data
center card) as well as on unified-memory machines such as the GB10, where the
GPU and the CPU share one pool. The steps are the same on both; this section
covers what differs.

**Requirements.** The NVIDIA driver with `nvidia-smi` at `/usr/bin/nvidia-smi`
(or `/bin/nvidia-smi`), which every driver package installs, or else in a
directory on the role's `PATH` (see "Running in a container"). CapyCTL runs it
with a cleared environment and a 3 s bound to read each GPU's index, UUID, PCI
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

The `system` domain's managed limit and free reserve are settings
(`host.resource_policy.memory.system`, see
[configuration](configuration.md#standalone-memory-limits)); on a GB10 they set
the single `unified` domain.

The reserve on the card leaves room for a desktop session on a workstation
GPU. Memory other programs already hold on the card lowers what CapyCTL sees as
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
otherwise); leaving them out and letting CapyCTL derive them is the portable form.

**One GPU per model; CapyCTL picks it.** On a machine with several GPUs, each GPU
is its own domain. CapyCTL places a new instance on the GPU where it fits with the
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
several times faster than a reload from disk. CapyCTL chooses it when the copy
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
deployment's card allocation plus its reserve: at deploy, when the size CapyCTL
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

Commands print text unless `--json` (or `--format json`) is given, whether or
not the output is a terminal. Commands that read records print an aligned
table: `list hosts`, `list deployments`, `list engines`, `status deployment`,
`engine list` and `engine detect`. Commands that change something (`deploy`,
`start`, `stop`, `drain`, `revoke`, `engine add`, ...) and `inspect`,
`validate` and `prune` print a short summary with key-value details. Hosts
appear by name (by id when they have none), memory in GiB and timeouts in
seconds. Nested detail (latency distributions, installation fingerprints,
development-control marks) is only in the JSON.

    $ capyctl list engines
    HOST      PROFILE   ENGINE   VERSION   CUSTOM   DEEP PARK   STATE    DEPLOYMENTS
    gpu-box   vllm      vllm     0.29.0    no       enabled     online   -

Scripts pass `--json`: the command then prints its JSON result, the same
document earlier releases printed (indented on a terminal, one compact line when piped or redirected), and reports errors as JSON on stderr.
`--output json` is still accepted and means the same. `--format text` asks for
the default explicitly. The roles (`start server`, `start host`, `start
standalone`) print text on a terminal and one JSON object per line otherwise,
so the journal and log files are JSON. Exit codes do not depend on the
format.

## Registering engines

CapyCTL uses engines you install yourself. Register them on the machine that runs them:

    capyctl engine detect [--path DIR]        # lists vLLM/SGLang/TensorFold environments; runs nothing
    capyctl engine add ~/venvs/vllm           # or its bin/vllm, or bin/python3 for SGLang
    capyctl engine add ~/sglang/bin/python3 --name sglang-patched --drift refuse
    capyctl engine list
    capyctl engine remove vllm [--drain]
    capyctl list engines                      # every host's engines on a server; this machine's on standalone

`detect` looks in PATH environments, conda, `~/venvs`, `~/.venv`,
`~/.virtualenvs`, uv and pipx tool environments, `/opt`, and any venv directly
in your home directory (for example `~/capyctl-vllm-venv2`).

`engine add` runs the installation only after you name or pick it (a bounded
version check, the installation fingerprint and the deep-park probe), writes
the profile into `engines.yaml`, and asks the running role to publish it
without a restart. CapyCTL never rewrites `host.yaml` or `standalone.yaml`.
`engines.yaml` sits beside the role's configuration file (`--config
dir/host.yaml` means `dir/engines.yaml`). Without `--config`, on a host machine
it sits beside the document the host was started with (the host records it),
and otherwise it is `~/.config/capyctl/engines.yaml`, for a host and for
standalone alike. The role
merges it with its own document at start; a profile name declared in both is
refused. Its first line records its revision (`# capyctl-document-revision: N`).
The running role listens on `<state_dir>/control.sock` (mode 0600; only the
role's own user and root are served) for these commands. Only the `capyctl engine`
command writes `engines.yaml`; the running role only reads it.

`engine remove` removes only profiles `engine add` registered; one you wrote
into `host.yaml` stays yours to edit. It is refused while a deployment on this
machine uses the profile (`profile_in_use`); `--drain` stops those deployments
through the ordinary stop path first. The command asks the running role to
retire the profile, waits until the server confirms their stop evidence, then
rewrites `engines.yaml` without it and asks the role to publish the removal.
With no role running (no control socket, or a stale one nobody listens on),
nothing runs on the profile, so the command removes it from `engines.yaml`
under the file's lock and exits 0 with `published: role_not_running`; the role
publishes the removal when it starts. If a removal is interrupted after
the confirmation (the command was killed, the connection dropped, or the
publication failed), the profile stays out of placement on that machine; run
`capyctl engine remove <name>` again, which resumes the same removal and finishes
it.

A deployment naming a runtime profile that no allowed host publishes is refused
at `deploy` (`profile_not_published`), naming the profile and each host; run
`capyctl engine add <path> --name <profile>` on a host, then deploy again.

An older CapyCTL that shares `engines.yaml` with a newer one may find a
profile for an engine it does not support (for example `tensorfold`, which
0.1.1 added). From this release on, a role skips such a profile at start and
on reload, prints a warning that names the profile and its engine, and starts
with the rest; a deployment that names it is refused `profile_not_published`.
The file keeps the profile. Upgrade CapyCTL to use it, or remove it with
`capyctl engine remove <name>`. Releases before this one refuse to start
(`invalid_config` at `runtime_profiles.engine`); remove the profile with the
newer binary, or edit `engines.yaml`, before starting the older one.

In standalone, `CAPYCTL_VLLM_BIN` or `CAPYCTL_SGLANG_BIN` alone gives the profile
`local`; both give `local-vllm` and `local-sglang`. Profiles you add coexist
with them.

**CUDA toolkit and compile jobs.** Engines compile some GPU kernels the first
time they start. `capyctl engine add` records the CUDA toolkit as the profile's
`cuda_home`: `CUDA_HOME` if it holds `bin/nvcc`, otherwise `/usr/local/cuda`
if that holds it. For the role's own installation (`--vllm-bin`,
`CAPYCTL_VLLM_BIN` or `local_engine`, on a host or standalone), state it with
`--cuda-home`, `CAPYCTL_CUDA_HOME` or `local_engine.cuda_home`; nothing is
detected for it. The engine then gets `<cuda_home>/bin` on its PATH and
`CUDA_HOME` set; vLLM uses FlashInfer only when `nvcc` is found. Without
`cuda_home`, the engine PATH has only the engine's own `bin` and the system
directories.

Each compile job can take several GB. CapyCTL sets `MAX_JOBS` to the free memory
at launch divided by 8 GiB, at most the CPU count, and
`FLASHINFER_NVCC_THREADS=1`. The host log prints the chosen value at every
launch. To choose other limits, put `MAX_JOBS` or `FLASHINFER_NVCC_THREADS`
(positive integers) in the profile's `env`.

**With the system units, run `capyctl engine` as root with the unit's
`--config`.** The host unit reads `/etc/capyctl/host.yaml`, so its `engines.yaml`
is `/etc/capyctl/engines.yaml`. `/etc/capyctl` is root's (mode 0750, group `capyctl`),
and the unit makes `/etc` read-only to the role (`ProtectSystem=strict`), so
the role never writes there; `capyctl engine` does, and only root can:

    sudo capyctl engine add /opt/venvs/vllm --config /etc/capyctl/host.yaml
    sudo capyctl engine list --config /etc/capyctl/host.yaml
    sudo capyctl engine remove vllm --drain --config /etc/capyctl/host.yaml

Run as root, the command keeps an existing `engines.yaml`'s owner and mode, and
creates a new one (and its lock) owned by the role's service user (the owner of
the host's `state_dir`, `capyctl`), mode 0600, so the role can read it. It talks to
the role over `<state_dir>/control.sock`, which serves root as well as the
service user. Root also runs the named installation's version check and
deep-park probe, so name only an installation you trust. Keep the `--config`
here: a command finds the role running on the machine through the state root
of the user who runs it, and root's is not the service user's, so without it
root's `~/.config/capyctl/engines.yaml` is a different file than the one the role
reads. Run as the service user, `capyctl engine` finds the host without it. The packaged standalone unit
starts without `--config`, so its `engines.yaml` is the service user's
`/var/lib/capyctl/.config/capyctl/engines.yaml`, which the service user can write:
`sudo -u capyctl env CAPYCTL_STATE_DIR=/var/lib/capyctl/standalone capyctl engine add …`.

`engine add` also works before any role has ever started — the first-run path
of adding an engine, then starting the role for the first time (standalone
refuses to start with no engine). It creates the state directory the role will
use (owner-only, mode 0700) if it does not exist yet, writes `engines.yaml`, and
exits 0 with `published: role_not_running` and the line `saved to
<engines.yaml> (revision N); start capyctl (…) to use it`: no role is running (no
control socket, or a stale one nobody listens on), so the profile takes effect
at the role's first start. A second line names the control socket it tried.
A role started with another `--state-dir` or `--config` listens on another
socket, so it looks absent: restart it to publish the profile, and pass the
same option to `capyctl engine` from then on. Only a role that is running but
does not take or answer the request exits 22 (`agent_unreachable`).

`--config` and `$CAPYCTL_CONFIG` may be relative: every command and role resolves
them against its working directory first, so `capyctl engine add … --config
host.yaml` run beside `host.yaml` writes the `engines.yaml` next to it and asks
the running role to publish it.

## Engine logs and troubleshooting

Each launch has an owner-only (0600) log, `<state dir>/logs/<deployment
id>/<launch id>.log`, for every engine. It holds the engine's own output at the
engine's default level (info for SGLang, vLLM and TensorFold), redacted as it
is written:

- The engine's output goes through a small writer process (the `capyctl`
  binary itself, in its own process group) before it reaches the file. The
  writer replaces the launch's own credentials wherever they appear (the
  engine and admin keys, an observation credential, any secret-named variable
  of the engine's environment), and, by rule, URL credentials (`user:pass@`),
  URL query strings and fragments (presigned URLs), secret-named assignments
  (`api_key=…`, `"password": …`, `Authorization: …`), Hugging Face tokens,
  bearer values and long credential-shaped runs. Each becomes `<redacted>`.
- The log rotates at 16 MiB to `<launch id>.log.1` and `.log.2`; older output
  is dropped, so one launch's log never exceeds 48 MiB.
- No engine logs prompts or completions: request logging stays off. SGLang
  runs with `log_requests` off and no crash dump folder; vLLM's
  `--enable-log-requests` is reserved and off by default; TensorFold has no
  request logging. Per-request lines carry the method, path and status only.

Read the end of an instance's log through the management API, on the
management listener, with the admin token from the role's credentials file
([configuration](configuration.md)):

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  "http://<management listener>/management/v1/deployments/<id>/engine-log?instance=0&kib=64"
```

`kib` defaults to 64 and may be 1 to 256; `instance` (the index `status
deployment` shows) may be left out when the deployment has one instance. The
answer, `{deployment_id, instance, host_id, incarnation, kib, truncated, text,
read_at_ms}`, holds at most `kib` KiB of whole lines, redacted again on the way
out, and `truncated: true` when older output exists. The read continues into
`<launch id>.log.1` when the current file is short. Refusals: `400
invalid_request` (a bad query, or no `instance` for a deployment of several),
`404 not_found` (unknown deployment or instance, no running launch, or no log
yet), `403 forbidden` (a `--debug-engine-logs` log), `503
unsupported_capability` (`host_capability_missing:engine_log_tail`: the host
runs an older CapyCTL), `503 observation_stale` (the host is not connected),
`504 deadline_exceeded` (no answer within 15 seconds) and `429 queue_full`
(two reads already running). No answer carries a file path.

`--debug-engine-logs` on `capyctl start host` or `capyctl start standalone`
raises SGLang to debug level and writes every engine's full output to the log
directly, without the writer, for launches from then on. That output may
contain prompts and secrets, so the management API never serves it (it answers
that the log was written under `--debug-engine-logs`). It is a flag only: a
variable left in a unit file must not turn it on. For a unit, add it to
`ExecStart=` in a drop-in while you investigate, then remove it.

- **A launch fails.** `capyctl status deployment <name>` shows the reason in
  `LAST OPERATION` (for example `initialize failed (launch_failed)`); the
  instance's `LAST ERROR` column can still read `-`. `--format json` has the
  full record. A request for the deployment answers `activation_failed`, and
  each new request tries a fresh start. The engine log says why the engine
  exited.
- **SGLang saver permission warning.** Before an SGLang park, CapyCTL checks
  that the engine's `torch_memory_saver` library is not writable by other
  accounts. When that cannot be proven (for example, a group-writable
  environment on a machine whose groups come from a directory service such as
  `sss`), CapyCTL parks anyway and writes one line to the engine log:
  `{"event":"capyctl_saver_library_permissions","problem":"group_undetermined","action":"warned"}`.
  The line is only in the engine log. To clear it, remove group and other
  write permission from the engine's environment (`chmod -R go-w
  <environment>`); CapyCTL never
  changes engine files itself.
- **`config show` does not match a running role.** `capyctl config show` reads
  the document, the environment of the shell it runs in and the `--set`
  options given to it. It does not ask the running role, so a role started
  with `--set`, a flag, or a unit's `.env` file shows those values only if you
  pass the same `--set` options or variables to `config show`.

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
  engines it re-attaches. Instead `TMPDIR=/var/lib/capyctl/tmp` keeps temporary
  files private, and `/tmp`, `/var/tmp` stay writable for engine code that
  ignores `TMPDIR`.

Writable paths are the state root and `/tmp`, `/var/tmp`. If a recipe must
write into the model store, add `ReadWritePaths=` for it in a drop-in. These
restrictions may need loosening for some engine builds; if an engine fails to
start under the unit but starts by hand, check these first.

## Running in a container

A host or standalone role can run as the main process of a container. CapyCTL
publishes no image: build one from a base that carries the NVIDIA user-space
tools your engine needs, add the `capyctl` binary and the engine virtual
environments, and start the role in the foreground. Not yet run live in a
container; the behaviour below is covered by CPU tests only.

**GPUs.** Inject them with the NVIDIA Container Toolkit, either through CDI
(`--device nvidia.com/gpu=all` with Podman, or Docker with CDI enabled) or with
`docker run --gpus all`. Either injects the device nodes, the driver libraries
and `nvidia-smi`. CapyCTL looks for `nvidia-smi` at `/usr/bin/nvidia-smi`,
then `/bin/nvidia-smi`, then in each absolute directory on the role's `PATH`,
so an image that installs it elsewhere works when that directory is on
`PATH`. Each run is still bounded to 3 s, with a cleared environment, so the
NVIDIA libraries must be on the default library path (the toolkit's injection
registers them). Without `nvidia-smi` the role sees no GPU: a discrete card is
unobserved, and an SGLang deployment fails placement.

**Mounts.** The paths inside the container follow the same rules as on a
host (see "Layout"):

| Mount | Access | Notes |
|---|---|---|
| State root (`CAPYCTL_STATE_DIR`) | read-write | Must persist across container restarts (identity, journal, ledger, logs, the managed runtime). The directory and its ancestors must be owned by root or the role's user and not group- or world-writable: prepare a host directory with `install -d -m 0700 -o <uid> -g <gid>`, since a new named volume is root-owned. |
| Models directory (`CAPYCTL_MODELS_ROOT`) | read-only, or read-write when downloads are allowed | Downloads land in `<models>/sources` unless `CAPYCTL_MODEL_SOURCES_PATH` names another mount. |
| Engine virtual environments | read-only | Mount each at the path its profile or `CAPYCTL_*_BIN` names. Owned by root or the role's user, nothing writable by others. |
| `/dev/shm` | | Engines pass tensors through shared memory; give the container `--ipc=host` or a `--shm-size` of several GiB (Docker's default 64 MiB is too small). |

**User and capabilities.** Run the role as the user that owns the state
directory (`--user <uid>:<gid>`), or as root. It needs no added capabilities
and no privileged mode: it signals only its own engines, and making itself a
child subreaper needs no privilege.

**`--init` is not required.** At start a host or standalone role makes itself a
child subreaper, so the processes its engines leave behind are handed to it,
and it reaps them; as PID 1 of a container without an init it reaps every
orphan there. A process it reaped reads gone, so a stop's verified cleanup
completes. An exited process whose parent is another live process that never
waits for it is reported (`... exited but its parent has not reaped it (a
zombie); ownership is retained`) rather than read as running. `--init`
(tini) still works and changes nothing: CapyCTL remains the reaper of its own
engines' processes. The role handles SIGTERM itself, as PID 1 too.

**Stopping.** Engines do not outlive the container: when the role exits the
container's PID namespace ends and the kernel kills every engine in it. Drain
first (`docker exec <container> capyctl drain standalone`), then stop, with a
stop timeout above `shutdown.drain_timeout` (`docker stop -t 60`).

**`/proc`.** Use the container's own `/proc` as the runtime mounts it. CapyCTL
proves an engine's processes gone from `/proc`, so it refuses to observe
(and cleanup stays retained) when `/proc` is mounted with `hidepid=` (other
than `hidepid=0`) or `subset=`, when there is more than one `/proc` mount, or
when something is mounted over a `/proc/<pid>` path. The runtime's masking of
files such as `/proc/kcore` is fine. Do not bind-mount the host's `/proc`.

**Memory limits.** With cgroup v2, a container memory limit (`--memory`, or
any `memory.max` above the role's cgroup) bounds what the role observes: the
capacity is the smaller of the host's memory and the tightest `memory.max` of
its cgroup and every ancestor, and the available memory the smaller of
`MemAvailable` and that cgroup's limit minus its usage (its inactive page cache
counted as free, since the kernel reclaims it first). Standalone derives its
limits from that capacity, and on a unified-memory machine (GB10) the single
`unified` domain follows the same bound. At start the role says which cgroup
bounds its memory, and `capyctl inspect host <name> --json` shows
`memory_source` on each host domain: `meminfo`, or `cgroup_v2:<cgroup>`.
cgroup v1 limits are not
read: on a v1 host the role warns at start (`memory_source`
`meminfo:cgroup_v1_unread`) and reads the whole machine, so state the limits in
the role document instead (see
[configuration](configuration.md#standalone-memory-limits)).
`meminfo:cgroup_v2_unreadable` means the role is in a v2 cgroup whose limits it
could not read under `/sys/fs/cgroup`. The JIT compile-job count (`MAX_JOBS`)
is still sized from the machine's `MemAvailable`; in a tightly limited container,
set `MAX_JOBS` in the engine env.

A standalone role on a GB10, as an example:

```bash
install -d -m 0700 -o 1000 -g 1000 /srv/capyctl-state
docker run -d --name capyctl --gpus all --ipc=host --memory 100g \
  --user 1000:1000 \
  -v /srv/capyctl-state:/var/lib/capyctl \
  -v /srv/models:/models:ro \
  -v /opt/vllm:/opt/vllm:ro \
  -e CAPYCTL_STATE_DIR=/var/lib/capyctl \
  -e HOME=/var/lib/capyctl \
  -e CAPYCTL_MODELS_ROOT=/models \
  -e CAPYCTL_MODEL_SOURCES=disabled \
  -e CAPYCTL_VLLM_BIN=/opt/vllm/bin/vllm \
  -p 8443:8443 \
  <image> capyctl start standalone
docker exec capyctl capyctl list hosts
```

`HOME` points at the state root, as the service user's home does under systemd,
so the engines' caches (Triton, FlashInfer) land in persistent private state.
The management listener stays on the container's loopback; run client
commands with `docker exec` as above.

## Upgrade

A restart re-attaches running engines, so an upgrade does not need a drain.
The one exception is the first start of 0.1.0 on a discrete-GPU machine that
ran an earlier release: it stops that machine's engines once (see
"Upgrading to 0.1.0").

**Order: the server first, then the hosts one at a time**. The
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
`capyctl list hosts` (its `VERSION` and `COMPATIBILITY` columns; with
`--format json`, `server_version`, and per host `binary_version`,
`compatibility`, `compatibility_reason`); `capyctl status deployment <id>
--format json` shows the same per allowed host. A host listing a `capabilities_missing` entry that a
launch needs is left out of placement; the operation it lacks is refused as
`host_capability_missing:<name>`.

Release rule: a change that affects the protocol or durable state ships only
in a minor (or major) release; a patch release never changes the protocol,
so patch releases of server and hosts mix freely.

```bash
systemctl stop capyctl-host                       # engines keep serving

# Back up state (see "State and migrations").
tar -C /var/lib/capyctl -czf /var/backups/capyctl-host-$(date +%Y%m%d%H%M).tar.gz \
  --warning=no-file-ignored host

# Replace the binary (and refresh the installed unit).
sh install.sh --system --version 0.2.0 --systemd host
capyctl validate config --file /etc/capyctl/host.yaml

systemctl start capyctl-host                      # re-attaches running engines
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
(`capyctl status deployment <id>`). If one is not, read its status reason before
acting on it.

### Upgrading to 0.1.0

Newly generated documents put the inference endpoint on all interfaces,
`0.0.0.0:8443`, with the API key required. CapyCTL never rewrites an existing
document: one an earlier release generated keeps `127.0.0.1:8443` until you
change it. To open it to the network, set `listeners.inference.bind` (under
`server:` in standalone) to `0.0.0.0:8443` or start with
`--listen 0.0.0.0:8443`.

To set the address, pick one:

- keep inference on the machine: `--listen 127.0.0.1:8443`, or
  `CAPYCTL_INFERENCE_ADDR=127.0.0.1:8443` in the unit's `.env` file, or
  `listeners.inference.bind: "127.0.0.1:8443"` in the document (under
  `server:` in standalone);
- limit it to your tailnet: the same, with the machine's Tailscale address
  (`tailscale ip -4`).

See [Network access](network-access.md) for the client key and a TLS reverse
proxy.

**Discrete GPUs: a one-time policy change and cold restart.** Earlier
releases described every machine as one unified memory pool. On a machine
with a discrete card, 0.1.0 derives a `system` domain (host RAM) and one
`gpuN` domain per card instead. For standalone, the first start of 0.1.0
replaces the resource policy it generated and prints:

```
this machine's memory shape changed, so the resource policy capyctl generated for it was replaced (revision 2): domains [unified] are now [gpu0, system]; stopped with verified cleanup first: <deployments>
re-sized for this machine's resource policy: <deployments>
```

To make that change, the start stops the engines the earlier release
launched on this machine, waiting until each is proven gone, and accepts each
deployment again as a new revision sized for the card (so its `REVISION`
goes up by one). The deployments stay eligible for on-demand activation: the
next request for each starts it cold. A restart does not re-attach them this
one time. If an engine cannot be proven stopped in time, the start fails,
keeps the earlier accounting and says so; start it again to retry. A
deployment that cannot be sized for the card as written is named in the
notice and must be deployed again with a file for this machine. This runs
once; later starts re-attach as usual. Unified-memory machines are not
changed. An enrolled host states its domains in its own document, which CapyCTL
never rewrites; for a discrete card, write it in the shape of
[`examples/host-discrete.yaml`](../examples/host-discrete.yaml).

Two other defaults changed in 0.1.0 for every host and standalone:
Hugging Face and HTTP model downloads are allowed (500 GiB cap, see "Models and
downloads"), and relative model paths resolve under `~/models` unless the
document or `CAPYCTL_MODELS_ROOT` names a models directory. On a discrete GPU,
deployments that state no residency now default to `host_backed` (see
"Discrete NVIDIA GPUs").

## Rollback

```bash
systemctl stop capyctl-host
sh install.sh --system --version <previous> --systemd host
systemctl start capyctl-host
```

The previous binary rewrites the managed runtime directory with its own
embedded helpers at start. Rolling back the binary alone is safe only if the
newer release did not migrate the state store (see below).

## State and migrations

The server store is SQLite with forward-only migrations: a new release upgrades the store on
first start and no release migrates it back. An older binary refuses a store
a newer one migrated: the role exits with code 5 and error
`store_from_newer_version`, naming the store's schema version and the newest
one the binary supports, and writes nothing. The host journal is refused the
same way. The packaged units do not restart on exit code 5; the fix is the
newer binary or a restored backup. Releases before this guard did not refuse,
so rolling back to one of them runs silently against a schema it does not
know. So:

- Back up each role's state directory before an upgrade, with the role
  stopped (the tar above). Engines keep running meanwhile; the backup is
  consistent because CapyCTL is not writing.
- Roll back across a schema change only by restoring that backup together
  with the old binary. A restored store does not know about engines launched
  after the backup was taken; drain the affected hosts first (`capyctl drain
  host`), then stop the role, restore, and start it.
- Never copy state between hosts or reuse a host's state under a different
  name: identities are bound to it. Losing a host's identity
  requires re-enrollment, not a copied directory.

Role documents under `/etc/capyctl` are operator configuration; CapyCTL never
rewrites them, with one exception: the one-time inference listener update
described in "Upgrading to 0.1.0". Validate them with the new binary
(`capyctl validate config`) before restarting.
