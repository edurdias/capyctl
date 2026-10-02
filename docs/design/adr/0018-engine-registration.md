# ADR 0018 — Engine registration: detect, add, list and remove, with live reload

**Status:** Accepted (owner decision 2026-09-25).
**Amends:** `SPEC.md` §4.2 (how host administrators register trusted runtime profiles) and
§15.1 (a new mllm-owned engines file beside the role document; the role document itself is
never rewritten).
**Related:** ADR 0008 (engine installations, fingerprints, capability probes), ADR 0012
(deep parking default-on), ADR 0017 (capability gating). Design:
`docs/specs/2026-09-25-engine-registration-design.md`.

**Amended by:** ADR 0023 (2026-10-01): detection also reads `tensorfold-*.dist-info`,
the entry point `<env>/bin/tensorfold`, and the verified set gains TensorFold 0.6.0.

## Context

mllm runs engines the user already installed, but registering one was manual. Standalone
took its engine only from `MLLM_VLLM_BIN` or `MLLM_SGLANG_BIN` and refused both at once. A
host's runtime profiles were hand-written into `host.yaml` (executable, build fingerprint,
security settings) and took effect only after a host restart. Users often do not remember
where their engine environment lives.

## Decision

### 1. Commands

`mllm engine detect [--path DIR]...`, `mllm engine add [PATH] [--name NAME] [--deep-park
enabled|disabled] [--drift warn|refuse] [--arg ARG]...`, `mllm engine list`, `mllm engine
remove NAME [--drain]`, and on the server `mllm list engines --config server.yaml`. They
behave the same on a host and in standalone.

- `detect` reads package metadata only (`vllm-*.dist-info`, `sglang-*.dist-info` under a
  `site-packages` directory). It executes nothing, follows no symlink out of the root it
  scans, and is bounded in roots, entries and depth. Locations: `PATH` entries (their
  environment), conda environments (`~/.conda/environments.txt` and the roots `~/miniconda3`,
  `~/anaconda3`, `~/miniforge3`, `~/mambaforge`, `~/.conda`, `/opt/conda`, `/opt/miniconda3`,
  `/opt/anaconda3`), `~/venvs/*`, `~/.venv`, `~/.virtualenvs/*`, uv tool environments,
  pipx venvs, `/opt/*`, every directory directly in the home directory that holds a
  `pyvenv.cfg` (one level deep), and every `--path`.
- `add` resolves the environment lexically (a venv's `python3` symlink is never followed),
  reads the engine and version from its `dist-info`, and only then, because the operator
  named or picked it, runs the bounded version check (60 s, 4 KiB, cleared environment,
  own process group), measures the ADR 0008 fingerprint and runs the deep-park probe. The
  profile name defaults to the engine (`vllm`, `sglang`); a name already registered, or
  declared in the role document, is refused `profile_exists`. The entry point is `<env>/bin/vllm` for vLLM and `<env>/bin/python3` for
  SGLang; `build_fingerprint` is the checked version.
- A version outside the verified set (vLLM 0.29.0, SGLang 0.5.20) is shown as `custom`.
  The mark is derived when listing; nothing is written for it.
- A probe that reports `deep_park` missing writes `security.deep_park: disabled` unless the
  operator asked for `enabled`, so deployments on it resolve `restart_only`.

### 2. The engines file

mllm never rewrites the role's own document (`host.yaml`, `standalone.yaml`). Registered
profiles live in `engines.yaml`, an mllm-owned file (`kind: engines`, `schema_version: 1`,
`runtime_profiles`), mode 0600, beside the role's configuration file: `--config dir/x.yaml`
(or `$MLLM_CONFIG`) means `dir/engines.yaml`; without one it is
`~/.config/mllm/engines.yaml` (`$XDG_CONFIG_HOME/mllm/engines.yaml`), for a host and for
standalone alike. The role merges it with its own document at load; a profile name declared
in both is refused. A write holds `engines.yaml.lock`, writes a temporary file in the same
directory, syncs it, renames it over the file and syncs the directory. The first line
records the revision, `# mllm-document-revision: N`, increased by one per write; the parser
ignores comments. A host whose merged document would exceed the 32 KiB publication bound is
refused before anything is written. `engine remove` removes only registered profiles; one
declared in the role document stays the operator's to edit.

The `mllm engine` CLI is the only writer of `engines.yaml`; a running role only reads it
(review decision C1, 2026-09-25). A rewrite keeps the file's owner and mode. When the CLI
runs as root (`sudo mllm engine …` under the system units) and creates the file, it creates it
and its lock for the owner of the role's state directory (the service user), mode 0600, so the
role can read it and a read-only `/etc` (`ProtectSystem=strict`) never blocks the role.

### 3. Live reload

The role listens on `<state_dir>/control.sock`, a Unix socket with mode 0600 whose
connections are accepted only from the user id running mllm and from root (`SO_PEERCRED`). A
client speaks only to a socket served by its own user id; root speaks to the owner of the
socket's private (0700) directory, the role's service user. It carries one
JSON request and one JSON response per connection, `add` (reload the engines file), `remove` and
`list`, and nothing else. It never reaches an engine.

On `add` the host agent re-reads its document merged with `engines.yaml`, refuses it if
anything outside `runtime_profiles` changed ("restart the role"), re-measures every profile and sends
`PublishProfiles` on its live session. While that is outstanding the host authorizes launch
plans against either the accepted or the pending document, and sends no inventory refresh.
The server validates the document as it validates a startup publication, accepts it only if
`runtime_profiles` alone changed and every removed profile's retirement is confirmed, and
then replaces the approved snapshot in one transaction. A rejection keeps the previous
snapshot and the session; the CLI prints the reason as `publish_rejected`, and `engine list`
shows the profile `not published`. If the role is not running `engines.yaml` stays written
and the CLI reports `agent_unreachable`: the profile is published when the role starts.

Re-publishing is capability `live_profile_update` (ADR 0017), declared by the host in
`Connect.capabilities` and by the server in `SessionReady.capabilities` (field 5). The server
sends `ProfilesPublished` and `ProfileRetirement` only to a host that declared it. If either
side lacks it, `engine add` writes `engines.yaml` and says a restart of the role is needed.
An agent that predates this ADR never reads `engines.yaml`; its profiles are published once
the host runs a release with it.

### 4. Removal

Two phases, so no placement slips in between the check and the removal. The host asks the
server to retire the profile (`RetireProfile`). In one transaction the server writes a
durable retirement for (host, profile), which placement excludes from then on, and names the
deployment instances on that host holding a runtime of that profile. None: confirmed. Some,
without `--drain`: the retirement is deleted in the same transaction and the request is
refused `profile_in_use` with the list. Some, with `--drain`: each is stopped through the
ordinary stop path (drain up to `switching.drain_timeout`, then terminate, gone evidence
required), and the retirement is confirmed only when every stop succeeded and a fresh
enumeration is empty. An unsettled or failed stop keeps its accounting; at the drain window
(900 s) the retirement ends and the CLI reports `profile_in_use` with what is unsettled.
The role answers `remove` once the retirement is confirmed and writes nothing. The CLI then
rewrites `engines.yaml` without the profile and sends `add`; the role re-publishes, and the
publication transaction deletes the retirement. Any accepted publication, at startup or live,
clears the confirmed retirement of every profile it does not list. A retirement keeps the key
it was first written under until it is cleared, so a retried `remove` (after a lost answer, a
dropped session, a crash between the confirmation and the CLI's write, or a failed reload)
resumes it and finishes the removal; a retirement still draining is resumed as draining. A
profile the role never published is removed from `engines.yaml` without a retirement. A
published profile is never removed while the role is unreachable (`agent_unreachable`,
nothing written). A `remove` the role took but did not answer (the connection closed or the
CLI's bound, the role's 960 s plus a margin, passed) is reported as an unknown outcome, with
`mllm engine list` to settle it.

### 5. Standalone

Standalone runs the same steps in one process: the same socket, the same CLI-written
`engines.yaml`, the same retirement over the embedded host. The embedded host is not enrolled,
so its published profiles are recorded as an embedded publication (store v36) that placement
reads as it reads a server's approved document: a profile it no longer publishes takes no new
instance, explicit or on demand. A reload that would drop a published profile without a
confirmed retirement is refused (`publish_rejected`). Standalone expires abandoned
retirements at start and every 30 s, and its startup publication clears confirmed
retirements of profiles it no longer lists, so a role stopped mid-drain never wedges a name. `MLLM_VLLM_BIN` or `MLLM_SGLANG_BIN` alone still
gives profile `local`; both give `local-vllm` and `local-sglang`. They coexist with added
profiles; a name collision is refused at start with `profile_exists`. Environment profiles are
not removable with `engine remove`. (A2: the last profile may be removed.)

### 6. Errors

Closed codes and CLI exit codes: `engine_not_found` 16, `engine_unsupported` 17,
`engine_version_failed` 18, `profile_exists` 19, `profile_in_use` 20, `publish_rejected` 21,
`agent_unreachable` 22, `not_interactive` 23, `profile_not_published` 24. Exit code 9 stays
unused.

### 7. Deploy fails fast

A deploy naming a `runtime_profile` that no allowed host publishes is refused at once
(`profile_not_published`, HTTP 409) and nothing is stored. The refusal names the profile,
each allowed host with the profiles it publishes, and the fix: `mllm engine add <path>
--name <profile>` on a host, then deploy again. Deployments are never re-resolved after
`engine add`; an allowed host that lacks the profile while another has it is recorded
refused (`profile_not_published`) for that deployment.

## Consequences

- A host's approved document can now change during a session. Every consumer already reads
  it per use (`Store::host_publication`); placement additionally requires the deployment's
  profile to be present in it and not retiring.
- Existing deployments are not re-resolved; a deploy whose profile no allowed host publishes
  is refused up front instead of being stored unplaceable.
- A host and a standalone role that both use implicit documents on one machine share
  `~/.config/mllm/engines.yaml`; the CLI asks for `--config` when both implicit documents
  exist.
- Older hosts list `live_profile_update` among `capabilities_missing` in `mllm list hosts`.
- `mllm list engines` shows what the server accepted; a profile only in a host's
  `engines.yaml` is visible in that host's `mllm engine list`.
- The role never needs write access to its configuration directory: under the system units
  `/etc/mllm` stays read-only to it, and the operator runs `sudo mllm engine … --config
  /etc/mllm/host.yaml` (review decision C1). The CLI runs the version check and the
  deep-park probe of a named installation as the user that invoked it, root in that case.
- A crash between a confirmed retirement and the CLI's write leaves the profile in
  `engines.yaml` while the server keeps it out of placement; running `engine remove` again
  finishes it.

## Verification

CPU and transport tests (`crates/mllm-config/tests/registration.rs`,
`crates/mllm-agent/tests/engines.rs`, `control_socket.rs`,
`crates/mllm-store/tests/instances_placement.rs`, `host_publication.rs`,
`crates/mllm-controller/tests/live_profiles.rs`, `crates/mllm-management/tests/engines.rs`,
`configuration.rs`, `crates/mllm-cli/tests/engine_cli.rs`, `standalone_engines.rs`). They are not qualification. Live rows ENG1–ENG4
(`scripts/live/matrix/rows/`) on host-a and host-b with the existing engine
environments are.

## Amendment A1 (owner rule 2026-09-25: every setting three ways)

The role's own installation (§5) is no longer standalone-only and no longer
environment-only. Both roles that run engines resolve it, by one rule in
shared code (`mllm_config::engine_settings`): CLI flag > environment > YAML >
default.

- YAML: `local_engine` (`vllm`, `sglang`, `build_fingerprint`, `args`,
  `kv_cache`, `deep_park`, `trust_remote_code`, `installation_drift`) in a host
  document, or under `host:` in a standalone document; `runtime_dir` and
  `resource_policy.endpoint_port_range` beside it.
- Flags on `start host` and `start standalone`: `--vllm-bin`, `--sglang-bin`,
  `--engine-fingerprint`, `--engine-args`, `--deep-park`,
  `--trust-remote-code`, `--installation-drift`, `--runtime-dir`,
  `--engine-ports`; `--kv-cache` on standalone only (a host generates no
  deployment).
- Variables: as before, with `MLLM_ENGINE_PORTS` for both roles and
  `MLLM_STANDALONE_ENGINE_PORTS` kept as a deprecated alias (warned once).

On a host the installation is published as the profile `local` (both engines:
`local-vllm` and `local-sglang`), built as `engine add` builds a profile, and
the `local_engine` block itself is not published. The names in
`ENVIRONMENT_PROFILES` are therefore reserved on a host as in standalone:
`engine add` refuses them, and a name declared in `runtime_profiles` or
`engines.yaml` and also by the local installation is refused at start. A live
reload carries the start-time settings over (§3): they change on restart only.

## Amendment A2: a role with no engine (owner decision 2026-10-01)

A host or standalone role starts with no profile and publishes an empty profile list. It
keeps its existing deployments and places none until a profile is published. Its start
banner and `capyctl status deployment <name>` say to run `capyctl engine add <path>`.

`engine remove` may remove the last registered profile. It retires the profile through §4:
deployments using it drain and stop first. The `agent_unreachable` rule of §4 stands, so
the operator starts the role, removes the profile and stops it again. A new deploy that
names a profile no host publishes still fails fast (§7).
