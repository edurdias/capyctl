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
| 5 | Unsupported, including state written by a newer mllm (`store_from_newer_version`) | all | The newer binary or a restored backup (see "State and migrations"). |
| 14 | The controller revoked this host (`host_revoked`) | host | Recovery under the same identity (below). |
| 15 | No allowed host is eligible for placement (`host_ineligible`) | none: a CLI command's exit (`start`), never a role's, so no unit lists it | Upgrade, undrain, reconnect or re-enroll the host the message names, then start again. |

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

A release (owner decision 2026-09-24: one self-contained binary, GitHub
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
of the private repository `edurdias/mllm` (owner decision
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
# Edit /etc/mllm/host.yaml: name, model_store, ingress, resource_policy,
# runtime_profiles (see docs/examples/host.yaml). Leave runtime_dir out.
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
`<MLLM_STATE_DIR>/runtime`. Its engine installation always comes from the
environment. Put that environment in `/etc/mllm/standalone.env`:

```bash
# /etc/mllm/standalone.env (root:mllm 0640)
MLLM_VLLM_BIN=/opt/vllm/bin/vllm
MLLM_MODELS_ROOT=/srv/models
```

```bash
systemctl enable --now mllm-standalone
```

The unit sets `MLLM_STATE_DIR=/var/lib/mllm/standalone`. Operator commands must
use the same state directory (and the same `MLLM_STANDALONE_MANAGEMENT_ADDR`,
if set): `sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/standalone mllm status deployment <id>`.
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
| Role document | `--config <file>`, else `<state root>/config/standalone.yaml`, else generated there. |
| State root | `MLLM_STATE_DIR`, else `$XDG_STATE_HOME/mllm`, else `~/.local/state/mllm`. The document may state `server.state_dir` and `host.state_dir` only as `<state root>/server` and `<state root>/host` (relative paths resolve against the document's directory); any other value is refused. |
| Listener addresses | `MLLM_STANDALONE_INFERENCE_ADDR` / `MLLM_STANDALONE_MANAGEMENT_ADDR` for one run (loopback only), else `127.0.0.1:8443` / `127.0.0.1:7443`. The document may state only those defaults. |
| Engine installation | The environment only (`MLLM_VLLM_BIN` or `MLLM_SGLANG_BIN`, `MLLM_MODELS_ROOT`, ...). |
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
`mllm list hosts` (`server_version`, and per host `binary_version`,
`compatibility`, `compatibility_reason`); `mllm status deployment <id>` shows the
same per allowed host. A host listing a `capabilities_missing` entry that a
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
rewrites them (SPEC §15.1). Validate them with the new binary
(`mllm validate config`) before restarting.
