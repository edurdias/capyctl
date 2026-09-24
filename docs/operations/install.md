# Installing and operating mllm as a service

This guide covers installing a release tarball, running each role under
systemd, upgrading, and rolling back. It follows SPEC §3.3 (one executable per
OS/architecture, ADR 0001), §4.3 (foreground roles, OS service definitions,
restart distinct from drain) and §13.3 (owner-only runtime files).

What this guide does not establish: the unit files and the tarball are checked
locally (`scripts/verify-packaging.sh`), not on a GPU host. Running a unit is
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

A signal (SIGTERM, SIGINT) to any role closes admission, lets admitted
requests finish within the role document's `shutdown.drain_timeout` (30 s by
default, 0 s to 600 s), cancels what is still streaming, and exits without
touching an engine (`crates/mllm-cli/src/shutdown.rs`). A second signal cuts
the wait short; engines are still retained.

The units therefore have no draining `ExecStop=`. To take engines down, drain
first, then stop the unit:

```bash
# Remote host: from the server, as the service user.
sudo -u mllm /opt/mllm/current/bin/mllm drain host host-a --config /etc/mllm/server.yaml
sudo systemctl stop mllm-host           # on host-a

# Standalone.
sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/standalone \
  /opt/mllm/current/bin/mllm drain standalone
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

## Release contents

`packaging/release.sh` builds `mllm-<version>-linux-<arch>.tar.gz` and a
`.sha256` beside it. The archive holds one directory:

```text
mllm-<version>-linux-<arch>/
  bin/mllm                  stripped release binary, every role and the CLI
  runtime/                  mllm's Python runtime helpers (0700 dirs, 0600 files)
  packaging/systemd/system/ system units
  packaging/systemd/user/   user units
  docs/examples/            example role and deployment documents
  docs/operations/install.md
  BUILDINFO                 version, commit, dirty flag, toolchain, source date
  SHA256SUMS                digest of every file in the directory
```

Build it from a clean, committed tree (`packaging/release.sh [OUT_DIR]`,
default `dist/`). The script builds with `--locked`, runs
`scripts/check-release-clean.sh` (no test engine may reach the shipped
binary), ships only files tracked by git (no `__pycache__`, no bytecode, no
`runtime/tests`), and stamps entries with the commit time so a rebuild of the
same commit and toolchain gives the same archive. Build on each target
architecture (x86-64, aarch64); the archive is for the architecture it was
built on.

Engines, engine Python environments, model weights and GPU drivers are not in
the release and are never installed by it (SPEC §4.2, §15.2).

## Layout

| Path | Owner, mode | Holds |
|---|---|---|
| `/opt/mllm/releases/<version>/` | root, 0755 | An unpacked release, never edited. |
| `/opt/mllm/current` | root symlink | Points at the active release; units run `/opt/mllm/current/bin/mllm`. |
| `/opt/mllm/runtime/` | `mllm:mllm`, 0700 / files 0600 | The runtime helpers engines import. A real directory, not a symlink. |
| `/etc/mllm/<role>.yaml` | root:mllm, 0640 | Role documents (operator configuration). |
| `/etc/mllm/<role>.env` | root:mllm, 0640 | Optional environment for the unit. |
| `/var/lib/mllm/` | `mllm:mllm`, 0700 | State root (`StateDirectory=`); holds `server/`, `host/`, `standalone/`, `tmp/`. |
| model store (`/srv/models`) | readable by `mllm` | Checkpoints. Read-only to the host unit by default. |

### The runtime directory rule

The engine imports mllm's own Python from the runtime directory, so a module
another account can rewrite runs as the engine behind the controls it is meant
to guard. A launch is refused unless (`crates/mllm-agent/src/runtime_integrity.rs`,
`crates/mllm-adapters/src/owner_only.rs`, SPEC §13.3):

- the directory, every subdirectory and every `.py` module are owned by the
  service user that runs the host (not root: the host accepts only its own
  user as owner of the runtime tree);
- nothing is writable by other, and group write is allowed only through the
  owning user's private group;
- there are no symlinks, and nothing importable besides `.py` source: no
  `__pycache__`/`.pyc`, `.so`, `.pth`, `.zip`;
- every ancestor directory of the SGLang entry is owned by root or the service
  user and not writable by others, and the path is canonical. This is why
  `runtime_dir` must name a real directory such as `/opt/mllm/runtime`, not a
  path through the `/opt/mllm/current` symlink.

The release keeps the runtime at 0700/0600; installing it means copying it to
`/opt/mllm/runtime` and giving it to the service user, as below. Never run
`python` against the runtime directory as another user or without `-B`: a
written `__pycache__` makes the next launch refuse until it is removed.

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
V=0.1.0; A=$(uname -m)
sha256sum -c mllm-$V-linux-$A.tar.gz.sha256

# Service user with a private group; its home is the state root, so engine
# caches under $HOME (triton, flashinfer, ...) land in private state.
useradd --system --user-group --home-dir /var/lib/mllm --shell /usr/sbin/nologin mllm
install -d -o mllm -g mllm -m 0700 /var/lib/mllm

# Unpack and verify the release.
install -d -m 0755 /opt/mllm/releases
tar -xzf mllm-$V-linux-$A.tar.gz -C /opt/mllm/releases --no-same-owner
mv /opt/mllm/releases/mllm-$V-linux-$A /opt/mllm/releases/$V
(cd /opt/mllm/releases/$V && sha256sum -c --quiet SHA256SUMS)
ln -sfn releases/$V /opt/mllm/current

# The runtime helpers, owned by the service user (see the rule above).
cp -a /opt/mllm/releases/$V/runtime /opt/mllm/runtime
chown -R mllm:mllm /opt/mllm/runtime

# Units.
install -m 0644 /opt/mllm/current/packaging/systemd/system/mllm-*.service /etc/systemd/system/
install -d -m 0750 -g mllm /etc/mllm
systemctl daemon-reload
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
  /opt/mllm/current/bin/mllm init server --output /var/lib/mllm/server/config/server.yaml
install -m 0640 -o root -g mllm /var/lib/mllm/server/config/server.yaml /etc/mllm/server.yaml
# Edit /etc/mllm/server.yaml: bootstrap and control listeners, enrollment
# addresses, shutdown.drain_timeout (see docs/examples/server.yaml).
/opt/mllm/current/bin/mllm validate config --file /etc/mllm/server.yaml
systemctl enable --now mllm-server
```

`init` creates the server identity and credentials under the state directory
(owner-only); it prints file locations, never secrets. Client commands on the
server machine run as the service user with the same document, for example
`sudo -u mllm /opt/mllm/current/bin/mllm list hosts --config /etc/mllm/server.yaml`.

### Host

```bash
sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/host \
  /opt/mllm/current/bin/mllm init host --output /var/lib/mllm/host/config/host.yaml
install -m 0640 -o root -g mllm /var/lib/mllm/host/config/host.yaml /etc/mllm/host.yaml
# Edit /etc/mllm/host.yaml: name, runtime_dir: /opt/mllm/runtime, model_store,
# ingress, resource_policy, runtime_profiles (see docs/examples/host.yaml).
/opt/mllm/current/bin/mllm validate config --file /etc/mllm/host.yaml

# Enroll with an invitation created on the server (`mllm invite host`).
sudo -u mllm /opt/mllm/current/bin/mllm join host --join-file host-a.join --config /etc/mllm/host.yaml
systemctl enable --now mllm-host
```

Set `runtime_dir` explicitly. Left out, it defaults to `<state_dir>/runtime`,
which is also acceptable if you copy the runtime there instead.

### Standalone

Without `--config`, `mllm start standalone` loads its role document from
`<MLLM_STATE_DIR>/config/standalone.yaml`, generating it (and the protected
credentials) on first start. Its engine installation always comes from the
environment. Put that environment in `/etc/mllm/standalone.env`:

```bash
# /etc/mllm/standalone.env (root:mllm 0640)
MLLM_VLLM_BIN=/opt/vllm/bin/vllm
MLLM_MODELS_ROOT=/srv/models
```

```bash
systemctl enable --now mllm-standalone
```

The unit sets `MLLM_STATE_DIR=/var/lib/mllm/standalone` and
`MLLM_RUNTIME_DIR=/opt/mllm/runtime`. Operator commands must use the same
state directory (and the same `MLLM_STANDALONE_MANAGEMENT_ADDR`, if set):
`sudo -u mllm env MLLM_STATE_DIR=/var/lib/mllm/standalone /opt/mllm/current/bin/mllm status deployment <id>`.

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
/opt/mllm/current/bin/mllm validate config --file /etc/mllm/standalone.yaml
systemctl edit mllm-standalone
#   [Service]
#   ExecStart=
#   ExecStart=/opt/mllm/current/bin/mllm start standalone --config /etc/mllm/standalone.yaml
systemctl restart mllm-standalone
```

### User services

`packaging/systemd/user/` holds the same three roles for the per-user manager,
for a single operator account without a dedicated service user. They run
`~/.local/opt/mllm/current/bin/mllm`, read `~/.config/mllm/<role>.yaml` and
`<role>.env`, keep state under `~/.local/state/mllm`, and expect the runtime
at `~/.local/opt/mllm/runtime` (owned by that user). The ancestor rules above
apply: with a umask of 002, `~/.local` and `~/.local/state` are created
group-writable and must be fixed first (`chmod go-w ~ ~/.local ~/.local/state`).
The standalone user unit leaves
`MLLM_STATE_DIR` unset, so it and your shell both use `~/.local/state/mllm`.

```bash
install -m 0644 ~/.local/opt/mllm/current/packaging/systemd/user/mllm-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now mllm-host
loginctl enable-linger "$USER"   # keep it running after logout
```

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
restrictions are not qualified on the Sparks; the first live run under the
units is the check.

## Upgrade

A restart re-attaches running engines, so an upgrade does not need a drain.

```bash
V=0.2.0; A=$(uname -m)
sha256sum -c mllm-$V-linux-$A.tar.gz.sha256
tar -xzf mllm-$V-linux-$A.tar.gz -C /opt/mllm/releases --no-same-owner
mv /opt/mllm/releases/mllm-$V-linux-$A /opt/mllm/releases/$V
(cd /opt/mllm/releases/$V && sha256sum -c --quiet SHA256SUMS)
/opt/mllm/releases/$V/bin/mllm validate config --file /etc/mllm/host.yaml

systemctl stop mllm-host                       # engines keep serving

# Back up state (see "State and migrations").
tar -C /var/lib/mllm -czf /var/backups/mllm-host-$(date +%Y%m%d%H%M).tar.gz \
  --warning=no-file-ignored host

# Swap runtime helpers and binary.
rm -rf /opt/mllm/runtime.prev
cp -a /opt/mllm/releases/$V/runtime /opt/mllm/runtime.new
chown -R mllm:mllm /opt/mllm/runtime.new
mv /opt/mllm/runtime /opt/mllm/runtime.prev
mv /opt/mllm/runtime.new /opt/mllm/runtime
ln -sfn releases/$V /opt/mllm/current
install -m 0644 /opt/mllm/current/packaging/systemd/system/mllm-*.service /etc/systemd/system/
systemctl daemon-reload

systemctl start mllm-host                      # re-attaches running engines
```

Do the same for the server and standalone roles (the server has no runtime
directory to swap). Upgrade the server and its hosts to the same release; no
compatibility between different releases of server and host is asserted.

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
mv /opt/mllm/runtime /opt/mllm/runtime.bad
mv /opt/mllm/runtime.prev /opt/mllm/runtime
ln -sfn releases/<previous> /opt/mllm/current
install -m 0644 /opt/mllm/current/packaging/systemd/system/mllm-*.service /etc/systemd/system/
systemctl daemon-reload
systemctl start mllm-host
```

Rolling back the binary alone is safe only if the newer release did not
migrate the state store (see below).

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
