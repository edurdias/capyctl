# Engine registration — design

Date: 2026-09-25. Status: approved in brainstorming with the owner, section by
section, then revised the same day with the owner's decisions on the implementation
plan (PR #22). Those decisions: registered engines live in a separate `engines.yaml`
and the role document is never rewritten; a deploy naming an unpublished profile
fails fast; detection also scans the home directory's top level; removal while the
role is unreachable is refused. Build starts after the v0.1.0-rc.4 soak. It will be
recorded as ADR 0018, amending SPEC §4.2 and §15.

Implementation status: implemented on `feat/engine-registration`;
CPU/Fake-tested; live rows ENG1–ENG4 pending.

## Problem

mllm uses engines the user has already installed, but registering one is manual:

- Standalone takes its engine only from `MLLM_VLLM_BIN` or `MLLM_SGLANG_BIN`, and
  refuses both at once. A single machine cannot run vLLM and SGLang together, or a
  stock build next to a custom one.
- On a host, runtime profiles are hand-written into `host.yaml` (executable path,
  build fingerprint, security settings), and take effect only after a host restart.
- A deployment naming a profile that no host has is accepted, then never placed.
- Users often do not remember where their engine environment lives.

## Scope

In scope:

- `mllm engine detect`, `add`, `list` and `remove`, identical on a standalone
  machine and on a host, and `mllm list engines` on the server.
- Live reload: a registered engine becomes usable without restarting the role.
- Custom builds (forks, patched or source builds, nightlies) registered the same
  way and marked `custom`.

Out of scope (owner decisions, 2026-09-25):

- Installing engines. There is no `mllm engine install`; users install vLLM or
  SGLang themselves.
- Containers. Pulling an official engine image is the likely later route for
  obtaining an engine, as a separate milestone.
- A wildcard such as `engine: vllm` in deployments. Deployments keep naming an exact
  `runtime_profile`.
- Registering an engine on a host from the server. Registration happens on the
  machine that runs the engine (SPEC §4.2: host administrators register trusted
  runtime profiles).

## Design rules

1. Standalone does not differ from server mode. Every command and behaviour below
   applies to both; standalone runs the same steps in one process.
2. mllm never executes something it only discovered. Detection reads metadata;
   execution (version check, probe) happens only after the operator names or picks
   the installation.
3. The server stays the authority over placement. A profile is removed only after
   the server confirms no deployment references it.
4. Accounting and profiles are never released on a guess: a removal that affects
   deployments waits for their stop evidence.

## Commands

```
mllm engine detect [--path DIR]...
mllm engine add [PATH] [--name NAME] [--deep-park enabled|disabled]
                [--drift warn|refuse] [--arg ARG]...
mllm engine list
mllm engine remove NAME [--drain]
mllm list engines --config server.yaml
```

### `engine detect`

Scans a bounded set of locations and prints candidates (engine, version, path).
It reads only package metadata (`vllm-*.dist-info` / `sglang-*.dist-info` under a
`site-packages` directory) and never runs anything.

Locations: each directory on `PATH` (resolving to its environment), conda
environments listed in `~/.conda/environments.txt` and under common conda roots,
`~/venvs/*`, `~/.venv`, `~/.virtualenvs/*`, uv tool environments, pipx venvs,
`/opt/*`, every directory directly in the home directory (one level deep) that holds
a `pyvenv.cfg` (for example `~/mllm-vllm-venv2`), and any `--path` given. The scan is bounded in depth and file count and
does not follow a symlink that resolves outside the root being scanned.

### `engine add [PATH]`

1. **Resolve.** `PATH` may be a venv directory, its `bin/vllm`, or its
   `bin/python3`. mllm resolves the environment and the entry point the engine is
   launched with (vLLM: the `vllm` executable; SGLang: the environment's Python).
2. **Detect.** The engine type and version come from the `dist-info`.
3. **Register (explicit, so execution is now allowed).** mllm runs the bounded
   version check, measures the installation fingerprint (ADR 0008), and runs the
   deep-park capability probe (isolated interpreter, time and output limits).
4. **Name.** The default profile name is the engine type (`vllm`, `sglang`), so
   hosts line up without effort. `--name` sets another (for example
   `vllm-patched`). A name already registered, or declared in the role's own
   document, is refused `profile_exists`; replacing a profile is a remove followed
   by an add and follows the removal rules.
5. **Mark.** A version not in the verified set is marked `custom`. Custom builds are
   never modified by mllm.
6. **Write** the profile into `engines.yaml` (see The engines file) atomically:
   write a temporary file, then rename; its revision, recorded in the first-line
   comment `# mllm-document-revision: N`, increases by one. The role's own document
   (`host.yaml`, `standalone.yaml`) is never rewritten.
7. **Reload** (see Live reload).

Options map to existing profile settings: `--deep-park` to
`security.deep_park` (default `enabled`, ADR 0012), `--drift` to
`security.installation_drift` (default `warn`), `--arg` to the profile's host-fixed
`args`, subject to the existing reserved-argument and `accept_extra_args` rules.

A deep-park probe that reports missing internals is not an error: the profile is
added with `security.deep_park: disabled` (unless the operator passed `--deep-park
enabled`), the report says `capability_missing`, and deployments on it use
`restart_only`, as today.

With no `PATH` on an interactive terminal, `add` shows the `detect` candidates and
asks the operator to pick one; the pick is the explicit registration. Without a
terminal, `add` with no `PATH` is refused `not_interactive`.

### `engine list`

For this machine: profile name, engine, version, `custom` flag, fingerprint,
deep-park probe result, whether the server has accepted it (`published` /
`not published`), and the deployments that use it.

### `engine remove NAME [--drain]`

Refused `profile_in_use`, listing the deployments, while any deployment on this
machine uses the profile. With `--drain`, those deployments are stopped on this
machine through the normal stop path (drain up to `switching.drain_timeout`, then
terminate) before the profile is removed. See Removal for the protocol. Only
profiles registered in `engines.yaml` are removed this way; a profile declared in the
role document is the operator's to edit.

### `list engines` (server)

Every host's profiles with the same columns as `engine list`, from the server's
approved snapshots and the hosts' reported inventories.

### Environment variables

`MLLM_VLLM_BIN` and `MLLM_SGLANG_BIN` remain a shortcut for standalone:

- One variable set: a profile named `local`, exactly as today. Existing deployments
  that name `runtime_profile: local` keep working.
- Both set (refused today, so no existing setup depends on it): profiles
  `local-vllm` and `local-sglang`.
- Profiles registered with `engine add` coexist with these. A name collision
  between an environment-variable profile and an added one is refused at start with
  `profile_exists`.

### The engines file

Registered profiles live in `engines.yaml`, an mllm-owned file (`kind: engines`,
`schema_version: 1`, `runtime_profiles`, mode 0600), never in the role's own
document. It sits beside the role's configuration file, with the same rule for a host
and for standalone: `--config dir/x.yaml` (or `$MLLM_CONFIG`) means
`dir/engines.yaml`; without one it is `~/.config/mllm/engines.yaml`. The generated
standalone document stays in the state directory. The role merges `engines.yaml` with
its own document at load; a profile name declared in both is refused.

### Deploy fails fast

A deploy naming a `runtime_profile` that no allowed host publishes is refused at once
and nothing is stored. The error names the profile, each allowed host with the
profiles it publishes, and the fix: `mllm engine add <path> --name <profile>` on a
host, then deploy again. Deployments are never re-resolved after `engine add`.

## Live reload

**Local control channel.** The agent (host, or the standalone role) listens on
`<state_dir>/control.sock`, a Unix socket with mode `0600` whose connections are
accepted only from the user id that runs mllm and from root, checked with
`SO_PEERCRED`. A root peer is trusted only as the owner of the socket's
private (0700) directory — the role's service user under the system units, so
`sudo mllm engine …` reaches the role it is meant to. It carries only engine
add, remove and list requests. It offers no other control path and never
reaches an engine.

As implemented, the role never writes `engines.yaml` itself (controller
ruling C1): `add` writes the file and then asks the role to reload, in that
order, below; `remove` asks the role to retire and confirm first and only
then writes the file, in a different order — see Removal.

**Add flow.**

1. The CLI validates, measures and probes, writes `engines.yaml`, then asks the agent
   to reload over the socket.
2. The agent re-reads and validates its document merged with `engines.yaml`, measures
   the profiles again, and sends a re-publish of its preparation over the existing
   mTLS session. Only `runtime_profiles` may change live; any other change to the
   role document needs a restart.
3. The server validates the re-published document exactly like a startup publish.
   Accepted: the approved snapshot is replaced atomically and the scheduler can
   place on the new profile. Rejected: the previous approved snapshot stays (the
   existing rule that a rejected import never replaces an approved one); the CLI
   prints the server's reason as `publish_rejected`, and `engine list` shows the
   profile as `not published` until it is fixed or removed.

If the agent is not running, the CLI writes `engines.yaml` and reports
`agent_unreachable`: the profile takes effect when the role starts.

**Version skew (ADR 0017).** Re-publishing is a new capability,
`live_profile_update`, advertised in Connect (and by the server in SessionReady). If
either side lacks it, `engine add` writes `engines.yaml` and prints that a restart of
the host is needed to publish. An agent that predates this design never reads
`engines.yaml`; its registered profiles are published once the host runs a release
with it. Nothing else changes for old peers.

## Removal

The running role never writes `engines.yaml`; only the CLI does (controller
ruling C1). Two phases, so no placement can slip in between the check and the
removal:

1. The CLI asks the role to retire the profile; the role asks the server
   (`RetireProfile` on its session, or the embedded host in standalone). In
   one transaction the server writes a durable retirement for (host,
   profile), which placement excludes from then on, and names every
   deployment instance on that host holding a runtime of that profile.
   - None: the retirement confirms at once.
   - Some, without `--drain`: the retirement is deleted in the same
     transaction and the request is refused `profile_in_use` with the list;
     placement resumes.
   - Some, with `--drain`: each is stopped through the normal stop path
     (drain up to `switching.drain_timeout`, then terminate, gone evidence
     required); the retirement confirms only when every stop succeeded and a
     fresh enumeration is empty. An unsettled or failed stop keeps its
     accounting; at the drain window (900 s) the retirement ends unconfirmed
     and the CLI reports `profile_in_use` naming what is unsettled.
2. The role answers `remove` once the retirement is confirmed, and writes
   nothing itself.
3. The CLI rewrites `engines.yaml` without the profile and sends `add`; the
   role re-publishes, and that publication transaction deletes the
   retirement. Any accepted publication — startup or live — clears the
   confirmed retirement of every profile it no longer lists, so a host
   restart cannot strand a confirmed row.

A retirement keeps the key it was first written under until it is cleared, so
a retried `remove` — after a lost answer, a dropped session, a crash between
the confirmation and the CLI's write, or a failed reload — resumes the same
retirement and finishes the removal instead of conflicting with it; a
retirement still draining is resumed as draining. A profile the role never
published is removed from `engines.yaml` without a retirement (no server
round trip). A published profile is never removed while the role is
unreachable: `engine remove` writes nothing and reports `agent_unreachable`.

A `remove` the role took but did not answer — the connection closed, or the
CLI's wait bound (the role's 960 s plus a margin) passed — is reported as an
**unknown outcome**, not "nothing was removed"; `mllm engine list` settles
what happened.

**Standalone** runs the same two phases in one process, against its embedded
host. The embedded host is not enrolled, so its published profiles are
recorded as an embedded publication (store schema v36) that placement reads
the way it reads a server's approved document: a profile it no longer
publishes takes no new instance, explicit or on demand. A reload that would
drop a published profile without a confirmed retirement is refused
(`publish_rejected`). Standalone expires abandoned retirements at start and
every 30 s, and its startup publication clears confirmed retirements of
profiles it no longer lists, so a role stopped mid-drain never wedges a name.

**Under the system units**, `/etc/mllm` is read-only to the role
(`ProtectSystem=strict`); the operator runs `sudo mllm engine … --config
/etc/mllm/host.yaml`. The CLI, running as root, keeps an existing
`engines.yaml`'s owner and mode on a rewrite, and creates a new file (and its
lock) for the owner of the role's state directory, mode 0600, so the role can
still read it under a read-only `/etc`.

**Version skew.** An older CLI's `remove` can still reach a newer role and
get back a confirmed retirement, but the older CLI does not know to write
`engines.yaml` afterward on this path; the profile stays excluded from
placement (the retirement is confirmed) but lingers in the file until a
current CLI runs `remove` again.

## Errors

Closed codes, each naming the remedy:

| Code | Meaning |
|---|---|
| `engine_not_found` | the path holds no `vllm` or `sglang` package |
| `engine_unsupported` | the package is not a supported engine |
| `engine_version_failed` | the version check failed or timed out; nothing is written |
| `profile_exists` | the name is taken; use `--name` or remove the existing profile |
| `profile_in_use` | removal or replacement would affect the listed deployments; use `--drain` |
| `publish_rejected` | the server refused the re-published document; its reason follows |
| `agent_unreachable` | add: `engines.yaml` is written and takes effect when the role starts; remove: nothing is written |
| `not_interactive` | `add` without a path needs a terminal |
| `profile_not_published` | a deploy names a profile no allowed host publishes; register it with `mllm engine add … --name <profile>`, then deploy again |

CLI exit codes: 16 `engine_not_found` through 23 `not_interactive` in table order,
24 `profile_not_published` (subject to the owner's answer); 9 is not reused.

## Security

- `detect` executes nothing and follows no symlink outside its roots.
- `add` executes the installation only after the operator names or picks it, and
  only through the existing bounded version check and capability probe.
- The control socket is owner-only with a peer user-id check (`SO_PEERCRED`);
  it also admits root, but only as the owner of the socket's own private
  (0700) directory — the role's service user under the system units — so
  `sudo mllm engine … --config /etc/mllm/host.yaml` reaches the intended role
  and nothing else.
- The running role never writes `engines.yaml` (review decision C1); the
  CLI is its only writer. `engines.yaml` and its lock are opened `O_NOFOLLOW`
  and, before any `fchown`, refused unless they are a regular file with
  exactly one link, owned by root or by the state-dir owner the write is for.
  Without that check, a hard link planted at either path before the CLI runs
  as root would let its `fchown` (meant to hand the file to the role's
  service user) change the ownership of whatever the link really points at,
  since a hard link shares one inode with its target (ADR 0018; fixed
  2026-09-25 with a regression test in `crates/mllm-config/tests/
  registration.rs`).
- Reserved engine arguments stay reserved; `--arg` goes through the existing
  `accept_extra_args` rules.
- The deep-park protections of ADR 0012 are unchanged: loopback-only engine
  listener, a per-launch engine key, the key-guard middleware, and no engine control
  path through host ingress or the router.

## Testing

CPU tests, tagged with their acceptance IDs:

- `detect` over a fake directory tree: correct candidates, nothing executed, a
  symlink escape not followed, the scan bounds respected.
- `add`, `list` and `remove` against a fake engine; atomic `engines.yaml` write and
  revision increase; the role document left byte-identical; a name in both files
  refused; `profile_exists`; the custom marking.
- `detect` finding a home-level venv with `pyvenv.cfg`, and not one without it.
- A deploy naming an unpublished profile refused with nothing stored, on a server
  and on the embedded standalone host.
- The control socket refusing a connection from another user id.
- Re-publish accepted, and re-publish rejected with the previous snapshot kept.
- Two-phase removal racing a placement; `profile_in_use`; `--drain` waiting for
  stop evidence, and holding on an uncertain stop.
- The `live_profile_update` fallback against a peer without the capability.
- Environment-variable compatibility: one variable gives `local`; both give
  `local-vllm` and `local-sglang`.

Live on host-a and host-b, using only the existing engine environments (no
new venvs):

- `engine add` of the existing vLLM and SGLang environments on a host under systemd:
  published within seconds, then a deployment on the new profile serves.
- Standalone with both engines on one machine, switching between them.
- Removal refused while in use, then `--drain`.
- Version skew: the new CLI beside an rc.3 agent reports `agent_unreachable` (the
  rc.3 agent never reads `engines.yaml`; the profile is published once the host runs
  the new release), and a new agent against an rc.3 server falls back to "restart to
  publish".

CPU and Fake-engine tests are not qualification; the live rows are.
