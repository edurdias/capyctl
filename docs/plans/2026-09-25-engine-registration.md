# Engine Registration Implementation Plan

**Goal:** Let an operator register the vLLM and SGLang installations already on a machine with `mllm engine detect|add|list|remove` (identical on a host and in standalone), see every host's engines with `mllm list engines`, and have an added or removed engine take effect without restarting the role.

**Architecture:** Detection and resolution read package metadata only (`mllm-agent::engines`). `engine add` runs the bounded version check, the ADR 0008 fingerprint and the deep-park probe, writes the profile into `engines.yaml`, an mllm-owned file beside the role's configuration file (revision header plus lock; the role's own document is never rewritten), then asks the running role over an owner-only Unix socket (`<state_dir>/control.sock`) to reload. A host agent re-measures and re-publishes its preparation on the live mTLS session (new capability `live_profile_update`, ADR 0017 gating); the server validates it like a startup publish and swaps the approved snapshot, or keeps the previous one. Removal is two-phase: the server writes a durable profile retirement (placement excludes the profile on that host in the same transaction), checks references, optionally stops them through the ordinary stop path and confirms only on stop evidence; the agent then rewrites `engines.yaml` and re-publishes. Standalone runs the same steps in one process against its embedded host document. A deploy naming a profile that no allowed host publishes is refused at once.

**Tech Stack:** Rust 2021 workspace (tokio, tonic/prost, axum, rusqlite, clap, serde_json, saphyr-parser strict YAML), bash live harness under `scripts/live/matrix/`.

**Spec:** `docs/specs/2026-09-25-engine-registration-design.md` (owner-approved, merged in d067252; revised in this PR with the owner's 2026-09-25 decisions). Read it with this plan. Governing documents: `docs/SPEC.md` (§4.2, §13, §14, §15), ADR 0008, ADR 0012, ADR 0017, and `AGENTS.md`.

## Decisions (owner-decided 2026-09-25)

The owner reviewed the first version of this plan on PR #22 and decided the items below on 2026-09-25. Items 1, 2, 4, 8 and 14 changed the design; the spec (`docs/specs/2026-09-25-engine-registration-design.md`) is updated in the same PR. One item still needs the owner: **20**, marked **owner check**.

1. **`host.yaml` is never rewritten.** Registered engines live in a separate, mllm-owned file, `engines.yaml`, merged with the role's own document at load. A profile name declared in both is refused (at role start, and by `engine add` as `profile_exists`). There is no rewrite of the role document, no `.before-engine-registration` backup and no SPEC §15.1 rewrite exception; ADR 0018 instead adds the engines file to the §15.1 authority table. `engines.yaml` is `schema_version: 1`, `kind: engines`, `runtime_profiles: {…}`, mode 0600, written as JSON-shaped YAML.
2. **Where `engines.yaml` lives.** It sits beside the role's configuration file, with the same rule for both roles: `--config dir/x.yaml` (or `$MLLM_CONFIG`) means `dir/engines.yaml`; without one it is `~/.config/mllm/engines.yaml` (`$XDG_CONFIG_HOME/mllm/engines.yaml`) for a host and for standalone alike. The generated standalone document stays in the state directory. Consequence: a host and a standalone role that both run with implicit documents on one machine would share one engines file; the CLI refuses that ambiguity and asks for `--config`.
3. **The revision is a first-line comment** of `engines.yaml`, `# mllm-document-revision: N`, increased by one per write.
4. **Remove while the role is unreachable is refused.** `engine remove` of a published profile writes nothing and answers `agent_unreachable` when the role is not running or has no control session. A profile the running role never published is removed locally.
5. **Row ENG4 adapted.** (a) The new CLI beside a running rc.3 agent gives `agent_unreachable` and writes `engines.yaml`; an rc.3 agent never reads `engines.yaml`, so the profile stays unpublished until the host runs the new binary, which publishes it at start. (b) A new agent against an rc.3 server gives `restart_required`; the host's next start publishes.
6. **A live re-publish changes `runtime_profiles` only**, and only removes a profile whose retirement the server confirmed. Anything else is `publish_rejected` ("only runtime profiles change live; restart the role").
7. **Retirement bound 900 s** (the `mllm drain host` window). Unsettled or failed stops at the bound end the retirement unconfirmed; accounting is kept; the CLI reports `profile_in_use` naming what is unsettled.
8. **Deploy fails fast; no re-resolution.** A deploy naming a `runtime_profile` that no allowed host publishes is refused at once and nothing is stored. The error names the profile, each allowed host with the profiles it publishes, and the fix: `mllm engine add <path> --name <profile>`, then deploy again. Deployments are never re-resolved after `engine add` (Task 17).
9. `custom` is derived, not stored: a version outside the verified set, vLLM `0.29.0` and SGLang `0.5.20` (`mllm_config::registration::VERIFIED`).
10. **Version check 60 s.** vLLM runs `<env>/bin/vllm --version`; SGLang runs `<env>/bin/python3 -I -B -c "import importlib.metadata,sys;print(importlib.metadata.version(sys.argv[1]))" sglang`. Both run with a cleared environment (only `HOME` and `PATH=/usr/bin:/bin`), stdin and stderr closed, their own process group, at most 60 s and 4 KiB of output. The reported version must equal the `dist-info` version, else `engine_version_failed`. `build_fingerprint` is that version.
11. Entry points: `<env>/bin/vllm` for vLLM and `<env>/bin/python3` for SGLang, lexical; the venv's `python3` symlink is never followed.
12. **Deep park disabled when the probe reports it missing**, unless the operator passed `--deep-park enabled` (then the report says `capability_missing` and the existing launch refusal applies). A probe that cannot run is `unknown` and changes nothing.
13. `--arg` values follow the existing profile rule (`engine_policy::validate_profile_args`); `accept_extra_args` is a deployment `engine_config` switch and does not govern profile arguments.
14. **Detection also scans the home directory's top level**, one level deep: a directory with `pyvenv.cfg` and a vllm or sglang `dist-info` (metadata only), so `~/mllm-vllm-venv2` is found without `--path`. Conda roots: `~/miniconda3`, `~/anaconda3`, `~/miniforge3`, `~/mambaforge`, `~/.conda`, `/opt/conda`, `/opt/miniconda3`, `/opt/anaconda3` (each root and its `envs/*`), plus `~/.conda/environments.txt`. uv tools: `${XDG_DATA_HOME:-~/.local/share}/uv/tools/*`. pipx: `${PIPX_HOME:-~/.local/pipx}/venvs/*` and `~/.local/share/pipx/venvs/*`.
15. Control socket protocol: one JSON request line and one JSON reply line per connection, at most 64 KiB each, `"v": 1`; operations `add`, `remove` (`profile`, `drain`), `list`; nothing else.
16. Proto field numbers: `AgentToServer.publish_profiles = 11`, `AgentToServer.retire_profile = 12`, `ServerToAgent.profiles_published = 12`, `ServerToAgent.profile_retirement = 13`, `SessionReady.capabilities = 5`. `live_profile_update` is a server-to-host capability.
17. Exit codes: 16 `engine_not_found`, 17 `engine_unsupported`, 18 `engine_version_failed`, 19 `profile_exists`, 20 `profile_in_use`, 21 `publish_rejected`, 22 `agent_unreachable`, 23 `not_interactive`. 9 stays unused.
18. Role document for the `engine` commands: `--config`; else `$MLLM_CONFIG`; else `~/.config/mllm/host.yaml` if it exists; else `<state_dir>/config/standalone.yaml` if it exists; both implicit present is refused `invalid_config`. The engines file follows item 2.
19. Standalone environment profiles (`local`, `local-vllm`, `local-sglang`) are not removable with `engine remove`; a profile declared in `host.yaml` is not removable either (the operator edits it). Standalone still needs `MLLM_MODELS_ROOT`; an engine variable is no longer required when `engines.yaml` registers an engine.
20. **owner check — exit code for the fail-fast deploy.** Item 8 needs a closed code; the plan uses `profile_not_published`, HTTP 409, CLI exit **24** (next free after 23). The owner accepted 16–23; 24 is new.

## Global Constraints

- Cite the governing requirement inline where behaviour is spec-driven, e.g. `// ADR 0018: ...`, `// SPEC §4.2: ...` (AGENTS.md "Code conventions").
- Tag every new test with its acceptance-matrix ID in a comment (`// T03`, `// T34`, ...). IDs used here (SPEC §20): T01 (action-first grammar, stable codes), T03 (invalid configuration fails without side effects), T04 (atomic creation, no overwrite), T07 (no implicit installation; eligibility from preparation), T14 (profile and reserved-flag changes), T16 and T32 (accounting retained until evidence), T21 (deep-park policy), T22 (SGLang and custom builds on the same contract), T33 (restart reconciliation), T34 (session replay and version/capability gating), T37 (security boundaries: socket, symlinks, nothing discovered is executed).
- Uncertainty keeps accounting: never release a reservation, advance an epoch, confirm a retirement or remove a profile without verified stop evidence (spec design rule 4).
- mllm never executes something it only discovered: `detect` reads metadata only; execution (version check, probe) happens only after the operator names or picks the installation (spec design rule 2, ADR 0008 carve-out 2).
- The server stays the authority over placement: a published profile is removed only after the server confirms no deployment on that host uses it (spec design rule 3).
- The control socket is `<state_dir>/control.sock`, mode `0600`, peer uid checked with `SO_PEERCRED`; it carries only engine add, remove and list, and never reaches an engine.
- The deep-park protections of ADR 0012 are unchanged: loopback-only engine listener, per-launch engine key, key-guard middleware, no engine control path through host ingress or the router.
- Additive protocol only: no field renumbered, command encoding version stays `"1"`, `PROTOCOL_VERSION` stays `"2"` (ADR 0017).
- Store schema moves from v34 to v35, forward-only.
- No new venvs and no environment changes on the hosts; live rows use only `$HOME/mllm-vllm-venv2` (host-a), `$HOME/mllm-vllm-0.29-venv` (host-b) and `$HOME/mllm-sglang-0.5.20-venv` (both).
- CPU and Fake-engine tests are not qualification; the live rows ENG1–ENG4 are. Say so in every status claim.
- Verification before every commit: the core suite
  `cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`,
  `cargo test --workspace --all-targets --locked`, and
  `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- Prose in documents and commit messages is normal English. Commits end with the session's attribution lines.

## Review Focus

The inputs below are the ones the spec implies but does not test, most likely to bite first. Each has a pinning test in the task named.

1. **`host.yaml` hand-edited outside `runtime_profiles` while the role runs** (for example a changed `resource_policy`), followed by `engine add`: the reload must refuse with "only runtime profiles change live; restart the role" and publish nothing, not silently publish the edit. Test: Task 13, `a_reload_refuses_a_document_changed_outside_profiles`.
2. **A venv whose `bin/python3` is a symlink to `/usr/bin/python3`:** resolution must stay inside the venv and register `<venv>/bin/python3`, never `/usr`. Test: Task 4, `the_interpreter_symlink_is_not_followed`.
3. **A state directory whose socket path exceeds the 107-byte `sun_path` limit:** the role must still start (the socket is refused with a logged reason) and the CLI must say `agent_unreachable` with the path, not panic. Test: Task 12, `an_overlong_socket_path_is_refused_cleanly`.
4. **An `engines.yaml` whose merged host document would exceed the 32 KiB publication bound:** refused before anything is written, naming the bound. Test: Task 2, `a_document_over_the_publication_bound_is_not_written`.
5. **Two `engine add` runs at once on one machine:** the `engines.yaml` lock serializes them; the second sees the first's profile (`profile_exists` for the same name, or revision + 2 for another name), never a lost update. Test: Task 2, `concurrent_writers_never_lose_an_update`.

---

## File Structure

New files:

| File | Responsibility |
|---|---|
| `docs/design/adr/0018-engine-registration.md` | The decision record; amends SPEC §4.2 and the §15.1 authority table. |
| `crates/mllm-config/src/registration.rs` | `engines.yaml`: location rule, load, revision header, lock, atomic write, merge into the host document; profile builder and checks, verified set, name rule, profiles-only comparison. |
| `crates/mllm-config/tests/registration.rs` | Tests for the above. |
| `crates/mllm-agent/src/engines.rs` | Module root: `Engine` candidate types, dist-info reader. |
| `crates/mllm-agent/src/engines/detect.rs` | Bounded metadata-only scan. |
| `crates/mllm-agent/src/engines/resolve.rs` | Path to environment and entry point; bounded version check. |
| `crates/mllm-agent/tests/engines.rs` | Detect, resolve and version-check tests over fake trees. |
| `crates/mllm-agent/src/profiles.rs` | `ProfileSet`, `HostProfiles` (accepted, previous, pending), measurement of a document into an inventory. |
| `crates/mllm-agent/src/control_socket.rs` | Owner-only Unix socket server and client, request/response types, peer uid check. |
| `crates/mllm-agent/src/host_control.rs` | Host implementation of the control handler (reload, retire, list). |
| `crates/mllm-agent/tests/control_socket.rs` | Socket permission, uid refusal, protocol bounds, stale socket. |
| `crates/mllm-store/src/profile_retirement.rs` | Durable retirements, candidates by profile, progress, confirmation, republish transaction. |
| `crates/mllm-controller/tests/live_profiles.rs` | mTLS session tests for re-publish and retirement, and the agent's live reload and removal end to end. |
| `crates/mllm-management/src/engines.rs` | `StoreRetirements` (retirement service over the ordinary stop path); `GET /management/v1/engines` lives in `hosts.rs`. |
| `crates/mllm-management/tests/engines.rs` | Retirement service and engines endpoint tests. |
| `crates/mllm-cli/src/engine.rs` | `mllm engine` commands and `list engines` client. |
| `crates/mllm-cli/src/engine/target.rs` | Which document and socket an `engine` command acts on. |
| `crates/mllm-cli/src/standalone_engines.rs` | Standalone control handler (in-process reload and removal). |
| `crates/mllm-cli/tests/engine_cli.rs` | CLI flows against fake environments and a scripted role socket. |
| `crates/mllm-cli/tests/standalone_engines.rs` | Standalone multi-engine start, env compatibility, live add and remove. |
| `scripts/live/matrix/rows/ENG1.sh` … `ENG4.sh` | Live rows (plan only; not run by this plan's author). |

Modified files (main ones; each task lists exact lines):

| File | Change |
|---|---|
| `docs/SPEC.md` | "Amended by ADR 0018" notes in §4.2 and §15.1 (engines file row). |
| `docs/specs/2026-09-25-engine-registration-design.md` | Updated in this PR with the owner's 2026-09-25 decisions. |
| `crates/mllm-config/src/lib.rs`, `effective.rs` | Export `registration`; `check_runtime_profile`. |
| `crates/mllm-config/src/schema.rs`, `remote_roles.rs` | `ConfigKind::Engines`; `HostConfig::load` merges `engines.yaml`. |
| `crates/mllm-protocol/proto/mllm/management/v1/management.proto`, `src/capabilities.rs`, `tests/version_skew.rs` | New messages and capability. |
| `crates/mllm-store/src/schema.rs`, `migrations.rs`, `lib.rs`, `ordinary_lifecycle/placement.rs` | Schema v35; placement exclusion. |
| `crates/mllm-controller/src/host_publication.rs`, `agent_sessions.rs` | Live re-publish, retirement messages, SessionReady capabilities. |
| `crates/mllm-agent/src/session.rs`, `native_execution.rs`, `lib.rs` | Profile updates channel; swappable profiles. |
| `crates/mllm-cli/src/grammar.rs`, `main.rs`, `output.rs`, `remote_roles.rs`, `roles.rs`, `standalone_config.rs`, `lib.rs`, `client.rs` | Commands, codes, wiring, standalone multi-engine. |
| `crates/mllm-controller/src/installation_gate.rs`, `engine_provider.rs` | Embedded installations keyed by executable; provider lists installations. |
| `crates/mllm-management/src/configuration.rs`, `drain.rs`, `installation.rs`, `lib.rs` | Swappable embedded host document; `drain_rounds` shared; deploy fails fast on an unpublished profile. |
| `crates/mllm-cli/tests/errors.rs` | Exit-code table. |
| `docs/operations/install.md`, `docs/runbooks/f2-current-status.md` | Operator docs, status. |
| `scripts/live/matrix/lib.sh`, `roles.sh`, `gen_host_doc.py`, `README.md` | Per-host binary override, systemd host bring-up, profile-less host document. |

Task order and dependencies: 1 (ADR) → 2, 3 (config) → 4, 5 (agent engine discovery) → 6 (protocol) → 7, 8 (store) → 9, 10 (controller) → 11 (management) → 12 (control socket) → 13 (host live reload and removal, with role wiring) → 14 (CLI engine commands, exit codes, `list engines`, install guide) → 15 (standalone: environment compatibility, several engines, live add and remove) → 16 (live harness, rows ENG1–ENG4, status runbook). 17 (deploy fails fast on an unpublished profile) depends only on Task 14's exit codes and may run any time after it, before Task 16's live rows.

---

### Task 1: ADR 0018 and the SPEC amendments

**Files:**
- Create: `docs/design/adr/0018-engine-registration.md`
- Modify: `docs/SPEC.md` §4.2 (after the paragraph ending "destructive park/restore verification is an explicit operation.", line 156) and §15.1 (the authority table, lines 538-544)

**Interfaces:**
- Consumes: the spec and the owner decisions 1–20 above (item 20 once the owner answers it).
- Produces: the names every later task cites: `ADR 0018`, `engines.yaml`, capability `live_profile_update`, codes `engine_not_found` … `not_interactive` and `profile_not_published`, exit codes 16–24, `# mllm-document-revision: N`, `<state_dir>/control.sock`.

- [ ] **Step 1: Confirm item 20.** Items 1–19 were decided by the owner on 2026-09-25. Ask the owner about item 20 (exit 24 for `profile_not_published`); if the answer differs, edit Task 17 and the ADR text below before continuing.

- [ ] **Step 2: Write the ADR.** Create `docs/design/adr/0018-engine-registration.md` with exactly this content (adjusted only for the answer from Step 1):

```markdown
# ADR 0018 — Engine registration: detect, add, list and remove, with live reload

**Status:** Accepted (owner decision 2026-09-25).
**Amends:** `SPEC.md` §4.2 (how host administrators register trusted runtime profiles) and
§15.1 (a new mllm-owned engines file beside the role document; the role document itself is
never rewritten).
**Related:** ADR 0008 (engine installations, fingerprints, capability probes), ADR 0012
(deep parking default-on), ADR 0017 (capability gating). Design:
`docs/specs/2026-09-25-engine-registration-design.md`.

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

### 3. Live reload

The role listens on `<state_dir>/control.sock`, a Unix socket with mode 0600 whose
connections are accepted only from the user id running mllm (`SO_PEERCRED`). It carries one
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
After confirmation the host rewrites `engines.yaml` without the profile and re-publishes; the
publication transaction deletes the retirement. A profile the role never published is
removed locally. A published profile is never removed while the role is unreachable
(`agent_unreachable`, nothing written).

### 5. Standalone

Standalone runs the same steps in one process: the same socket, the same `engines.yaml` write,
the same retirement over the embedded host. `MLLM_VLLM_BIN` or `MLLM_SGLANG_BIN` alone still
gives profile `local`; both give `local-vllm` and `local-sglang`. They coexist with added
profiles; a name collision is refused at start with `profile_exists`. Environment profiles are
not removable with `engine remove`.

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

## Verification

CPU and transport tests (`crates/mllm-config/tests/registration.rs`,
`crates/mllm-agent/tests/engines.rs`, `control_socket.rs`,
`crates/mllm-store/tests/instances_placement.rs`, `host_publication.rs`,
`crates/mllm-controller/tests/live_profiles.rs`, `crates/mllm-management/tests/engines.rs`,
`configuration.rs`, `crates/mllm-cli/tests/engine_cli.rs`, `standalone_engines.rs`). They are not qualification. Live rows ENG1–ENG4
(`scripts/live/matrix/rows/`) on host-a and host-b with the existing engine
environments are.
```

- [ ] **Step 3: Amend SPEC §4.2.** In `docs/SPEC.md`, after the paragraph that ends "destructive park/restore verification is an explicit operation." (line 156), insert:

```markdown
> **Amended by [ADR 0018](design/adr/0018-engine-registration.md)** (owner decision 2026-09-25).

Host administrators register runtime profiles with `mllm engine detect`, `add`, `list` and `remove`, the same on a host and in standalone. Registered profiles live in `engines.yaml` beside the role's configuration file and are merged with it at load; mllm never rewrites the role document. Detection reads package metadata only and executes nothing; an installation is executed (bounded version check, installation fingerprint, deep-park probe) only after the operator names or picks it. A registered profile is published on the live control session without restarting the role (capability `live_profile_update`); the server validates it like a startup publication and keeps the previous approved snapshot when it refuses one. A published profile is removed only after the server confirms, in two phases, that no deployment on that host uses it, stopping them through the ordinary stop path when asked and never confirming without stop evidence. A deploy naming a profile no allowed host publishes is refused at once (`profile_not_published`). mllm still installs no engine.
```

- [ ] **Step 4: Amend SPEC §15.1.** Add a row to the authority table (after the "Host YAML" row, line 541) and, after the paragraph ending "Creating a missing configuration during initialization/enrollment is an explicit documented exception." (line 546), a note:

```markdown
| Engines file (`engines.yaml`) | Runtime profiles registered with `mllm engine add`, beside the role's configuration file; written only by `mllm engine add` and `remove`, merged with the role document at load. | Anything else; a profile name the role document also declares. |
```

```markdown
> **Amended by [ADR 0018](design/adr/0018-engine-registration.md)** (owner decision 2026-09-25).

The engines file is mllm-owned operational state, not administrator YAML: mllm writes it only when the operator runs `mllm engine add` or `remove`, under a lock and atomically, with its revision in the first-line comment `# mllm-document-revision: N`. The role's own document is never rewritten.
```

- [ ] **Step 5: Check the links render and nothing else changed.**

Run: `git diff --stat && grep -n "ADR 0018" docs/SPEC.md`
Expected: SPEC hunks in §4.2 and §15.1, one new ADR file; two `Amended by [ADR 0018]` lines.

- [ ] **Step 6: Commit**

```bash
git add docs/design/adr/0018-engine-registration.md docs/SPEC.md
git commit -m "docs: ADR 0018 engine registration, amending SPEC 4.2 and 15.1

Records the owner-approved design for mllm engine detect, add, list and
remove: metadata-only detection, explicit execution only after the
operator names an installation, an mllm-owned engines.yaml beside the
role document (never rewritten), live re-publication gated by the
live_profile_update capability, two-phase removal confirmed only on stop
evidence, and a deploy that fails fast on an unpublished profile."
```

---

### Task 2: `engines.yaml`: the mllm-owned engines file, its lock, atomic write, and the merge into the host document

**Files:**
- Create: `crates/mllm-config/src/registration.rs`
- Create: `crates/mllm-config/tests/registration.rs`
- Modify: `crates/mllm-config/src/lib.rs:4-17` (add `pub mod registration;`)
- Modify: `crates/mllm-config/Cargo.toml` (add `libc = "0.2"`, already a dependency of `mllm-agent` and `mllm-cli`, so `Cargo.lock` gains no new package)
- Modify: `crates/mllm-config/src/schema.rs` (`ConfigKind::Engines` in the enum `:10-15`, `as_str` `:17-26`, `from_str` `:30-43`, and a `KindSchema` arm after `ConfigKind::Standalone` `:440-450`)
- Modify: `crates/mllm-config/src/remote_roles.rs` (new `HostConfig::load` beside `parse` at `:387`)
- Modify: `crates/mllm-cli/src/remote_roles.rs:871-882, 890-891` (`start host` and `join host` load the host document with `HostConfig::load`)

**Interfaces:**
- Consumes: `mllm_config::parse_strict(ConfigKind, &str) -> Result<Value, ConfigError>` (`strict_yaml.rs:43`), `ConfigError::new(code, path, detail)` (`error.rs:54`), `ConfigErrorCode::{Io, UnsupportedCombination}`, `HostConfig::parse` (`remote_roles.rs:387`).
- Produces (used by Tasks 3, 13, 14, 15):
  - `pub const ENGINES_FILE: &str = "engines.yaml"`, `pub const REVISION_HEADER: &str = "# mllm-document-revision: "`, `pub const MAX_PUBLISHED_BYTES: usize = 32 * 1024`
  - `pub fn engines_beside(role_document: &Path) -> PathBuf` (the role document's directory + `engines.yaml`)
  - `pub fn engines_path(role_document: Option<&Path>, config_home: &Path) -> PathBuf` (`--config dir/x.yaml` → `dir/engines.yaml`; none → `<config_home>/mllm/engines.yaml`)
  - `pub fn config_home(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf>` (`$XDG_CONFIG_HOME`, else `$HOME/.config`)
  - `pub fn revision_of(text: &str) -> u64`
  - `pub struct EnginesFile { pub path: PathBuf, pub revision: u64, pub profiles: Map<String, Value> }` with `load(&Path)` (a missing file is empty at revision 0), `parse(&Path, &str)`, `render(&self, revision: u64) -> String`
  - `pub struct EnginesLock` and `pub fn lock_engines(path: &Path) -> Result<EnginesLock, ConfigError>`
  - `pub fn merge_into_host(document: &mut Value, engines: &EnginesFile) -> Result<(), ConfigError>` (a name in both is refused)
  - `pub fn write_engines(file: &EnginesFile, lock: &EnginesLock, host_document: Option<&Value>) -> Result<u64, ConfigError>` (returns the new revision; with a host document, checks names and the merged publication size)
  - `HostConfig::load(path: &Path) -> Result<HostConfig, ConfigError>` (the host document merged with `engines_beside(path)`)
  - `ConfigKind::Engines` (`kind: engines`; fields `schema_version`, `kind`, `runtime_profiles`)

- [ ] **Step 1: Write the failing tests.** Create `crates/mllm-config/tests/registration.rs`:

```rust
//! ADR 0018 §2: `engines.yaml`, the mllm-owned file engine registration
//! writes beside the role's document. The role's own document is never
//! rewritten. CPU tests only; they are not qualification.
use mllm_config::registration::*;
use mllm_config::remote_roles::HostConfig;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn host_doc(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("host.yaml");
    std::fs::write(&path, format!("# operator notes\n{}", HostConfig::template(&dir.join("state")))).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn profile() -> serde_json::Value {
    serde_json::json!({"engine":"vllm","revision":1,"executable":"/opt/v/bin/vllm",
        "build_fingerprint":"0.29.0","args":[],"env":{},
        "log_policy":{"max_file_bytes":"16MiB","retained_files":3},
        "security":{"deep_park":"enabled","trust_remote_code":false,
            "credential_ref":"secret://engine-key","admin_credential_ref":"secret://admin-key"}})
}

fn host_value(path: &Path) -> serde_json::Value {
    mllm_config::parse_strict(mllm_config::ConfigKind::Host, &std::fs::read_to_string(path).unwrap()).unwrap()
}

// ADR 0018 §2 (owner decision 2026-09-25): the engines file sits beside the
// role's configuration file; without one, under the user's config home.
#[test]
fn the_engines_file_sits_beside_the_role_document() {
    assert_eq!(engines_path(Some(Path::new("/etc/mllm/x.yaml")), Path::new("/home/u/.config")), Path::new("/etc/mllm/engines.yaml"));
    assert_eq!(engines_path(None, Path::new("/home/u/.config")), Path::new("/home/u/.config/mllm/engines.yaml"));
    assert_eq!(engines_beside(Path::new("/r/host.yaml")), Path::new("/r/engines.yaml"));
    let env = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
    assert_eq!(config_home(&env).unwrap(), Path::new("/home/u/.config"));
    let xdg = |k: &str| (k == "XDG_CONFIG_HOME").then(|| "/x".to_string());
    assert_eq!(config_home(&xdg).unwrap(), Path::new("/x"));
}

// T04 T03: the first write creates the file at revision 1 with mode 0600;
// the host document is never touched; the merged host document parses.
#[test]
fn a_write_creates_the_engines_file_and_never_touches_the_host_document() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let before = std::fs::read(&host).unwrap();
    let path = engines_beside(&host);
    let mut engines = EnginesFile::load(&path).unwrap();
    assert_eq!((engines.revision, engines.profiles.len()), (0, 0));
    engines.profiles.insert("vllm".into(), profile());
    let lock = lock_engines(&path).unwrap();
    assert_eq!(write_engines(&engines, &lock, Some(&host_value(&host))).unwrap(), 1);
    drop(lock);
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.starts_with("# mllm-document-revision: 1\n"), "{written}");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&host).unwrap(), before, "host.yaml is never rewritten");
    let config = HostConfig::load(&host).unwrap();
    assert!(config.profiles.contains_key("vllm"));
    assert_eq!(config.document["runtime_profiles"]["vllm"], profile());
    let lock = lock_engines(&path).unwrap();
    assert_eq!(write_engines(&EnginesFile::load(&path).unwrap(), &lock, None).unwrap(), 2);
}

// T03 (owner decision 2026-09-25): the same profile name in the host document
// and the engines file is refused, at load and at write.
#[test]
fn a_name_in_both_files_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let mut document = host_value(&host);
    document["runtime_profiles"]["vllm"] = profile();
    std::fs::write(&host, document.to_string()).unwrap();
    let path = engines_beside(&host);
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert("vllm".into(), profile());
    let lock = lock_engines(&path).unwrap();
    let refused = write_engines(&engines, &lock, Some(&document)).unwrap_err();
    assert!(refused.detail.contains("vllm") && refused.detail.contains("both"), "{refused:?}");
    assert!(!path.exists(), "nothing was written");
    std::fs::write(&path, format!("kind: engines\nschema_version: 1\nruntime_profiles:\n  vllm: {}\n", profile())).unwrap();
    let error = HostConfig::load(&host).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.vllm");
}

// T04: the lock serializes writers, so no update is lost.
#[test]
fn concurrent_writers_never_lose_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let path = engines_beside(&host);
    let workers: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|name| {
            let (path, host) = (path.clone(), host_value(&host));
            std::thread::spawn(move || {
                let lock = lock_engines(&path).unwrap();
                let mut engines = EnginesFile::load(&path).unwrap();
                engines.profiles.insert(name.into(), profile());
                write_engines(&engines, &lock, Some(&host)).unwrap()
            })
        })
        .collect();
    let mut revisions: Vec<u64> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    revisions.sort();
    assert_eq!(revisions, vec![1, 2, 3, 4]);
    assert_eq!(EnginesFile::load(&path).unwrap().profiles.len(), 4);
}

// T03 (Review Focus 4): an engines file whose merged host document would
// exceed the publication bound is refused before anything is written.
#[test]
fn a_document_over_the_publication_bound_is_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let path = engines_beside(&host);
    let mut engines = EnginesFile::load(&path).unwrap();
    for i in 0..80 {
        let mut p = profile();
        p["args"] = serde_json::json!(["--served-model-name", "x".repeat(300)]);
        engines.profiles.insert(format!("p{i}"), p);
    }
    let lock = lock_engines(&path).unwrap();
    let error = write_engines(&engines, &lock, Some(&host_value(&host))).unwrap_err();
    assert!(error.detail.contains("32768"), "{error:?}");
    assert!(!path.exists());
}

// T03: an engines file is strict: unknown fields and another kind are refused.
#[test]
fn the_engines_file_is_strict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engines.yaml");
    for text in ["kind: host\nschema_version: 1\nname: x\n", "kind: engines\nschema_version: 1\nruntime_profiles: {}\nextra: 1\n"] {
        assert!(EnginesFile::parse(&path, text).is_err(), "{text}");
    }
    let ok = EnginesFile::parse(&path, "# mllm-document-revision: 7\nkind: engines\nschema_version: 1\nruntime_profiles: {}\n").unwrap();
    assert_eq!(ok.revision, 7);
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-config --test registration --locked`
Expected: FAIL to compile: `unresolved import mllm_config::registration`.

- [ ] **Step 3: Add the `engines` document kind.** In `schema.rs`: add `Engines` to `enum ConfigKind`; `ConfigKind::Engines => "engines"` in `as_str`; `"engines" => Ok(ConfigKind::Engines)` in `from_str`; and in `schema()` after the `Standalone` arm:

```rust
        // ADR 0018 §2: the mllm-owned engines file beside a role document.
        ConfigKind::Engines => &KindSchema {
            required: &["schema_version", "kind"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("runtime_profiles", FieldSpec::MapOf(&PROFILE)),
            ],
        },
```

(If any other `match kind` over `ConfigKind` in the crate is exhaustive, give `Engines` the arm the compiler asks for; `mllm validate config` treats it like any other kind.)

- [ ] **Step 4: Implement.** Add `pub mod registration;` to `lib.rs` and `libc = "0.2"` to `crates/mllm-config/Cargo.toml`. Create `crates/mllm-config/src/registration.rs`:

```rust
//! ADR 0018 §2 (owner decision 2026-09-25): engine registration writes only
//! `engines.yaml`, an mllm-owned file beside the role's configuration file.
//! The host or standalone document is never rewritten; the role merges the
//! two at load, and a profile name declared in both is refused. Writes hold
//! `engines.yaml.lock`, go through a temporary file, sync and rename, and
//! record the revision on the first line (a comment the parser ignores).
use crate::{parse_strict, ConfigError, ConfigErrorCode, ConfigKind};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const ENGINES_FILE: &str = "engines.yaml";
pub const REVISION_HEADER: &str = "# mllm-document-revision: ";
/// The largest host document a publication carries
/// (`mllm_store::host_publication`, `config_json.len() > 32768` is refused).
pub const MAX_PUBLISHED_BYTES: usize = 32 * 1024;

fn io(path: &Path, error: impl std::fmt::Display) -> ConfigError {
    ConfigError::new(ConfigErrorCode::Io, path.display().to_string(), error.to_string())
}

/// ADR 0018 §2: `dir/x.yaml` → `dir/engines.yaml`.
pub fn engines_beside(role_document: &Path) -> PathBuf {
    role_document.parent().unwrap_or(Path::new(".")).join(ENGINES_FILE)
}

/// ADR 0018 §2: beside the role document named with `--config`; without one,
/// `<config home>/mllm/engines.yaml`, for a host and for standalone alike.
pub fn engines_path(role_document: Option<&Path>, config_home: &Path) -> PathBuf {
    match role_document {
        Some(document) => engines_beside(document),
        None => config_home.join("mllm").join(ENGINES_FILE),
    }
}

/// `$XDG_CONFIG_HOME`, else `$HOME/.config` (absolute paths only).
pub fn config_home(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")).filter(|p| p.is_absolute()))
}

/// ADR 0018 §2: the revision on the first line, or 0.
pub fn revision_of(text: &str) -> u64 {
    text.lines()
        .next()
        .and_then(|line| line.strip_prefix(REVISION_HEADER))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// The registered profiles of one role.
#[derive(Debug, Clone, PartialEq)]
pub struct EnginesFile {
    pub path: PathBuf,
    /// 0 when the file does not exist yet.
    pub revision: u64,
    pub profiles: Map<String, Value>,
}

impl EnginesFile {
    /// A missing file is an empty one at revision 0.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(path, &text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self { path: path.to_path_buf(), revision: 0, profiles: Map::new() })
            }
            Err(e) => Err(io(path, e)),
        }
    }

    /// SPEC §15.3: strict, like every role document.
    pub fn parse(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let value = parse_strict(ConfigKind::Engines, text)?;
        let profiles = value["runtime_profiles"].as_object().cloned().unwrap_or_default();
        Ok(Self { path: path.to_path_buf(), revision: revision_of(text), profiles })
    }

    /// JSON-shaped YAML, as `mllm init` writes, under the revision line.
    pub fn render(&self, revision: u64) -> String {
        let body = serde_json::json!({"schema_version": 1, "kind": "engines", "runtime_profiles": self.profiles});
        format!(
            "{REVISION_HEADER}{revision}\n# Written by `mllm engine add` and `remove`. The role merges it with its own document.\n{}\n",
            serde_json::to_string_pretty(&body).expect("profiles always encode")
        )
    }
}

/// ADR 0018 §2: `document` (a host document) with `engines`' profiles added.
/// A profile name the host document already declares is refused.
pub fn merge_into_host(document: &mut Value, engines: &EnginesFile) -> Result<(), ConfigError> {
    if engines.profiles.is_empty() {
        return Ok(());
    }
    let declared = document
        .as_object_mut()
        .ok_or_else(|| ConfigError::new(ConfigErrorCode::UnsupportedCombination, "", "not a mapping"))?
        .entry("runtime_profiles")
        .or_insert_with(|| Value::Object(Map::new()));
    let declared = declared
        .as_object_mut()
        .ok_or_else(|| ConfigError::new(ConfigErrorCode::UnsupportedCombination, "runtime_profiles", "must be a mapping"))?;
    for (name, profile) in &engines.profiles {
        if declared.contains_key(name) {
            return Err(ConfigError::new(
                ConfigErrorCode::UnsupportedCombination,
                format!("runtime_profiles.{name}"),
                format!("profile {name} is declared in both the role document and {}; remove one", engines.path.display()),
            ));
        }
        declared.insert(name.clone(), profile.clone());
    }
    Ok(())
}

/// An exclusive advisory lock on `<engines file>.lock` for one
/// read-modify-write. Dropping it releases the lock.
pub struct EnginesLock {
    _file: File,
    path: PathBuf,
}

pub fn lock_engines(path: &Path) -> Result<EnginesLock, ConfigError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    let lock = PathBuf::from(format!("{}.lock", path.display()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock)
        .map_err(|e| io(&lock, e))?;
    file.lock().map_err(|e| io(&lock, e))?;
    Ok(EnginesLock { _file: file, path: path.to_path_buf() })
}

/// ADR 0018 §2: write `file` under `lock` at the next revision and return it.
/// The revision is read again under the lock. With `host_document`, a name
/// the host document declares is refused and the merged document must fit a
/// publication. Nothing is written when a check fails.
pub fn write_engines(file: &EnginesFile, lock: &EnginesLock, host_document: Option<&Value>) -> Result<u64, ConfigError> {
    if lock.path != file.path {
        return Err(ConfigError::new(ConfigErrorCode::UnsupportedCombination, file.path.display().to_string(), "the lock names another file"));
    }
    if let Some(host) = host_document {
        let mut merged = host.clone();
        merge_into_host(&mut merged, file)?;
        let published = merged.to_string().len();
        if published > MAX_PUBLISHED_BYTES {
            return Err(ConfigError::new(
                ConfigErrorCode::UnsupportedCombination,
                file.path.display().to_string(),
                format!("the host document with these engines would be {published} bytes; a publication carries at most {MAX_PUBLISHED_BYTES} (32768)"),
            ));
        }
    }
    let current = EnginesFile::load(&file.path)?;
    let revision = current.revision + 1;
    let text = file.render(revision);
    // SPEC §15.3: validate before side effects.
    EnginesFile::parse(&file.path, &text)?;
    let dir = file.path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| io(dir, e))?;
    tmp.write_all(text.as_bytes()).map_err(|e| io(tmp.path(), e))?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600)).map_err(|e| io(tmp.path(), e))?;
    tmp.as_file().sync_all().map_err(|e| io(tmp.path(), e))?;
    tmp.persist(&file.path).map_err(|e| io(&file.path, e.error))?;
    File::open(dir).and_then(|d| d.sync_all()).map_err(|e| io(dir, e))?;
    Ok(revision)
}
```

In `crates/mllm-config/src/remote_roles.rs`, beside `parse`:

```rust
    /// ADR 0018 §2: the host document at `path` with the engines registered
    /// beside it (`engines.yaml`) merged in. The file itself is never
    /// rewritten; a profile name declared in both is refused.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::new(ConfigErrorCode::Io, path.display().to_string(), e.to_string()))?;
        let mut document = crate::parse_strict(crate::ConfigKind::Host, &text)?;
        let engines = crate::registration::EnginesFile::load(&crate::registration::engines_beside(path))?;
        crate::registration::merge_into_host(&mut document, &engines)?;
        Self::parse(&document.to_string())
    }
```

In `crates/mllm-cli/src/remote_roles.rs`, `start host` (`:879-881`) becomes `serve_host(HostConfig::load(&path).map_err(|e| error(&format!("Invalid host configuration: {}: {}", e.path, e.detail)))?)` (keep `read_config(&path)?` before it for the existing size and file checks), and `join host` (`:890`) uses `HostConfig::load(&path)` the same way.

Note for the implementer: `std::fs::File::lock` is stable since Rust 1.89 (the toolchain is 1.98). `O_NOFOLLOW` differs between x86_64 and aarch64 (the hosts), so it comes from `libc`, never a literal.

- [ ] **Step 5: Run the tests to verify they pass.**

Run: `cargo test -p mllm-config --test registration --locked`
Expected: 7 passed.

- [ ] **Step 6: Run the crate suites and Clippy.**

Run: `cargo test -p mllm-config -p mllm-cli --all-targets --locked && cargo clippy -p mllm-config -p mllm-cli --all-targets --locked -- -D warnings`
Expected: all pass, no warnings (a host with no `engines.yaml` loads exactly as before).

- [ ] **Step 7: Commit**

```bash
git add crates/mllm-config crates/mllm-cli/src/remote_roles.rs Cargo.lock
git commit -m "feat(config): engines.yaml beside the role document, merged at load

ADR 0018 section 2 (owner decision 2026-09-25): engine registration
writes only an mllm-owned engines.yaml beside the role's configuration
file, under a lock, atomically, with the revision on the first line.
The host document is never rewritten; the host role merges the two at
load and refuses a profile name declared in both."
```

---

### Task 3: Profile builder, profile checks and the verified set

**Files:**
- Modify: `crates/mllm-config/src/registration.rs` (append)
- Modify: `crates/mllm-config/src/effective.rs` (new `pub fn check_runtime_profile` after `resolve_effective_with_checkpoint`, near line 851)
- Test: `crates/mllm-config/tests/registration.rs` (append)

**Interfaces:**
- Consumes: Task 2's `EnginesFile`; `engine_policy::{Engine, validate_profile_args}`; `effective::InstallationDrift`.
- Produces (used by Tasks 5, 8, 11, 13, 14, 15):
  - `pub const VERIFIED: &[(Engine, &str)] = &[(Engine::Vllm, "0.29.0"), (Engine::Sglang, "0.5.20")]`
  - `pub fn is_verified(engine: Engine, version: &str) -> bool`
  - `pub fn valid_profile_name(name: &str) -> bool` (`^[a-z0-9][a-z0-9_-]{0,63}$`)
  - `pub struct ProfileSpec { pub engine: Engine, pub executable: PathBuf, pub build_fingerprint: String, pub deep_park: bool, pub installation_drift: InstallationDrift, pub args: Vec<String> }`
  - `pub fn profile_document(spec: &ProfileSpec) -> Value`
  - `pub fn check_profile(name: &str, profile: &Value) -> Result<(), ConfigError>`
  - `pub fn only_profiles_differ(old: &Value, new: &Value) -> bool`
  - `pub fn removed_profiles(old: &Value, new: &Value) -> Vec<String>` and `pub fn added_profiles(old: &Value, new: &Value) -> Vec<String>`
  - `mllm_config::effective::check_runtime_profile(profile: &Value) -> Result<(), ConfigError>`
  - `pub const ENVIRONMENT_PROFILES: &[&str] = &["local", "local-vllm", "local-sglang"]`

- [ ] **Step 1: Write the failing tests.** Append to `crates/mllm-config/tests/registration.rs`:

```rust
use mllm_config::effective::InstallationDrift;
use mllm_config::engine_policy::Engine;

fn spec(engine: Engine) -> ProfileSpec {
    ProfileSpec {
        engine,
        executable: match engine {
            Engine::Vllm => "/home/u/venv/bin/vllm".into(),
            Engine::Sglang => "/home/u/venv/bin/python3".into(),
        },
        build_fingerprint: "0.29.0".into(),
        deep_park: true,
        installation_drift: InstallationDrift::Warn,
        args: vec![],
    }
}

// T14 T21: a built profile carries both per-launch key references, deep
// park as asked, drift only when refused, and passes the resolution rules.
#[test]
fn a_built_profile_passes_the_resolution_rules() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        let profile = profile_document(&spec(engine));
        assert_eq!(profile["revision"], 1);
        assert_eq!(profile["security"]["credential_ref"], "secret://engine-key");
        assert_eq!(profile["security"]["admin_credential_ref"], "secret://admin-key");
        assert_eq!(profile["security"]["deep_park"], "enabled");
        assert!(profile["security"].get("installation_drift").is_none());
        check_profile("vllm", &profile).unwrap();
    }
    let mut refusing = spec(Engine::Vllm);
    refusing.installation_drift = InstallationDrift::Refuse;
    refusing.deep_park = false;
    let profile = profile_document(&refusing);
    assert_eq!(profile["security"]["installation_drift"], "refuse");
    assert_eq!(profile["security"]["deep_park"], "disabled");
}

// T14: reserved arguments stay reserved; SGLang takes no host-fixed args.
#[test]
fn profile_arguments_follow_the_existing_rules() {
    let mut vllm = spec(Engine::Vllm);
    vllm.args = vec!["--port".into(), "1".into()];
    assert!(check_profile("vllm", &profile_document(&vllm)).is_err());
    vllm.args = vec!["--max-num-seqs".into(), "8".into()];
    check_profile("vllm", &profile_document(&vllm)).unwrap();
    let mut sglang = spec(Engine::Sglang);
    sglang.args = vec!["--mem-fraction-static".into(), "0.5".into()];
    assert!(check_profile("sglang", &profile_document(&sglang)).is_err());
}

// T01 T03: names are short lowercase identifiers.
#[test]
fn profile_names_are_bounded_identifiers() {
    for good in ["vllm", "sglang", "vllm-patched", "v2_exl3"] {
        assert!(valid_profile_name(good), "{good}");
    }
    for bad in ["", "Vllm", "-x", "a.b", "a/b", &"x".repeat(65)] {
        assert!(!valid_profile_name(bad), "{bad}");
    }
    assert!(check_profile("Bad Name", &profile_document(&spec(Engine::Vllm))).is_err());
}

// T22: the verified set; anything else is `custom`.
#[test]
fn the_verified_set_marks_custom_builds() {
    assert!(is_verified(Engine::Vllm, "0.29.0"));
    assert!(is_verified(Engine::Sglang, "0.5.20"));
    assert!(!is_verified(Engine::Sglang, "0.5.20+custom"));
    assert!(!is_verified(Engine::Vllm, "0.5.20"));
}

// ADR 0018 §3: only a change confined to runtime profiles is live.
#[test]
fn profile_only_changes_are_recognised() {
    let dir = tempfile::tempdir().unwrap();
    let old: serde_json::Value =
        serde_json::from_str(&HostConfig::template(&dir.path().join("state"))).unwrap();
    let mut new = old.clone();
    new["runtime_profiles"]["vllm"] = profile_document(&spec(Engine::Vllm));
    assert!(only_profiles_differ(&old, &new));
    assert_eq!(added_profiles(&old, &new), vec!["vllm".to_string()]);
    assert_eq!(removed_profiles(&new, &old), vec!["vllm".to_string()]);
    let mut edited = new.clone();
    edited["load_report_interval"] = "9s".into();
    assert!(!only_profiles_differ(&old, &edited));
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-config --test registration --locked`
Expected: FAIL to compile (`ProfileSpec`, `profile_document`, ... not found).

- [ ] **Step 3: Add `check_runtime_profile` to `effective.rs`.** After `resolve_effective_with_checkpoint` (around line 851) add:

```rust
/// ADR 0018: one runtime profile checked with the rules deployment
/// resolution applies (`core::normalize_profile`), before `mllm engine add`
/// writes it. A parking residency is checked too when the profile allows
/// deep parking, so a sleep-mode-reserved argument is refused now rather
/// than at the first deployment (T14, T21).
pub fn check_runtime_profile(profile: &serde_json::Value) -> Result<(), ConfigError> {
    let raw: RawProfile = decode(profile, "runtime_profiles")?;
    core::normalize_profile(&raw, raw.revision, Residency::RestartOnly)?;
    if raw.security.deep_park.is_enabled() {
        core::normalize_profile(&raw, raw.revision, Residency::Deep)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Append the builder and checks to `registration.rs`.**

```rust
use crate::effective::InstallationDrift;
use crate::engine_policy::Engine;

/// ADR 0018 §1: the versions the live matrix qualifies. Any other version is
/// shown `custom`; nothing is written for it.
pub const VERIFIED: &[(Engine, &str)] = &[(Engine::Vllm, "0.29.0"), (Engine::Sglang, "0.5.20")];

pub fn is_verified(engine: Engine, version: &str) -> bool {
    VERIFIED.iter().any(|(e, v)| *e == engine && *v == version)
}

/// ADR 0018 §5: the profile names standalone gives its environment-variable
/// installations (`MLLM_VLLM_BIN` / `MLLM_SGLANG_BIN`).
pub const ENVIRONMENT_PROFILES: &[&str] = &["local", "local-vllm", "local-sglang"];

/// ADR 0018 §1: short lowercase identifiers, safe in a JSON path and a
/// status table.
pub fn valid_profile_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

/// What `mllm engine add` measured and the operator chose.
#[derive(Debug, Clone)]
pub struct ProfileSpec {
    pub engine: Engine,
    pub executable: PathBuf,
    pub build_fingerprint: String,
    pub deep_park: bool,
    pub installation_drift: InstallationDrift,
    pub args: Vec<String>,
}

/// ADR 0018 §1: the profile `engine add` writes. SPEC §13.3, ADR 0012: every
/// launch seals an inference key and a separate admin key, so both
/// references are named (the coordinator resolves them per launch). ADR 0008:
/// drift is stated only when refused, so a default profile is unchanged.
pub fn profile_document(spec: &ProfileSpec) -> Value {
    let mut security = serde_json::json!({
        "deep_park": if spec.deep_park { "enabled" } else { "disabled" },
        "trust_remote_code": false,
        "credential_ref": "secret://engine-key",
        "admin_credential_ref": "secret://admin-key",
    });
    if spec.installation_drift == InstallationDrift::Refuse {
        security["installation_drift"] = "refuse".into();
    }
    serde_json::json!({
        "engine": match spec.engine { Engine::Vllm => "vllm", Engine::Sglang => "sglang" },
        "revision": 1,
        "executable": spec.executable.to_string_lossy(),
        "build_fingerprint": spec.build_fingerprint,
        "args": spec.args,
        "env": {},
        "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
        "security": security,
    })
}

/// ADR 0018 §1, SPEC §15.3: a profile is written only if its name is valid
/// and it passes the rules deployment resolution applies.
pub fn check_profile(name: &str, profile: &Value) -> Result<(), ConfigError> {
    if !valid_profile_name(name) {
        return Err(ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "runtime_profiles",
            format!("profile name {name:?} must be 1-64 lowercase letters, digits, '-' or '_', starting with a letter or digit"),
        ));
    }
    crate::effective::check_runtime_profile(profile)
}

fn without_profiles(document: &Value) -> Value {
    let mut copy = document.clone();
    if let Some(map) = copy.as_object_mut() {
        map.remove("runtime_profiles");
        if let Some(host) = map.get_mut("host").and_then(Value::as_object_mut) {
            host.remove("runtime_profiles");
        }
    }
    copy
}

fn profile_names(document: &Value) -> std::collections::BTreeSet<String> {
    document["runtime_profiles"]
        .as_object()
        .or_else(|| document["host"]["runtime_profiles"].as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// ADR 0018 §3: whether `new` differs from `old` in runtime profiles alone.
pub fn only_profiles_differ(old: &Value, new: &Value) -> bool {
    without_profiles(old) == without_profiles(new)
}

/// Profiles in `old` that `new` no longer has.
pub fn removed_profiles(old: &Value, new: &Value) -> Vec<String> {
    profile_names(old).difference(&profile_names(new)).cloned().collect()
}

/// Profiles in `new` that `old` did not have.
pub fn added_profiles(old: &Value, new: &Value) -> Vec<String> {
    profile_names(new).difference(&profile_names(old)).cloned().collect()
}
```

- [ ] **Step 5: Run the tests to verify they pass.**

Run: `cargo test -p mllm-config --all-targets --locked`
Expected: all pass, including the 5 new tests.

- [ ] **Step 6: Clippy and commit.**

Run: `cargo clippy -p mllm-config --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-config
git commit -m "feat(config): engine profile builder, checks and verified set

ADR 0018: the profile engine add writes (both per-launch key references,
deep park and drift as chosen), checked with the rules deployment
resolution applies. The verified set (vLLM 0.29.0, SGLang 0.5.20)
decides the derived custom mark."
```

---

### Task 4: Resolve an installation and check its version

**Files:**
- Create: `crates/mllm-agent/src/engines.rs` (module root, package metadata reader)
- Create: `crates/mllm-agent/src/engines/resolve.rs`
- Create: `crates/mllm-agent/tests/engines.rs`
- Modify: `crates/mllm-agent/src/lib.rs` (add `pub mod engines;` beside `pub mod installation;`, line 23)

**Interfaces:**
- Consumes: `mllm_config::engine_policy::Engine`; `mllm_config::registration::is_verified` (Task 3).
- Produces (used by Tasks 5, 17, 20):
  - `pub fn packages(env: &Path) -> Vec<(Engine, String)>` (engine, dist-info version), sorted, metadata only
  - `pub struct Resolved { pub engine: Engine, pub version: String, pub env: PathBuf, pub executable: PathBuf }` with `pub fn custom(&self) -> bool`
  - `pub enum ResolveError { NotFound(String), Unsupported(String) }` with `code(&self) -> &'static str` (`engine_not_found` / `engine_unsupported`) and `Display`
  - `pub fn resolve(path: &Path) -> Result<Resolved, ResolveError>`
  - `pub const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(60)`, `pub const VERSION_OUTPUT_LIMIT: usize = 4096`
  - `pub enum VersionCheckError { Spawn, TimedOut, Failed, Output, Mismatch { reported: String, installed: String } }` with `Display`
  - `pub fn check_version(resolved: &Resolved, timeout: Duration) -> Result<String, VersionCheckError>`

- [ ] **Step 1: Write the failing tests.** Create `crates/mllm-agent/tests/engines.rs`:

```rust
//! ADR 0018 §1: resolving a named installation and its bounded version
//! check. Fake environments only: CPU evidence, never qualification.
use mllm_agent::engines::*;
use mllm_config::engine_policy::Engine;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A venv-shaped tree: `bin/`, `lib/python3.12/site-packages/<pkg>-<v>.dist-info`.
pub fn fake_env(root: &Path, packages: &[(&str, &str)]) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    for (name, version) in packages {
        let info = site.join(format!("{name}-{version}.dist-info"));
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("METADATA"), format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n")).unwrap();
        std::fs::create_dir_all(site.join(name)).unwrap();
    }
    root.to_path_buf()
}

pub fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// T07 T22: a venv directory, its bin/vllm and its bin/python3 all resolve to
// the same environment; the engine and version come from dist-info.
#[test]
fn a_named_path_resolves_to_its_environment() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(&dir.path().join("v"), &[("vllm", "0.29.0")]);
    script(&env.join("bin/vllm"), "echo 0.29.0");
    for named in [env.clone(), env.join("bin/vllm")] {
        let resolved = resolve(&named).unwrap();
        assert_eq!(resolved.engine, Engine::Vllm);
        assert_eq!(resolved.version, "0.29.0");
        assert_eq!(resolved.env, env);
        assert_eq!(resolved.executable, env.join("bin/vllm"));
        assert!(!resolved.custom());
    }
    let sg = fake_env(&dir.path().join("s"), &[("sglang", "0.5.20+custom")]);
    script(&sg.join("bin/python3"), "echo 0.5.20+custom");
    let resolved = resolve(&sg.join("bin/python3")).unwrap();
    assert_eq!(resolved.engine, Engine::Sglang);
    assert_eq!(resolved.executable, sg.join("bin/python3"));
    assert!(resolved.custom());
}

// T37 (Review Focus 2): a venv's python3 is a symlink to the system
// interpreter; resolution stays in the venv.
#[test]
fn the_interpreter_symlink_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let system = fake_env(&dir.path().join("system"), &[("vllm", "0.1.0")]);
    script(&system.join("bin/python3"), "echo 0.1.0");
    let env = fake_env(&dir.path().join("venv"), &[("sglang", "0.5.20")]);
    std::os::unix::fs::symlink(system.join("bin/python3"), env.join("bin/python3")).unwrap();
    let resolved = resolve(&env.join("bin/python3")).unwrap();
    assert_eq!(resolved.env, env);
    assert_eq!(resolved.engine, Engine::Sglang);
    assert_eq!(resolved.executable, env.join("bin/python3"));
}

// T07: no engine package, not an environment, and ambiguity are named.
#[test]
fn unresolvable_paths_are_refused_with_their_code() {
    let dir = tempfile::tempdir().unwrap();
    let empty = fake_env(&dir.path().join("e"), &[("numpy", "2.0.0")]);
    assert_eq!(resolve(&empty).unwrap_err().code(), "engine_not_found");
    std::fs::create_dir_all(dir.path().join("plain")).unwrap();
    assert_eq!(resolve(&dir.path().join("plain")).unwrap_err().code(), "engine_unsupported");
    let both = fake_env(&dir.path().join("b"), &[("vllm", "0.29.0"), ("sglang", "0.5.20")]);
    script(&both.join("bin/vllm"), "echo 0.29.0");
    script(&both.join("bin/python3"), "echo 0.5.20");
    let error = resolve(&both).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(error.to_string().contains("bin/vllm"), "{error}");
    assert_eq!(resolve(&both.join("bin/vllm")).unwrap().engine, Engine::Vllm);
    assert_eq!(resolve(&both.join("bin/python3")).unwrap().engine, Engine::Sglang);
    assert_eq!(resolve(&dir.path().join("b/../b")).unwrap_err().code(), "engine_unsupported");
}

// T37: the version check is bounded, sees no inherited environment, and
// must agree with dist-info.
#[test]
fn the_version_check_is_bounded_and_must_agree() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(&dir.path().join("v"), &[("vllm", "0.29.0")]);
    script(&env.join("bin/vllm"), "echo 0.29.0");
    let resolved = resolve(&env).unwrap();
    assert_eq!(check_version(&resolved, Duration::from_secs(5)).unwrap(), "0.29.0");

    std::env::set_var("MLLM_TEST_LEAK", "leaked");
    script(&env.join("bin/vllm"), "echo \"${MLLM_TEST_LEAK:-0.29.0}\"");
    assert_eq!(check_version(&resolved, Duration::from_secs(5)).unwrap(), "0.29.0");

    script(&env.join("bin/vllm"), "echo 0.28.0");
    assert!(matches!(
        check_version(&resolved, Duration::from_secs(5)),
        Err(VersionCheckError::Mismatch { .. })
    ));
    script(&env.join("bin/vllm"), "exit 3");
    assert!(matches!(check_version(&resolved, Duration::from_secs(5)), Err(VersionCheckError::Failed)));
    script(&env.join("bin/vllm"), "sleep 5; echo 0.29.0");
    let started = std::time::Instant::now();
    assert!(matches!(check_version(&resolved, Duration::from_millis(300)), Err(VersionCheckError::TimedOut)));
    assert!(started.elapsed() < Duration::from_secs(3));
    script(&env.join("bin/vllm"), "yes 0.29.0 | head -c 100000");
    assert!(matches!(check_version(&resolved, Duration::from_secs(5)), Err(VersionCheckError::Output)));
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-agent --test engines --locked`
Expected: FAIL to compile (`mllm_agent::engines` not found).

- [ ] **Step 3: Implement the module root.** Create `crates/mllm-agent/src/engines.rs`:

```rust
//! ADR 0018 §1: engine installations the operator registers. Detection and
//! resolution read package metadata only; nothing found here is executed
//! until the operator names or picks it (ADR 0008 carve-out 2, SPEC §4.2).
use mllm_config::engine_policy::Engine;
use std::path::{Path, PathBuf};

pub mod detect;
pub mod resolve;
pub use detect::*;
pub use resolve::*;

/// Entries read from one `site-packages` directory at most.
pub(crate) const MAX_SITE_ENTRIES: usize = 65_536;
/// Bytes read from one `METADATA` file at most.
const MAX_METADATA: u64 = 1 << 20;

fn engine_of(package: &str) -> Option<Engine> {
    match package {
        "vllm" => Some(Engine::Vllm),
        "sglang" => Some(Engine::Sglang),
        _ => None,
    }
}

/// The environment's `lib/python3.*/site-packages` directories, sorted.
/// Symlinked entries are skipped: a scan never leaves the root it reads.
pub(crate) fn site_packages(env: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(env.join("lib")) else { return Vec::new() };
    let mut sites: Vec<PathBuf> = entries
        .flatten()
        .take(256)
        .filter(|e| e.file_name().to_string_lossy().starts_with("python3"))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path().join("site-packages"))
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .collect();
    sites.sort();
    sites
}

fn metadata_version(info: &Path) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(info.join("METADATA")).ok()?;
    let mut text = String::new();
    file.take(MAX_METADATA).read_to_string(&mut text).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("Version: "))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_graphic()))
}

/// ADR 0018 §1: the vLLM and SGLang packages an environment holds, from
/// `<name>-<version>.dist-info` directories (not symlinks) and their
/// `METADATA` `Version:` line. Metadata only; bounded.
pub fn packages(env: &Path) -> Vec<(Engine, String)> {
    let mut found = Vec::new();
    for site in site_packages(env) {
        let Ok(entries) = std::fs::read_dir(&site) else { continue };
        for entry in entries.flatten().take(MAX_SITE_ENTRIES) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".dist-info") else { continue };
            let Some((package, _)) = stem.split_once('-') else { continue };
            let Some(engine) = engine_of(&package.to_ascii_lowercase()) else { continue };
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Some(version) = metadata_version(&entry.path()) {
                found.push((engine, version));
            }
        }
    }
    found.sort_by(|a, b| (a.0 as u8, &a.1).cmp(&(b.0 as u8, &b.1)));
    found.dedup();
    found
}
```

(`Engine` is a fieldless enum; if `as u8` is rejected because it lacks `#[repr]`, sort by `format!("{:?}", e)` instead.)

- [ ] **Step 4: Implement resolution and the version check.** Create `crates/mllm-agent/src/engines/resolve.rs`:

```rust
//! ADR 0018 §1: from a path the operator named to the environment and the
//! entry point the engine is launched with, then the bounded version check.
use super::packages;
use mllm_config::engine_policy::Engine;
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
pub const VERSION_OUTPUT_LIMIT: usize = 4096;
/// Reads the installed version the interpreter would import, without
/// importing the engine.
const SGLANG_VERSION: &str =
    "import importlib.metadata,sys;print(importlib.metadata.version(sys.argv[1]))";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub engine: Engine,
    pub version: String,
    pub env: PathBuf,
    pub executable: PathBuf,
}

impl Resolved {
    /// ADR 0018 §1: outside the verified set.
    pub fn custom(&self) -> bool {
        !mllm_config::registration::is_verified(self.engine, &self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound(String),
    Unsupported(String),
}

impl ResolveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "engine_not_found",
            Self::Unsupported(_) => "engine_unsupported",
        }
    }
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Unsupported(m) => f.write_str(m),
        }
    }
}

pub(crate) fn entry(env: &Path, engine: Engine) -> PathBuf {
    match engine {
        Engine::Vllm => env.join("bin/vllm"),
        Engine::Sglang => env.join("bin/python3"),
    }
}

/// ADR 0018 §1: `path` is a venv directory, its `bin/vllm`, or its
/// `bin/python3` (`python`, `python3.N`). Lexical: a venv's interpreter is a
/// symlink to the system Python and is never followed (Review Focus 2).
pub fn resolve(path: &Path) -> Result<Resolved, ResolveError> {
    let path = std::path::absolute(path)
        .map_err(|_| ResolveError::Unsupported(format!("{} cannot be made absolute", path.display())))?;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ResolveError::Unsupported(format!(
            "{} contains '..'; name the environment directly",
            path.display()
        )));
    }
    let meta = std::fs::metadata(&path)
        .map_err(|_| ResolveError::NotFound(format!("{} does not exist", path.display())))?;
    let (env, wanted) = if meta.is_dir() {
        (path.clone(), None)
    } else {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bin = path.parent().filter(|p| p.file_name().is_some_and(|n| n == "bin"));
        let env = bin.and_then(Path::parent).map(Path::to_path_buf).ok_or_else(|| {
            ResolveError::Unsupported(format!("{} is not inside an environment's bin directory", path.display()))
        })?;
        let wanted = if name == "vllm" {
            Engine::Vllm
        } else if name == "python" || name == "python3" || name.starts_with("python3.") {
            Engine::Sglang
        } else {
            return Err(ResolveError::Unsupported(format!(
                "{} is neither bin/vllm nor bin/python3",
                path.display()
            )));
        };
        (env, Some(wanted))
    };
    if super::site_packages(&env).is_empty() {
        return Err(ResolveError::Unsupported(format!(
            "{} is not a Python environment (no lib/python3.*/site-packages)",
            env.display()
        )));
    }
    let found = packages(&env);
    if found.is_empty() {
        return Err(ResolveError::NotFound(format!("{} holds no vllm or sglang package", env.display())));
    }
    let (engine, version) = match wanted {
        Some(engine) => found.iter().find(|(e, _)| *e == engine).cloned().ok_or_else(|| {
            ResolveError::NotFound(format!("{} holds no {} package", env.display(), mllm_agent_engine_name(engine)))
        })?,
        None if found.len() == 1 => found[0].clone(),
        None => {
            return Err(ResolveError::Unsupported(format!(
                "{} holds both vllm and sglang; name {} for vLLM or {} for SGLang",
                env.display(),
                entry(&env, Engine::Vllm).display(),
                entry(&env, Engine::Sglang).display()
            )))
        }
    };
    let executable = entry(&env, engine);
    if std::fs::symlink_metadata(&executable).is_err() {
        return Err(ResolveError::NotFound(format!(
            "{} has the {} package but no {}",
            env.display(),
            mllm_agent_engine_name(engine),
            executable.display()
        )));
    }
    Ok(Resolved { engine, version, env, executable })
}

fn mllm_agent_engine_name(engine: Engine) -> &'static str {
    crate::installation::package_name(engine)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheckError {
    Spawn,
    TimedOut,
    Failed,
    Output,
    Mismatch { reported: String, installed: String },
}

impl std::fmt::Display for VersionCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn => f.write_str("the version check could not be started"),
            Self::TimedOut => f.write_str("the version check timed out"),
            Self::Failed => f.write_str("the version check exited unsuccessfully"),
            Self::Output => f.write_str("the version check printed no usable version"),
            Self::Mismatch { reported, installed } => write!(
                f,
                "the engine reports {reported} but its package metadata says {installed}"
            ),
        }
    }
}

/// ADR 0018 §1: run the installation only now that the operator named it.
/// Cleared environment, stdin and stderr closed, own process group, killed
/// at `timeout`, at most [`VERSION_OUTPUT_LIMIT`] bytes kept. The reported
/// version must equal the dist-info version.
pub fn check_version(resolved: &Resolved, timeout: Duration) -> Result<String, VersionCheckError> {
    let mut command = Command::new(&resolved.executable);
    match resolved.engine {
        Engine::Vllm => command.arg("--version"),
        Engine::Sglang => command.args(["-I", "-B", "-c", SGLANG_VERSION, "sglang"]),
    };
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    let mut child = command.spawn().map_err(|_| VersionCheckError::Spawn)?;
    let pgid = child.id() as i32;
    let mut stdout = child.stdout.take().ok_or(VersionCheckError::Spawn)?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stdout).take(VERSION_OUTPUT_LIMIT as u64 + 1).read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                // SAFETY: signals only the process group this call created.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
                let _ = child.wait();
                return Err(VersionCheckError::TimedOut);
            }
        }
    };
    // The group may still hold a writer (a pipeline); end it before reading.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let output = reader.join().map_err(|_| VersionCheckError::Output)?;
    // Too much output first: a writer cut off by the closed pipe also exits
    // unsuccessfully, and the cause is the output.
    if output.len() > VERSION_OUTPUT_LIMIT {
        return Err(VersionCheckError::Output);
    }
    if !status.success() {
        return Err(VersionCheckError::Failed);
    }
    let text = String::from_utf8(output).map_err(|_| VersionCheckError::Output)?;
    let line = text.lines().map(str::trim).filter(|l| !l.is_empty()).last().ok_or(VersionCheckError::Output)?;
    let reported = line.rsplit(' ').next().unwrap_or(line).to_owned();
    if reported != resolved.version {
        return Err(VersionCheckError::Mismatch { reported, installed: resolved.version.clone() });
    }
    Ok(reported)
}
```

Create an empty `crates/mllm-agent/src/engines/detect.rs` containing only `//! ADR 0018 §1: detection (Task 5).` so the module compiles; Task 5 fills it.

- [ ] **Step 5: Run the tests to verify they pass.**

Run: `cargo test -p mllm-agent --test engines --locked`
Expected: 4 passed. (In the `yes | head` case the reader stops after 4097 bytes and drops the pipe; the writer dies of `SIGPIPE`, and the check reports `Output` because the length is judged before the status.)

- [ ] **Step 6: Clippy and commit.**

Run: `cargo clippy -p mllm-agent --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-agent/src/lib.rs crates/mllm-agent/src/engines.rs crates/mllm-agent/src/engines crates/mllm-agent/tests/engines.rs
git commit -m "feat(agent): resolve a named engine installation and check its version

ADR 0018 section 1: a venv directory, its bin/vllm or its bin/python3
resolves lexically to the environment and the entry point; engine and
version come from dist-info. The version check runs only for a named
installation: cleared environment, own process group, 60 s, 4 KiB, and
it must agree with the package metadata."
```

---

### Task 5: Metadata-only detection

**Files:**
- Modify: `crates/mllm-agent/src/engines/detect.rs` (replace the placeholder from Task 4)
- Test: `crates/mllm-agent/tests/engines.rs` (append)

**Interfaces:**
- Consumes: Task 4's `packages(&Path)`, `site_packages`, `resolve::entry`, `Resolved::custom` rule (`is_verified`).
- Produces (used by Tasks 17 and 20):
  - `pub struct Candidate { pub engine: Engine, pub version: String, pub env: PathBuf, pub entry: PathBuf, pub source: &'static str, pub custom: bool }`
  - `pub struct ScanBounds { pub max_envs: usize, pub max_depth: usize, pub max_dir_entries: usize }` (`Default`: 512, 3, 4096)
  - `pub struct ScanRoots { pub path_dirs: Vec<PathBuf>, pub home: Option<PathBuf>, pub xdg_data: Option<PathBuf>, pub pipx_home: Option<PathBuf>, pub conda_roots: Vec<PathBuf>, pub opt: Option<PathBuf>, pub extra: Vec<PathBuf> }` with `pub fn from_env(extra: Vec<PathBuf>) -> Self`
  - `pub fn detect(roots: &ScanRoots, bounds: &ScanBounds) -> Vec<Candidate>`

- [ ] **Step 1: Write the failing tests.** Append to `crates/mllm-agent/tests/engines.rs`:

```rust
fn empty_roots(home: &Path) -> ScanRoots {
    ScanRoots {
        path_dirs: vec![],
        home: Some(home.to_path_buf()),
        xdg_data: Some(home.join(".local/share")),
        pipx_home: None,
        conda_roots: vec![home.join("miniconda3")],
        opt: None,
        extra: vec![],
    }
}

// T07 T37: every documented location is found, and detection runs nothing.
#[test]
fn detection_finds_the_documented_locations_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let marker = dir.path().join("executed");
    let run_marker = format!("touch {}; echo 0.29.0", marker.display());
    let venvs = fake_env(&home.join("venvs/a"), &[("vllm", "0.29.0")]);
    script(&venvs.join("bin/vllm"), &run_marker);
    let dot = fake_env(&home.join(".venv"), &[("sglang", "0.5.20")]);
    script(&dot.join("bin/python3"), &run_marker);
    let conda = fake_env(&home.join("miniconda3/envs/sg"), &[("sglang", "0.5.21")]);
    script(&conda.join("bin/python3"), &run_marker);
    let listed = fake_env(&dir.path().join("elsewhere/env"), &[("vllm", "0.30.0")]);
    script(&listed.join("bin/vllm"), &run_marker);
    std::fs::create_dir_all(home.join(".conda")).unwrap();
    std::fs::write(home.join(".conda/environments.txt"), format!("{}\n", listed.display())).unwrap();
    let uv = fake_env(&home.join(".local/share/uv/tools/vllm"), &[("vllm", "0.29.0")]);
    script(&uv.join("bin/vllm"), &run_marker);
    let on_path = fake_env(&dir.path().join("pathenv"), &[("vllm", "0.29.0")]);
    script(&on_path.join("bin/vllm"), &run_marker);
    let mut roots = empty_roots(&home);
    roots.path_dirs = vec![on_path.join("bin"), PathBuf::from("/nonexistent/bin")];
    let found = detect(&roots, &ScanBounds::default());
    let envs: std::collections::BTreeSet<_> = found.iter().map(|c| c.env.clone()).collect();
    for env in [&venvs, &dot, &conda, &listed, &uv, &on_path] {
        assert!(envs.contains(env), "{} missing from {found:?}", env.display());
    }
    let custom: Vec<_> = found.iter().filter(|c| c.custom).map(|c| c.version.clone()).collect();
    assert!(custom.contains(&"0.5.21".to_string()) && custom.contains(&"0.30.0".to_string()));
    assert!(!marker.exists(), "detection executed an installation");
}

// T07 T37 (owner decision 2026-09-25): environments directly in the home
// directory (the hosts' `~/mllm-vllm-venv2` layout) are found without
// `--path`, one level deep and only when they carry `pyvenv.cfg`.
#[test]
fn home_level_environments_are_found_without_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let venv = fake_env(&home.join("mllm-vllm-venv2"), &[("vllm", "0.29.0")]);
    script(&venv.join("bin/vllm"), "echo 0.29.0");
    let bare = fake_env(&home.join("not-a-venv"), &[("sglang", "0.5.20")]);
    std::fs::remove_file(bare.join("pyvenv.cfg")).unwrap();
    script(&bare.join("bin/python3"), "echo 0.5.20");
    let deeper = fake_env(&home.join("projects/env"), &[("vllm", "0.29.0")]);
    script(&deeper.join("bin/vllm"), "echo 0.29.0");
    let found = detect(&empty_roots(&home), &ScanBounds::default());
    let envs: Vec<_> = found.iter().map(|c| c.env.clone()).collect();
    assert!(envs.contains(&venv), "{found:?}");
    assert!(found.iter().any(|c| c.env == venv && c.source == "home"));
    assert!(!envs.contains(&bare), "no pyvenv.cfg: not a home-level venv");
    assert!(!envs.contains(&deeper), "one level deep only");
}

// T37: a symlink that resolves outside the scanned root is not followed.
#[test]
fn a_symlink_escaping_its_root_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let outside = fake_env(&dir.path().join("outside/env"), &[("vllm", "0.29.0")]);
    script(&outside.join("bin/vllm"), "echo 0.29.0");
    std::fs::create_dir_all(home.join("venvs")).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("venvs/escape")).unwrap();
    let inside = fake_env(&home.join("venvs/real"), &[("vllm", "0.29.0")]);
    script(&inside.join("bin/vllm"), "echo 0.29.0");
    std::os::unix::fs::symlink(&inside, home.join("venvs/alias")).unwrap();
    let found = detect(&empty_roots(&home), &ScanBounds::default());
    assert!(found.iter().all(|c| !c.env.starts_with(dir.path().join("outside"))), "{found:?}");
    assert_eq!(found.iter().filter(|c| c.engine == Engine::Vllm).count(), 1, "alias deduplicated: {found:?}");
}

// T37: the scan is bounded in environments and depth.
#[test]
fn the_scan_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let wide = dir.path().join("wide");
    for i in 0..40 {
        let env = fake_env(&wide.join(format!("e{i}")), &[("vllm", "0.29.0")]);
        script(&env.join("bin/vllm"), "echo 0.29.0");
    }
    let deep = fake_env(&dir.path().join("deep/a/b/c/d/env"), &[("vllm", "0.29.0")]);
    script(&deep.join("bin/vllm"), "echo 0.29.0");
    let mut roots = empty_roots(&dir.path().join("nohome"));
    roots.extra = vec![wide.clone(), dir.path().join("deep")];
    let bounds = ScanBounds { max_envs: 10, max_depth: 3, max_dir_entries: 4096 };
    let found = detect(&roots, &bounds);
    assert!(found.len() <= 10, "{}", found.len());
    assert!(found.iter().all(|c| c.env != deep), "depth bound exceeded");
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-agent --test engines --locked detection home_level a_symlink the_scan`
Expected: FAIL to compile (`ScanRoots`, `detect` not found).

- [ ] **Step 3: Implement.** Replace `crates/mllm-agent/src/engines/detect.rs` with:

```rust
//! ADR 0018 §1: `mllm engine detect`. Reads package metadata only, executes
//! nothing, never follows a symlink that resolves outside the root being
//! scanned, and is bounded in environments, depth and directory entries.
use super::{packages, resolve::entry, site_packages};
use mllm_config::engine_policy::Engine;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub engine: Engine,
    pub version: String,
    pub env: PathBuf,
    pub entry: PathBuf,
    /// Where it was found: `PATH`, `conda`, `home`, `venv`, `uv`, `pipx`, `opt` or `path`.
    pub source: &'static str,
    pub custom: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ScanBounds {
    pub max_envs: usize,
    pub max_depth: usize,
    pub max_dir_entries: usize,
}

impl Default for ScanBounds {
    fn default() -> Self {
        Self { max_envs: 512, max_depth: 3, max_dir_entries: 4096 }
    }
}

/// Where detection looks. `from_env` fills it from the process environment;
/// tests build it directly.
#[derive(Debug, Clone, Default)]
pub struct ScanRoots {
    pub path_dirs: Vec<PathBuf>,
    pub home: Option<PathBuf>,
    pub xdg_data: Option<PathBuf>,
    pub pipx_home: Option<PathBuf>,
    pub conda_roots: Vec<PathBuf>,
    pub opt: Option<PathBuf>,
    pub extra: Vec<PathBuf>,
}

/// ADR 0018 §1 (owner decision 2026-09-25): the conda roots detection reads.
const HOME_CONDA_ROOTS: &[&str] = &["miniconda3", "anaconda3", "miniforge3", "mambaforge", ".conda"];
const SYSTEM_CONDA_ROOTS: &[&str] = &["/opt/conda", "/opt/miniconda3", "/opt/anaconda3"];

impl ScanRoots {
    pub fn from_env(extra: Vec<PathBuf>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute());
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).take(256).collect())
            .unwrap_or_default();
        let xdg_data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        let pipx_home = std::env::var_os("PIPX_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".local/pipx")));
        let mut conda_roots: Vec<PathBuf> =
            home.iter().flat_map(|h| HOME_CONDA_ROOTS.iter().map(move |r| h.join(r))).collect();
        conda_roots.extend(SYSTEM_CONDA_ROOTS.iter().map(PathBuf::from));
        Self { path_dirs, home, xdg_data, pipx_home, conda_roots, opt: Some("/opt".into()), extra }
    }
}

struct Scan<'a> {
    bounds: &'a ScanBounds,
    seen: BTreeSet<PathBuf>,
    examined: usize,
    found: Vec<Candidate>,
}

impl Scan<'_> {
    /// Examine one environment root. Metadata only.
    fn env(&mut self, env: &Path, source: &'static str) {
        if self.examined >= self.bounds.max_envs {
            return;
        }
        let Ok(key) = env.canonicalize() else { return };
        if !self.seen.insert(key) || site_packages(env).is_empty() {
            return;
        }
        self.examined += 1;
        for (engine, version) in packages(env) {
            let entry = entry(env, engine);
            if std::fs::symlink_metadata(&entry).is_err() {
                continue;
            }
            let custom = !mllm_config::registration::is_verified(engine, &version);
            self.found.push(Candidate { engine, version, env: env.to_path_buf(), entry, source, custom });
        }
    }

    /// The directories directly under `root`; a symlink is kept only if it
    /// resolves inside `root` (spec: never follow one out of the root).
    fn children(&self, root: &Path) -> Vec<PathBuf> {
        let Ok(canonical_root) = root.canonicalize() else { return Vec::new() };
        let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .take(self.bounds.max_dir_entries)
            .filter_map(|e| {
                let path = e.path();
                let kind = e.file_type().ok()?;
                if kind.is_symlink() {
                    let target = path.canonicalize().ok()?;
                    (target.starts_with(&canonical_root) && target.is_dir()).then_some(path)
                } else {
                    kind.is_dir().then_some(path)
                }
            })
            .collect();
        children.sort();
        children
    }

    /// `--path`: the directory itself, or environments below it, to `depth`.
    fn tree(&mut self, root: &Path, depth: usize) {
        if !site_packages(root).is_empty() {
            self.env(root, "path");
            return;
        }
        if depth == 0 {
            return;
        }
        for child in self.children(root) {
            self.tree(&child, depth - 1);
        }
    }
}

/// ADR 0018 §1: candidates in the documented locations, deduplicated by
/// canonical environment, in scan order.
pub fn detect(roots: &ScanRoots, bounds: &ScanBounds) -> Vec<Candidate> {
    let mut scan = Scan { bounds, seen: BTreeSet::new(), examined: 0, found: Vec::new() };
    for dir in &roots.path_dirs {
        if dir.file_name().is_some_and(|n| n == "bin") {
            if let Some(env) = dir.parent() {
                scan.env(env, "PATH");
            }
        }
    }
    if let Some(home) = &roots.home {
        let listed = home.join(".conda/environments.txt");
        if let Ok(text) = std::fs::read_to_string(&listed) {
            for line in text.lines().take(256).map(str::trim).filter(|l| l.starts_with('/')) {
                scan.env(Path::new(line), "conda");
            }
        }
    }
    for root in &roots.conda_roots {
        scan.env(root, "conda");
        for env in scan.children(&root.join("envs")) {
            scan.env(&env, "conda");
        }
    }
    if let Some(home) = &roots.home {
        // Owner decision 2026-09-25: a venv directly in the home directory,
        // one level deep, recognised by its `pyvenv.cfg` (metadata only).
        for child in scan.children(home) {
            if std::fs::symlink_metadata(child.join("pyvenv.cfg")).is_ok_and(|m| m.is_file()) {
                scan.env(&child, "home");
            }
        }
        scan.env(&home.join(".venv"), "venv");
        for parent in ["venvs", ".virtualenvs"] {
            for env in scan.children(&home.join(parent)) {
                scan.env(&env, "venv");
            }
        }
        for env in scan.children(&home.join(".local/share/pipx/venvs")) {
            scan.env(&env, "pipx");
        }
    }
    if let Some(data) = &roots.xdg_data {
        for env in scan.children(&data.join("uv/tools")) {
            scan.env(&env, "uv");
        }
    }
    if let Some(pipx) = &roots.pipx_home {
        for env in scan.children(&pipx.join("venvs")) {
            scan.env(&env, "pipx");
        }
    }
    if let Some(opt) = &roots.opt {
        for child in scan.children(opt) {
            scan.env(&child, "opt");
            scan.env(&child.join("venv"), "opt");
            scan.env(&child.join(".venv"), "opt");
        }
    }
    for extra in &roots.extra {
        scan.tree(extra, bounds.max_depth);
    }
    scan.found
}
```

- [ ] **Step 4: Run the tests to verify they pass.**

Run: `cargo test -p mllm-agent --test engines --locked`
Expected: 8 passed.

- [ ] **Step 5: Clippy and commit.**

Run: `cargo clippy -p mllm-agent --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-agent/src/engines/detect.rs crates/mllm-agent/tests/engines.rs
git commit -m "feat(agent): metadata-only engine detection

ADR 0018 section 1: mllm engine detect scans PATH environments, conda,
home-level venvs, venv, uv, pipx, /opt and --path roots for vllm and sglang dist-info,
executes nothing, never follows a symlink out of its root, and is
bounded in environments, depth and directory entries."
```

---

### Task 6: Protocol: `live_profile_update`, the new session messages, server capabilities

**Files:**
- Modify: `crates/mllm-protocol/proto/mllm/management/v1/management.proto` (`AgentToServer` oneof at `:42-61`, `ServerToAgent` oneof at `:207-222`, `SessionReady` at `:416-424`; new messages after `HostDrainAcknowledged` at `:70`)
- Modify: `crates/mllm-protocol/src/capabilities.rs` (constant after `INSTALLATION_FINGERPRINT` at `:55`, `CATALOGUE` at `:67-82`, new `server_capabilities()`)
- Modify: `crates/mllm-protocol/tests/version_skew.rs:195-196` (server-to-host list)
- Test: `crates/mllm-protocol/tests/version_skew.rs` (append)

**Interfaces:**
- Consumes: ADR 0017 catalogue conventions.
- Produces (used by Tasks 9, 10, 13, 14):
  - `pb::PublishProfiles { request_id: String, inventory: Option<pb::ReportInventory> }` as `agent_to_server::Msg::PublishProfiles` (field 11)
  - `pb::RetireProfile { request_id: String, profile: String, drain: bool }` as `agent_to_server::Msg::RetireProfile` (field 12)
  - `pb::ProfilesPublished { request_id: String, accepted: bool, reason: String }` as `server_to_agent::Msg::ProfilesPublished` (field 12)
  - `pb::ProfileRetirement { request_id: String, outcome: String, deployments: Vec<String>, reason: String }` as `server_to_agent::Msg::ProfileRetirement` (field 13); `outcome` ∈ `confirmed | in_use | draining | holding | refused`
  - `pb::SessionReady.capabilities: Vec<String>` (field 5)
  - `capabilities::LIVE_PROFILE_UPDATE: &str = "live_profile_update"`, `capabilities::server_capabilities() -> Vec<String>`
  - `pub const MAX_REQUEST_ID: usize = 64`, `pub const MAX_RETIREMENT_DEPLOYMENTS: usize = 256`, `pub const MAX_REASON: usize = 512` in `capabilities.rs`

- [ ] **Step 1: Write the failing test.** Append to `crates/mllm-protocol/tests/version_skew.rs`:

```rust
// T34 (ADR 0018 §3): live profile updates are a server-to-host capability
// that every current agent declares and the server advertises back; the new
// messages are additive and an absent SessionReady list encodes as before.
#[test]
fn live_profile_update_is_declared_both_ways() {
    use capabilities::*;
    assert!(CATALOGUE.contains(&(LIVE_PROFILE_UPDATE, Direction::ServerToHost)));
    assert!(agent_capabilities().contains(&LIVE_PROFILE_UPDATE.to_owned()));
    assert_eq!(server_capabilities(), vec![LIVE_PROFILE_UPDATE.to_owned()]);
    let before = pb::SessionReady { controller_id: "c".into(), session_id: "s".into(), ..Default::default() };
    let mut after = before.clone();
    assert_eq!(before.encode_to_vec(), after.encode_to_vec());
    after.capabilities = server_capabilities();
    assert!(after.encode_to_vec().len() > before.encode_to_vec().len());
    let publish = pb::AgentToServer {
        msg: Some(pb::agent_to_server::Msg::PublishProfiles(pb::PublishProfiles {
            request_id: "01J00000000000000000000001".into(),
            inventory: Some(pb::ReportInventory::default()),
        })),
    };
    let decoded = pb::AgentToServer::decode(publish.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, publish);
    let retire = pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ProfileRetirement(pb::ProfileRetirement {
            request_id: "r".into(),
            outcome: "in_use".into(),
            deployments: vec!["q14".into()],
            reason: String::new(),
        })),
    };
    assert_eq!(pb::ServerToAgent::decode(retire.encode_to_vec().as_slice()).unwrap(), retire);
}
```

(If `Message` is not yet imported in this test file, add `use prost::Message;` at the top; the file already encodes with `encode_to_vec`.)

- [ ] **Step 2: Run the test to verify it fails.**

Run: `cargo test -p mllm-protocol --test version_skew --locked live_profile_update`
Expected: FAIL to compile (`PublishProfiles`, `LIVE_PROFILE_UPDATE` not found).

- [ ] **Step 3: Extend the proto.** In `AgentToServer`'s oneof, after `Heartbeat heartbeat = 10;` add:

```proto
    // ADR 0018 (capability `live_profile_update`, additive): after `mllm
    // engine add` or `remove`, the host re-publishes its whole preparation on
    // the live session. The server answers with ProfilesPublished and keeps
    // the session whatever it decides.
    PublishProfiles publish_profiles = 11;
    // ADR 0018 §4: first phase of removing a published profile.
    RetireProfile retire_profile = 12;
```

In `ServerToAgent`'s oneof, after `Heartbeat heartbeat = 11;` add:

```proto
    // ADR 0018: sent only to a host whose Connect declared
    // `live_profile_update`, and only in answer to its request.
    ProfilesPublished profiles_published = 12;
    ProfileRetirement profile_retirement = 13;
```

After `message HostDrainAcknowledged { string host_id = 1; }` add:

```proto
// ADR 0018 §3: the host's new preparation, validated like a startup
// publication. `request_id` is a ULID the host chose, echoed in the answer.
message PublishProfiles {
  string request_id = 1;
  ReportInventory inventory = 2;
}
// ADR 0018 §3: accepted replaces the approved snapshot; refused keeps the
// previous one. `reason` is a bounded operator-safe phrase.
message ProfilesPublished {
  string request_id = 1;
  bool accepted = 2;
  string reason = 3;
}
// ADR 0018 §4: ask the server to retire `profile` on this host, stopping the
// deployments that use it when `drain` is set.
message RetireProfile {
  string request_id = 1;
  string profile = 2;
  bool drain = 3;
}
// ADR 0018 §4: `confirmed` (nothing uses the profile any more), `in_use`
// (refused without drain; `deployments` names them), `draining` (stops
// issued; a terminal answer follows), `holding` (stops unsettled at the
// bound; nothing confirmed, accounting kept), `refused` (`reason` says why).
message ProfileRetirement {
  string request_id = 1;
  string outcome = 2;
  repeated string deployments = 3;
  string reason = 4;
}
```

In `SessionReady`, after `int64 heartbeat_lost_after_ms = 4;` add:

```proto
  // ADR 0018 (additive): the server's own post-baseline features by name, so
  // a host knows whether it may send PublishProfiles and RetireProfile. Empty
  // from a server that predates it.
  repeated string capabilities = 5;
```

- [ ] **Step 4: Extend the catalogue.** In `capabilities.rs`, after `INSTALLATION_FINGERPRINT`:

```rust
/// ADR 0018: PublishProfiles / RetireProfile from the host and
/// ProfilesPublished / ProfileRetirement from the server. Server-to-host: the
/// server sends its two messages only to a host that declared it, and a host
/// sends its two only to a server whose SessionReady lists it.
pub const LIVE_PROFILE_UPDATE: &str = "live_profile_update";

/// ADR 0018: bounds on the new messages' strings and lists.
pub const MAX_REQUEST_ID: usize = 64;
pub const MAX_RETIREMENT_DEPLOYMENTS: usize = 256;
pub const MAX_REASON: usize = 512;

/// ADR 0018: what this server advertises in `SessionReady.capabilities`.
pub fn server_capabilities() -> Vec<String> {
    vec![LIVE_PROFILE_UPDATE.to_owned()]
}
```

Append `(LIVE_PROFILE_UPDATE, Direction::ServerToHost),` as the last `CATALOGUE` entry. Do not add it to `PLACEMENT_REQUIRED`: a host without it still takes placements.

- [ ] **Step 5: Update the hard-coded server-to-host list.** In `version_skew.rs:195-196` add `LIVE_PROFILE_UPDATE` to the array:

```rust
        if [HEARTBEATS, MODEL_SOURCES, CHECKPOINT_DIGEST, CHECKPOINT_SIZE_ONLY, STARTUP_BYTES,
            INSTANCE_INDEX, RESTORE_CHECKPOINT_DIGEST, TERMINATE_RECORDED_PROCESSES,
            LIVE_PROFILE_UPDATE].contains(name)
```

- [ ] **Step 6: Keep the one SessionReady constructor compiling.** In `crates/mllm-controller/src/agent_sessions.rs:1075` the struct literal gains `capabilities: Vec::new()` for now (Task 9 fills it); in `crates/mllm-controller/tests/control_heartbeats.rs:390` add `..Default::default()` if the literal is exhaustive.

- [ ] **Step 7: Run the tests.**

Run: `cargo test -p mllm-protocol --all-targets --locked && cargo test -p mllm-controller --test version_skew --test control_heartbeats --locked`
Expected: all pass (the controller's `record.capabilities.len() == CATALOGUE.len()` still holds: the agent declares every catalogue name).

- [ ] **Step 8: Clippy and commit.**

Run: `cargo clippy -p mllm-protocol -p mllm-controller --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-protocol crates/mllm-controller/src/agent_sessions.rs crates/mllm-controller/tests/control_heartbeats.rs
git commit -m "feat(protocol): live_profile_update capability and its messages

ADR 0018: PublishProfiles and RetireProfile from the host,
ProfilesPublished and ProfileRetirement from the server, and
SessionReady.capabilities so a host learns what its server supports.
All additive on new field numbers; gated as a server-to-host capability."
```

---

### Task 7: Store: durable profile retirements and the placement exclusion

**Files:**
- Create: `crates/mllm-store/src/profile_retirement.rs`
- Modify: `crates/mllm-store/src/lib.rs:5-30` (add `pub mod profile_retirement;`)
- Modify: `crates/mllm-store/src/schema.rs` (new `SCHEMA_V35` after `SCHEMA_V34` at `:832-841`)
- Modify: `crates/mllm-store/src/migrations.rs:7-9, 45-46` (import and list `SCHEMA_V35`)
- Modify: `crates/mllm-store/src/ordinary_lifecycle/placement.rs:82-199` (`candidates`: read the profile name, exclude retiring or unpublished profiles)
- Modify: `crates/mllm-controller/src/enrollment.rs:90-96` (`hosts_with_pending_drain` also expires abandoned retirements)
- Test: `crates/mllm-store/tests/instances_placement.rs` (append; reuses its `TwoHosts` harness)

**Interfaces:**
- Consumes: `DrainCandidate` (`host_drain.rs:20`), the tables `runtime_bindings`, `deployment_instances`, `remote_binding_ingress`, `host_effective_revisions.source_json`, `managed_configuration_sources`, `approved_host_publications`, `operations`.
- Produces (used by Tasks 8, 10, 11, 20):
  - `pub struct RetirementCandidate { pub deployment_id: String, pub name: String, pub revision: i64, pub instance: u32 }` and `impl RetirementCandidate { pub fn drain(&self) -> DrainCandidate }`
  - `pub enum RetirementStart { Clear, InUse(Vec<RetirementCandidate>), Draining(Vec<RetirementCandidate>) }`
  - `pub enum RetirementProgress { Waiting(Vec<String>), Settled, Holding(Vec<String>), Expired(Vec<String>), Gone }` (the `Vec<String>` are deployment names still unsettled)
  - `Store::begin_profile_retirement(&self, host: &str, profile: &str, key: &str, now_ms: i64, deadline_ms: i64, drain: bool) -> Result<RetirementStart, StoreError>`
  - `Store::profile_candidates(&self, host: &str, profile: &str) -> Result<Vec<RetirementCandidate>, StoreError>`
  - `Store::record_profile_retirement_stops(&self, host: &str, profile: &str, key: &str, operations: &[String]) -> Result<(), StoreError>`
  - `Store::profile_retirement_progress(&self, host: &str, profile: &str, key: &str, now_ms: i64) -> Result<RetirementProgress, StoreError>`
  - `Store::cancel_profile_retirement(&self, host: &str, profile: &str, key: &str) -> Result<(), StoreError>`
  - `Store::profile_retirement(&self, host: &str, profile: &str) -> Result<Option<(String, String, i64)>, StoreError>` (key, state, deadline)
  - `Store::expire_profile_retirements(&self, now_ms: i64) -> Result<Vec<(String, String)>, StoreError>`
  - `pub(crate) fn profile_placeable(tx: &rusqlite::Transaction<'_>, host: &str, profile: &str) -> Result<bool, rusqlite::Error>`

- [ ] **Step 1: Write the failing tests.** Append to `crates/mllm-store/tests/instances_placement.rs`:

```rust
use mllm_store::profile_retirement::{RetirementProgress, RetirementStart};

/// ADR 0018 §4: from the moment a retirement of (host, profile) commits, no
/// new instance of that profile is placed on that host; cancelling it lets
/// placement resume. Placement elsewhere is unaffected.
// T16 T33
#[test]
fn a_retiring_profile_takes_no_new_placement_on_its_host() {
    let t = two_hosts("32GiB");
    let id = t
        .deploy("deploy", json!({"instances": 1, "placement": {"hosts": ["spark-a", "spark-b"], "strategy": "pack"}}))
        .deployment_id;
    let start = t
        .store
        .begin_profile_retirement("host-a", "local", "retire-1", NOW, DEADLINE, true)
        .unwrap();
    assert!(matches!(start, RetirementStart::Clear), "nothing runs yet");
    t.start(&id, "start", StartScope::All, None);
    let hosts: Vec<String> = t.planned(&id).into_iter().map(|(_, _, h)| h).collect();
    assert_eq!(hosts, vec!["host-b".to_string()], "host-a's profile is retiring");
    t.store.cancel_profile_retirement("host-a", "local", "retire-1").unwrap();
    assert!(t.store.profile_retirement("host-a", "local").unwrap().is_none());
}

/// ADR 0018 §4: the check names only instances of that profile on that host;
/// without drain it is refused and cancelled in the same transaction; with
/// drain it stands until its stops settle on evidence.
// T16 T32
#[test]
fn a_retirement_names_its_instances_and_waits_for_evidence() {
    let t = two_hosts("32GiB");
    let id = t
        .deploy("deploy", json!({"instances": 2, "placement": {"hosts": ["spark-a", "spark-b"], "max_per_host": 1}}))
        .deployment_id;
    all_ready(&t, &id, "start");
    // Without drain: refused, listing the host-a instance only, and cancelled.
    match t.store.begin_profile_retirement("host-a", "local", "k1", NOW, DEADLINE, false).unwrap() {
        RetirementStart::InUse(named) => {
            assert_eq!(named.len(), 1);
            assert_eq!(named[0].deployment_id, id);
            assert_eq!(named[0].name, "deploy");
        }
        other => panic!("{other:?}"),
    }
    assert!(t.store.profile_retirement("host-a", "local").unwrap().is_none());
    // Another profile on the same host is clear and confirmed at once.
    assert!(matches!(
        t.store.begin_profile_retirement("host-a", "other", "k0", NOW, DEADLINE, false).unwrap(),
        RetirementStart::Clear
    ));
    assert_eq!(t.store.profile_retirement("host-a", "other").unwrap().unwrap().1, "confirmed");
    // With drain: the retirement stands until the stop succeeds and the
    // runtime is released; an unsettled stop is Waiting, never confirmed.
    assert!(matches!(
        t.store.begin_profile_retirement("host-a", "local", "k2", NOW, DEADLINE, true).unwrap(),
        RetirementStart::Draining(ref named) if named.len() == 1
    ));
    t.sql.execute("INSERT INTO operations(id,kind,state) VALUES('stop-a','ordinary_cleanup','running')", []).unwrap();
    t.store
        .record_profile_retirement_stops("host-a", "local", "k2", &["stop-a".to_string()])
        .unwrap();
    assert!(matches!(
        t.store.profile_retirement_progress("host-a", "local", "k2", NOW + 1).unwrap(),
        RetirementProgress::Waiting(_)
    ));
    t.sql.execute("UPDATE operations SET state='succeeded' WHERE id='stop-a'", []).unwrap();
    assert!(
        matches!(
            t.store.profile_retirement_progress("host-a", "local", "k2", NOW + 2).unwrap(),
            RetirementProgress::Waiting(_)
        ),
        "the runtime is still held: success of the operation alone is not evidence"
    );
    t.sql
        .execute(
            "UPDATE runtime_bindings SET state='released' WHERE deployment_id=?1 AND instance_index IN
               (SELECT instance_index FROM deployment_instances WHERE deployment_id=?1 AND host_id='host-a')",
            [&id],
        )
        .unwrap();
    assert!(matches!(
        t.store.profile_retirement_progress("host-a", "local", "k2", NOW + 3).unwrap(),
        RetirementProgress::Settled
    ));
    assert_eq!(t.store.profile_retirement("host-a", "local").unwrap().unwrap().1, "confirmed");
}

/// ADR 0018 §4 (owner decision 2026-09-25): a failed stop ends the retirement
/// without confirming it, and so does the deadline; placements resume and
/// nothing is released by the retirement itself.
// T32
#[test]
fn a_failed_stop_or_the_deadline_ends_a_retirement_unconfirmed() {
    let t = two_hosts("32GiB");
    let id = t
        .deploy("deploy", json!({"instances": 1, "placement": {"hosts": ["spark-a"]}}))
        .deployment_id;
    all_ready(&t, &id, "start");
    t.store.begin_profile_retirement("host-a", "local", "k", NOW, NOW + 100, true).unwrap();
    t.sql.execute("INSERT INTO operations(id,kind,state) VALUES('stop-f','ordinary_cleanup','failed')", []).unwrap();
    t.store.record_profile_retirement_stops("host-a", "local", "k", &["stop-f".to_string()]).unwrap();
    assert!(matches!(
        t.store.profile_retirement_progress("host-a", "local", "k", NOW + 1).unwrap(),
        RetirementProgress::Holding(ref names) if names == &vec!["deploy".to_string()]
    ));
    assert!(t.store.profile_retirement("host-a", "local").unwrap().is_none());
    t.store.begin_profile_retirement("host-a", "local", "k3", NOW, NOW + 100, true).unwrap();
    assert!(matches!(
        t.store.profile_retirement_progress("host-a", "local", "k3", NOW + 101).unwrap(),
        RetirementProgress::Expired(_)
    ));
    assert!(t.store.profile_retirement("host-a", "local").unwrap().is_none());
    // Nothing above released the instance's runtime.
    let live: i64 = t.sql.query_row(
        "SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?1 AND state!='released'",
        [&id],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(live, 1);
}
```

(`t.sql` is the harness's second connection, `TwoHosts.sql`. If `StartScope` or `all_ready` are not in scope at the end of the file, they already are at the top.)

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-store --test instances_placement --locked retir`
Expected: FAIL to compile (`mllm_store::profile_retirement` not found).

- [ ] **Step 3: Add schema v35.** In `schema.rs` after `SCHEMA_V34`:

```rust
/// v35 (ADR 0018 §4): profile retirements. A row holds (host, profile) out of
/// placement from the transaction that wrote it; `confirmed` means the server
/// found no instance of the profile left on the host. Its stops are recorded
/// so progress is judged on their evidence. Deleted when cancelled, refused,
/// ended unconfirmed, expired, or when the host's re-publication without the
/// profile is accepted.
pub const SCHEMA_V35: &str = r#"
CREATE TABLE IF NOT EXISTS profile_retirements(
  host_id TEXT NOT NULL CHECK(length(host_id) BETWEEN 1 AND 128),
  profile TEXT NOT NULL CHECK(length(profile) BETWEEN 1 AND 64),
  retire_key TEXT NOT NULL CHECK(length(retire_key) BETWEEN 1 AND 128),
  state TEXT NOT NULL CHECK(state IN ('retiring','confirmed')),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0),
  deadline_ms INTEGER NOT NULL CHECK(deadline_ms>=recorded_at_ms),
  PRIMARY KEY(host_id, profile)
);
CREATE TABLE IF NOT EXISTS profile_retirement_stops(
  host_id TEXT NOT NULL,
  profile TEXT NOT NULL,
  operation_id TEXT NOT NULL REFERENCES operations(id),
  PRIMARY KEY(host_id, profile, operation_id),
  FOREIGN KEY(host_id, profile) REFERENCES profile_retirements(host_id, profile) ON DELETE CASCADE
);
"#;
```

In `migrations.rs` add `SCHEMA_V35` to the import list and append to `MIGRATIONS`:

```rust
    // ADR 0018 §4: durable profile retirements and their stops.
    SCHEMA_V35,
```

- [ ] **Step 4: Implement the store API.** Create `crates/mllm-store/src/profile_retirement.rs`:

```rust
//! ADR 0018 §4: removing a published runtime profile from a host, in two
//! phases, so no placement slips in between the check and the removal. The
//! retirement row is written in the transaction that names the instances
//! still using the profile; placement excludes (host, profile) from then on
//! (`profile_placeable`). Nothing here stops, releases or settles anything:
//! stops go through the ordinary path, and a retirement is confirmed only on
//! their evidence (spec design rule 4).
use crate::host_drain::DrainCandidate;
use crate::{Store, StoreError};
use rusqlite::{params, OptionalExtension, Transaction};

/// One instance of the profile holding a runtime on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetirementCandidate {
    pub deployment_id: String,
    pub name: String,
    pub revision: i64,
    pub instance: u32,
}

impl RetirementCandidate {
    /// The ordinary stop this instance needs (the drain path's shape).
    pub fn drain(&self) -> DrainCandidate {
        DrainCandidate {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            instance: self.instance,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetirementStart {
    /// Nothing uses the profile on the host: confirmed.
    Clear,
    /// Refused without drain; the retirement was cancelled in the same
    /// transaction, so placements on the profile resume.
    InUse(Vec<RetirementCandidate>),
    /// Drain requested: these need stopping; the retirement stands.
    Draining(Vec<RetirementCandidate>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetirementProgress {
    /// Stops still unsettled, or runtimes still held: keep waiting.
    Waiting(Vec<String>),
    /// Every stop succeeded and nothing holds a runtime: confirmed.
    Settled,
    /// A stop ended without success; the retirement ended unconfirmed.
    Holding(Vec<String>),
    /// The deadline passed first; the retirement ended unconfirmed.
    Expired(Vec<String>),
    /// No retirement under this key (cancelled, expired or replaced).
    Gone,
}

/// The most instances one retirement names; more is refused, never half done.
const MAX_CANDIDATES: usize = 4096;

/// The profile name a binding's revision was resolved with on `host`.
const PROFILE_OF: &str = "COALESCE(
    (SELECT json_extract(h.source_json,'$.runtime_profile') FROM host_effective_revisions h
      WHERE h.deployment_id=b.deployment_id AND h.revision=b.revision AND h.host_id=?1 AND h.source_json IS NOT NULL),
    (SELECT json_extract(s.config_json,'$.runtime_profile') FROM managed_configuration_sources s
      WHERE s.deployment_id=b.deployment_id AND s.revision=b.revision))";

fn candidates(tx: &rusqlite::Connection, host: &str, profile: &str) -> Result<Vec<RetirementCandidate>, StoreError> {
    let sql = format!(
        "SELECT DISTINCT d.id, d.name, b.revision, b.instance_index FROM runtime_bindings b
           JOIN deployments d ON d.id=b.deployment_id
          WHERE b.state!='released' AND d.kind='model'
            AND (EXISTS(SELECT 1 FROM deployment_instances i WHERE i.deployment_id=b.deployment_id
                          AND i.instance_index=b.instance_index AND i.host_id=?1)
                 OR EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id AND r.host_id=?1))
            AND {PROFILE_OF}=?2
          ORDER BY d.id, b.instance_index LIMIT ?3"
    );
    let found = tx
        .prepare(&sql)?
        .query_map(params![host, profile, (MAX_CANDIDATES + 1) as i64], |r| {
            Ok(RetirementCandidate { deployment_id: r.get(0)?, name: r.get(1)?, revision: r.get(2)?, instance: r.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if found.len() > MAX_CANDIDATES {
        return Err(StoreError::Conflict);
    }
    Ok(found)
}

/// ADR 0018 §4: whether a new instance of `profile` may be placed on `host`.
/// Not while a retirement of it stands, nor when the host's approved
/// publication exists and no longer carries the profile. A host with no
/// publication row (the embedded host) is judged by retirements alone.
pub(crate) fn profile_placeable(tx: &Transaction<'_>, host: &str, profile: &str) -> Result<bool, rusqlite::Error> {
    let retiring: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM profile_retirements WHERE host_id=?1 AND profile=?2)",
        params![host, profile],
        |r| r.get(0),
    )?;
    if retiring {
        return Ok(false);
    }
    let published: Option<String> = tx
        .query_row("SELECT config_json FROM approved_host_publications WHERE host_id=?1", [host], |r| r.get(0))
        .optional()?;
    Ok(published.is_none_or(|json| {
        serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .is_some_and(|doc| doc["runtime_profiles"].get(profile).is_some())
    }))
}

fn valid(host: &str, profile: &str, key: &str) -> bool {
    !host.is_empty() && host.len() <= 128 && !profile.is_empty() && profile.len() <= 64 && !key.is_empty() && key.len() <= 128
}

impl Store {
    /// ADR 0018 §4, phase one: write the retirement and name the instances
    /// that use the profile on the host, in one transaction. A retried
    /// request under the same key reuses its row; another key while one
    /// stands is refused (`Conflict`).
    pub fn begin_profile_retirement(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        now_ms: i64,
        deadline_ms: i64,
        drain: bool,
    ) -> Result<RetirementStart, StoreError> {
        if !valid(host, profile, key) || now_ms < 0 || deadline_ms < now_ms {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT retire_key FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(other) if other != key => return Err(StoreError::Conflict),
            Some(_) => {}
            None => {
                tx.execute(
                    "INSERT INTO profile_retirements(host_id,profile,retire_key,state,recorded_at_ms,deadline_ms)
                     VALUES(?1,?2,?3,'retiring',?4,?5)",
                    params![host, profile, key, now_ms, deadline_ms],
                )?;
            }
        }
        let named = candidates(&tx, host, profile)?;
        let start = if named.is_empty() {
            tx.execute(
                "UPDATE profile_retirements SET state='confirmed' WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementStart::Clear
        } else if drain {
            RetirementStart::Draining(named)
        } else {
            tx.execute("DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2", params![host, profile])?;
            RetirementStart::InUse(named)
        };
        tx.commit()?;
        Ok(start)
    }

    /// The instances of `profile` holding a runtime on `host` now.
    pub fn profile_candidates(&self, host: &str, profile: &str) -> Result<Vec<RetirementCandidate>, StoreError> {
        candidates(&self.conn, host, profile)
    }

    /// Record the ordinary stops a drained retirement issued.
    pub fn record_profile_retirement_stops(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        operations: &[String],
    ) -> Result<(), StoreError> {
        if operations.is_empty() || !valid(host, profile, key) {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        let ours: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND retire_key=?3)",
            params![host, profile, key],
            |r| r.get(0),
        )?;
        if !ours {
            return Err(StoreError::Conflict);
        }
        for operation in operations {
            tx.execute(
                "INSERT OR IGNORE INTO profile_retirement_stops(host_id,profile,operation_id) VALUES(?1,?2,?3)",
                params![host, profile, operation],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// ADR 0018 §4: confirmed only when every recorded stop succeeded and a
    /// fresh enumeration finds nothing; a stop that ended otherwise, or the
    /// deadline, ends the retirement unconfirmed. Never confirms on a guess.
    pub fn profile_retirement_progress(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        now_ms: i64,
    ) -> Result<RetirementProgress, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT state, deadline_ms FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND retire_key=?3",
                params![host, profile, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((state, deadline)) = row else { return Ok(RetirementProgress::Gone) };
        if state == "confirmed" {
            return Ok(RetirementProgress::Settled);
        }
        let named: Vec<String> = candidates(&tx, host, profile)?.into_iter().map(|c| c.name).collect();
        let unsuccessful: i64 = tx.query_row(
            "SELECT COUNT(*) FROM profile_retirement_stops s JOIN operations o ON o.id=s.operation_id
              WHERE s.host_id=?1 AND s.profile=?2 AND o.state IN ('failed','cancelled')",
            params![host, profile],
            |r| r.get(0),
        )?;
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM profile_retirement_stops s JOIN operations o ON o.id=s.operation_id
              WHERE s.host_id=?1 AND s.profile=?2 AND o.state NOT IN ('succeeded','failed','cancelled')",
            params![host, profile],
            |r| r.get(0),
        )?;
        let progress = if unsuccessful > 0 {
            tx.execute("DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2", params![host, profile])?;
            RetirementProgress::Holding(named)
        } else if open == 0 && named.is_empty() {
            tx.execute(
                "UPDATE profile_retirements SET state='confirmed' WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementProgress::Settled
        } else if now_ms >= deadline {
            tx.execute("DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2", params![host, profile])?;
            RetirementProgress::Expired(named)
        } else {
            RetirementProgress::Waiting(named)
        };
        tx.commit()?;
        Ok(progress)
    }

    /// Cancel a retirement (placements on the profile resume). Idempotent.
    pub fn cancel_profile_retirement(&self, host: &str, profile: &str, key: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND retire_key=?3",
            params![host, profile, key],
        )?;
        Ok(())
    }

    /// The standing retirement of (host, profile): key, state, deadline.
    pub fn profile_retirement(&self, host: &str, profile: &str) -> Result<Option<(String, String, i64)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT retire_key, state, deadline_ms FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// ADR 0018 §4 (owner decision 2026-09-25): a retirement abandoned past its
    /// deadline (a server that stopped mid-removal) ends unconfirmed, so it
    /// cannot hold a profile out of placement for ever. Returns what ended.
    pub fn expire_profile_retirements(&self, now_ms: i64) -> Result<Vec<(String, String)>, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let ended: Vec<(String, String)> = tx
            .prepare("SELECT host_id, profile FROM profile_retirements WHERE deadline_ms<=?1")?
            .query_map([now_ms], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        tx.execute("DELETE FROM profile_retirements WHERE deadline_ms<=?1", [now_ms])?;
        tx.commit()?;
        Ok(ended)
    }
}
```

Note: `ON DELETE CASCADE` needs `PRAGMA foreign_keys=ON`; the store enables it on open (check `Store::open`; if it does not, delete from `profile_retirement_stops` explicitly before each `DELETE FROM profile_retirements` in this file).

- [ ] **Step 5: Exclude retiring and unpublished profiles in placement.** In `ordinary_lifecycle/placement.rs::candidates`, change the host query to also read the profile name:

```rust
    let hosts: Vec<(String, String, Option<String>)> = tx
        .prepare(
            "SELECT h.host_id, h.effective_json,
                    COALESCE(json_extract(h.source_json,'$.runtime_profile'),
                             (SELECT json_extract(s.config_json,'$.runtime_profile') FROM managed_configuration_sources s
                               WHERE s.deployment_id=h.deployment_id AND s.revision=h.revision))
               FROM host_effective_revisions h
              WHERE h.deployment_id=?1 AND h.revision=?2 AND h.outcome='resolved' ORDER BY h.host_id",
        )?
        .query_map(params![deployment_id, revision], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
```

Change the loop header to `for (host, raw, profile) in hosts {`, and the `eligible` field of the pushed `HostCandidate` to:

```rust
            // ADR 0018 §4: nor while the deployment's profile is retiring on
            // this host, or absent from the host's current approved document.
            eligible: eligible.is_none_or(|set| set.contains(&host))
                && match &profile {
                    Some(profile) => crate::profile_retirement::profile_placeable(tx, &host, profile)?,
                    None => true,
                },
```

(The `?` converts `rusqlite::Error` into `LifecycleError` as the other queries in this function do.)

- [ ] **Step 6: Expire abandoned retirements with abandoned drains.** In `crates/mllm-controller/src/enrollment.rs::hosts_with_pending_drain` add, after the `expire_host_drain_intents` call:

```rust
        // ADR 0018 §4: likewise an abandoned profile retirement.
        let _ = owner.store().expire_profile_retirements(mllm_protocol::now_unix_ms());
```

- [ ] **Step 7: Run the tests.**

Run: `cargo test -p mllm-store --all-targets --locked && cargo test -p mllm-controller --test host_eligibility --locked`
Expected: all pass, including the three new tests; existing placement tests are unchanged because their hosts have no publication row and no retirement.

- [ ] **Step 8: Clippy and commit.**

Run: `cargo clippy -p mllm-store -p mllm-controller --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-store crates/mllm-controller/src/enrollment.rs
git commit -m "feat(store): durable profile retirements excluded from placement

ADR 0018 section 4, schema v35: a retirement of (host, profile) is
written in the transaction that names the instances still using it, and
placement excludes the profile on that host from then on. Progress is
judged on the recorded stops' evidence and a fresh enumeration; a failed
stop or the deadline ends it unconfirmed, releasing nothing."
```

---

### Task 8: Store and controller: the live re-publication transaction

**Files:**
- Modify: `crates/mllm-store/src/host_publication.rs` (new `RepublishRefusal`, `Store::republish_host_configuration` after `publish_host_configuration_with_launch_claims`, `:47-102`)
- Modify: `crates/mllm-controller/src/host_publication.rs` (new `pub fn republish` after `publish`, `:36-115`)
- Test: `crates/mllm-store/tests/host_publication.rs` (append)

**Interfaces:**
- Consumes: Task 3 `registration::{only_profiles_differ, removed_profiles, added_profiles, check_profile}`; Task 7 `profile_retirements` table.
- Produces (used by Tasks 9 and 20):
  - `pub enum RepublishRefusal { NotEnrolled, PublicationChanged, Invalid, NotProfilesOnly, NotRetired(String), Store }` with `pub fn reason(&self) -> String` (bounded, operator-safe)
  - `Store::republish_host_configuration(&self, publication: &HostPublication, previous_fingerprint: &str) -> Result<(), RepublishRefusal>`
  - `mllm_controller::host_publication::republish(state: &SharedCoordinatorState, host_id: &str, inventory: &ReportInventory, previous: &ReportInventory) -> Result<(), String>`

- [ ] **Step 1: Write the failing test.** Append to `crates/mllm-store/tests/host_publication.rs`:

```rust
fn enrolled(store: &Store) -> String {
    store.create_host_invitation(&"e".repeat(64), "spark-r", 100, 0).unwrap();
    store
        .redeem_host_invitation(
            &Redemption {
                invitation_digest: "e".repeat(64),
                transaction_id: "tx-r".into(),
                host_name: "spark-r".into(),
                key_digest: "c".repeat(64),
                csr_digest: "d".repeat(64),
            },
            1,
            |host| Ok(CertificateRecord { host_id: host.into(), fingerprint: "f".repeat(64), certificate_pem: "c".into(), expires_unix: 500 }),
        )
        .unwrap()
        .host_id
}

fn publication(host: &str, document: &serde_json::Value) -> HostPublication {
    HostPublication {
        host_id: host.into(),
        config_json: document.to_string(),
        boot_id: "boot-a".into(),
        fingerprint: mllm_config::remote_resources::policy_fingerprint(document),
        received_at_ms: 100,
    }
}

// T07 T33 (ADR 0018 §3, §4): a live re-publication replaces the approved
// document only when it changes runtime profiles alone, only over the
// publication it was based on, and only drops a profile whose retirement the
// server confirmed; a refusal keeps the previous document.
#[test]
fn a_live_republication_changes_profiles_only_and_never_unretired_ones() {
    use mllm_store::host_publication::RepublishRefusal;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("s.sqlite3")).unwrap();
    let host = enrolled(&store);
    let base: serde_json::Value = serde_json::from_str(&mllm_config::remote_roles::HostConfig::template(
        std::path::Path::new("/home/operator/host"),
    ))
    .unwrap();
    store.publish_host_configuration(&publication(&host, &base)).unwrap();
    let profile = serde_json::json!({"engine":"vllm","revision":1,"executable":"/v/bin/vllm","build_fingerprint":"0.29.0",
        "args":[],"env":{},"log_policy":{"max_file_bytes":"16MiB","retained_files":3},
        "security":{"deep_park":"enabled","trust_remote_code":false,"credential_ref":"secret://engine-key","admin_credential_ref":"secret://admin-key"}});
    let mut added = base.clone();
    added["runtime_profiles"]["vllm"] = profile;
    let base_fp = mllm_config::remote_resources::policy_fingerprint(&base);
    store.republish_host_configuration(&publication(&host, &added), &base_fp).unwrap();
    assert_eq!(store.host_publication(&host).unwrap().unwrap().config_json, added.to_string());
    // Based on a stale publication: refused, nothing changes.
    assert_eq!(
        store.republish_host_configuration(&publication(&host, &base), &base_fp),
        Err(RepublishRefusal::PublicationChanged)
    );
    // Anything but profiles: refused.
    let added_fp = mllm_config::remote_resources::policy_fingerprint(&added);
    let mut edited = added.clone();
    edited["load_report_interval"] = "9s".into();
    assert_eq!(
        store.republish_host_configuration(&publication(&host, &edited), &added_fp),
        Err(RepublishRefusal::NotProfilesOnly)
    );
    // Dropping a profile the server did not retire: refused.
    assert_eq!(
        store.republish_host_configuration(&publication(&host, &base), &added_fp),
        Err(RepublishRefusal::NotRetired("vllm".into()))
    );
    // After a confirmed retirement: accepted, and the retirement is gone.
    assert!(matches!(
        store.begin_profile_retirement(&host, "vllm", "k", 1, 10, false).unwrap(),
        mllm_store::profile_retirement::RetirementStart::Clear
    ));
    store.republish_host_configuration(&publication(&host, &base), &added_fp).unwrap();
    assert!(store.profile_retirement(&host, "vllm").unwrap().is_none());
    assert_eq!(store.host_publication(&host).unwrap().unwrap().config_json, base.to_string());
}
```

(Add `use mllm_store::enrollment::{CertificateRecord, Redemption};` only if the existing `use` at the top does not already bring them in; it does.)

- [ ] **Step 2: Run the test to verify it fails.**

Run: `cargo test -p mllm-store --test host_publication --locked a_live_republication`
Expected: FAIL to compile (`republish_host_configuration`, `RepublishRefusal` not found).

- [ ] **Step 3: Implement the store transaction.** In `crates/mllm-store/src/host_publication.rs` add (and `#[derive(PartialEq, Eq)]`-able types):

```rust
/// ADR 0018 §3: why a live re-publication was refused. The previous approved
/// document stays in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepublishRefusal {
    NotEnrolled,
    /// The approved document is no longer the one the host based this on.
    PublicationChanged,
    Invalid,
    /// Something other than `runtime_profiles` changed.
    NotProfilesOnly,
    /// The profile would be dropped without a confirmed retirement.
    NotRetired(String),
    Store,
}

impl RepublishRefusal {
    /// A bounded, operator-safe phrase for `ProfilesPublished.reason`.
    pub fn reason(&self) -> String {
        match self {
            Self::NotEnrolled => "the host is not enrolled or is revoked".into(),
            Self::PublicationChanged => "the approved document changed since this host last published; reconnect and retry".into(),
            Self::Invalid => "the document is not a valid host document".into(),
            Self::NotProfilesOnly => "only runtime profiles change live; restart the host to publish other changes".into(),
            Self::NotRetired(name) => format!("profile {name} is still in use or was not retired; run mllm engine remove"),
            Self::Store => "the server could not record the publication".into(),
        }
    }
}

impl From<rusqlite::Error> for RepublishRefusal {
    fn from(_: rusqlite::Error) -> Self {
        Self::Store
    }
}

impl Store {
    /// ADR 0018 §3, §4: replace the approved document of a connected host
    /// with a re-publication, in one transaction: the host is enrolled and
    /// not revoked; the approved document is still `previous_fingerprint`;
    /// only `runtime_profiles` differ; every dropped profile has a confirmed
    /// retirement, which is deleted here. Launch claims are unchanged.
    pub fn republish_host_configuration(
        &self,
        publication: &HostPublication,
        previous_fingerprint: &str,
    ) -> Result<(), RepublishRefusal> {
        if publication.config_json.len() > 32768 {
            return Err(RepublishRefusal::Invalid);
        }
        let config = mllm_config::remote_roles::HostConfig::parse(&publication.config_json)
            .map_err(|_| RepublishRefusal::Invalid)?;
        if mllm_config::remote_resources::policy_fingerprint(&config.document) != publication.fingerprint {
            return Err(RepublishRefusal::Invalid);
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let enrolled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1 AND revoked=0)",
            [&publication.host_id],
            |r| r.get(0),
        )?;
        if !enrolled {
            return Err(RepublishRefusal::NotEnrolled);
        }
        let current: Option<(String, String)> = tx
            .query_row(
                "SELECT config_json, fingerprint FROM approved_host_publications WHERE host_id=?1",
                [&publication.host_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((current_json, current_fingerprint)) = current else {
            return Err(RepublishRefusal::PublicationChanged);
        };
        if current_fingerprint != previous_fingerprint {
            return Err(RepublishRefusal::PublicationChanged);
        }
        let old: serde_json::Value = serde_json::from_str(&current_json).map_err(|_| RepublishRefusal::Store)?;
        if !mllm_config::registration::only_profiles_differ(&old, &config.document) {
            return Err(RepublishRefusal::NotProfilesOnly);
        }
        for dropped in mllm_config::registration::removed_profiles(&old, &config.document) {
            let confirmed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND state='confirmed')",
                params![publication.host_id, dropped],
                |r| r.get(0),
            )?;
            if !confirmed {
                return Err(RepublishRefusal::NotRetired(dropped));
            }
            tx.execute(
                "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![publication.host_id, dropped],
            )?;
        }
        tx.execute(
            "UPDATE approved_host_publications SET config_json=?2, boot_id=?3, fingerprint=?4, received_at_ms=?5 WHERE host_id=?1",
            params![publication.host_id, config.document.to_string(), publication.boot_id, publication.fingerprint, publication.received_at_ms],
        )?;
        tx.commit()?;
        Ok(())
    }
}
```

- [ ] **Step 4: Implement the controller wrapper.** In `crates/mllm-controller/src/host_publication.rs` add:

```rust
/// ADR 0018 §3: a live re-publication, validated like a startup publication
/// (`HostConfig::parse`, fingerprint), with every added profile checked by
/// the rules resolution applies, then stored only if runtime profiles alone
/// changed and every dropped profile's retirement was confirmed. `Err` is
/// the operator-safe reason; the previous approved document stays.
pub fn republish(
    state: &SharedCoordinatorState,
    host_id: &str,
    inventory: &ReportInventory,
    previous: &ReportInventory,
) -> Result<(), String> {
    let refusal = |r: mllm_store::host_publication::RepublishRefusal| r.reason();
    let config = mllm_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
        .map_err(|e| format!("the document is not a valid host document: {}", e.detail))?;
    if mllm_config::remote_resources::policy_fingerprint(&config.document) != inventory.policy_fingerprint {
        return Err("the document does not match its fingerprint".into());
    }
    let old = mllm_config::remote_roles::HostConfig::parse(&previous.approved_host_config_json)
        .map_err(|_| "the approved document could not be read".to_owned())?;
    for added in mllm_config::registration::added_profiles(&old.document, &config.document) {
        mllm_config::registration::check_profile(&added, &config.document["runtime_profiles"][&added])
            .map_err(|e| format!("profile {added}: {}", e.detail))?;
    }
    let publication = mllm_store::host_publication::HostPublication {
        host_id: host_id.into(),
        config_json: config.document.to_string(),
        boot_id: inventory.host_boot_id.clone(),
        fingerprint: inventory.policy_fingerprint.clone(),
        received_at_ms: mllm_protocol::now_unix_ms(),
    };
    let state = state.lock().map_err(|_| "the server could not record the publication".to_owned())?;
    state
        .store()
        .republish_host_configuration(&publication, &previous.policy_fingerprint)
        .map_err(refusal)
}
```

- [ ] **Step 5: Run the tests.**

Run: `cargo test -p mllm-store --test host_publication --locked && cargo test -p mllm-controller --lib --locked`
Expected: all pass.

- [ ] **Step 6: Clippy and commit.**

Run: `cargo clippy -p mllm-store -p mllm-controller --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-store/src/host_publication.rs crates/mllm-store/tests/host_publication.rs crates/mllm-controller/src/host_publication.rs
git commit -m "feat(store): live re-publication of a host's runtime profiles

ADR 0018 sections 3 and 4: one transaction replaces the approved document
only over the publication the host based it on, only when runtime
profiles alone changed, and only drops a profile whose retirement the
server confirmed. A refusal keeps the previous approved document."
```

---

### Task 9: Controller: accept a live re-publication on the session

**Files:**
- Modify: `crates/mllm-controller/src/agent_sessions.rs` (factor the inventory shape check at `:904-906` into `fn inventory_shape_valid`; new match arm beside the `ReportInventory` arms at `:903-933`; `enum After` at `:1105-1111`; the `After` handling at `:1004-1035`; `SessionReady` at `:1075`)
- Modify: `crates/mllm-controller/src/enrollment.rs:61-63` (new `republish_inventory`)
- Create: `crates/mllm-controller/tests/live_profiles.rs`

**Interfaces:**
- Consumes: Task 6 messages and `capabilities::{LIVE_PROFILE_UPDATE, server_capabilities, MAX_REQUEST_ID, MAX_REASON}`; Task 8 `host_publication::republish`.
- Produces (used by Tasks 10, 13, 15):
  - `EnrollmentAuthority::republish_inventory(&self, host: &str, inventory: &pb::ReportInventory, previous: &pb::ReportInventory) -> Result<(), String>`
  - Server behaviour: `SessionReady.capabilities == server_capabilities()`; `PublishProfiles` from a reconciled host that declared `live_profile_update` is answered by exactly one `ProfilesPublished` with the same `request_id`, and the session continues either way; from any other host it ends the session (`permission_denied`), as any unexpected message does.

- [ ] **Step 1: Write the failing tests.** Create `crates/mllm-controller/tests/live_profiles.rs`. Start it with the module doc below, then copy verbatim from `crates/mllm-controller/tests/version_skew.rs` the `use` block (lines 11-45), and the helpers `now`, `directory`, `prepared_document`, `inventory`, `struct Harness`, `enrolled`, `type Opened`, `Harness::open` and `Harness::reconciled` (lines 46-212). Drop imports the compiler reports unused. Then add:

```rust
//! ADR 0018 §3: live re-publication of a host's runtime profiles over a real
//! mTLS control session. CPU-only transport tests: nothing here qualifies a
//! native engine.

fn all() -> Vec<String> {
    capabilities::agent_capabilities()
}

/// The prepared inventory with one more runtime profile, `vllm`.
fn with_vllm(host: &str) -> pb::ReportInventory {
    let mut inventory = inventory(host);
    let mut document: Value = serde_json::from_str(&inventory.approved_host_config_json).unwrap();
    document["runtime_profiles"]["vllm"] = mllm_config::registration::profile_document(&mllm_config::registration::ProfileSpec {
        engine: mllm_config::engine_policy::Engine::Vllm,
        executable: "/home/operator/venv/bin/vllm".into(),
        build_fingerprint: "0.29.0".into(),
        deep_park: true,
        installation_drift: mllm_config::effective::InstallationDrift::Warn,
        args: vec![],
    });
    inventory.approved_host_config_json = document.to_string();
    inventory.policy_fingerprint = mllm_config::remote_resources::policy_fingerprint(&document);
    inventory.profiles.push(pb::RuntimeProfileStatus {
        name: "vllm".into(),
        build_fingerprint: "0.29.0".into(),
        eligibility: "unknown".into(),
        ..Default::default()
    });
    inventory
}

fn publish(request_id: &str, inventory: pb::ReportInventory) -> pb::AgentToServer {
    pb::AgentToServer {
        msg: Some(agent_to_server::Msg::PublishProfiles(pb::PublishProfiles {
            request_id: request_id.into(),
            inventory: Some(inventory),
        })),
    }
}

async fn verdict(stream: &mut tonic::Streaming<pb::ServerToAgent>) -> pb::ProfilesPublished {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.message()).await.unwrap().unwrap().unwrap().msg {
            Some(server_to_agent::Msg::ProfilesPublished(v)) => return v,
            Some(server_to_agent::Msg::Heartbeat(_)) => continue,
            other => panic!("expected ProfilesPublished, got {other:?}"),
        }
    }
}

// T34: the server tells a host it can take live profile updates.
#[tokio::test]
async fn session_ready_advertises_live_profile_update() {
    let h = enrolled().await;
    let (send, mut stream) = h.open(BINARY_VERSION, all()).await.unwrap();
    send.send(pb::AgentToServer { msg: Some(agent_to_server::Msg::ReportInventory(inventory(&h.host))) }).await.unwrap();
    send.send(pb::AgentToServer { msg: Some(agent_to_server::Msg::ReconcileHistory(pb::ReconcileHistory { records: vec![], complete: true })) }).await.unwrap();
    match stream.message().await.unwrap().unwrap().msg {
        Some(server_to_agent::Msg::SessionReady(ready)) => {
            assert!(ready.capabilities.contains(&capabilities::LIVE_PROFILE_UPDATE.to_owned()));
        }
        other => panic!("{other:?}"),
    }
    h.server.abort();
}

// T07 T34: an accepted re-publication replaces the approved snapshot at once,
// the session view lists the new profile, and the session carries on.
#[tokio::test]
async fn an_accepted_republication_replaces_the_snapshot_live() {
    let h = enrolled().await;
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    let next = with_vllm(&h.host);
    send.send(publish("01K00000000000000000000010", next.clone())).await.unwrap();
    let answer = verdict(&mut stream).await;
    assert_eq!(answer.request_id, "01K00000000000000000000010");
    assert!(answer.accepted, "{}", answer.reason);
    let approved = h.state.lock().unwrap().store().host_publication(&h.host).unwrap().unwrap();
    assert_eq!(approved.fingerprint, next.policy_fingerprint);
    let view = h.sessions.inspect(&h.host).unwrap();
    assert!(view.profiles.iter().any(|p| p.name == "vllm"), "{:?}", view.profiles);
    assert!(view.eligible);
    // The next refresh carries the new document and is accepted.
    send.send(pb::AgentToServer { msg: Some(agent_to_server::Msg::ReportInventory(next)) }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.sessions.current_session(&h.host).is_some());
    h.server.abort();
}

// T03 T07: a refused re-publication keeps the previous snapshot and the
// session, and says why.
#[tokio::test]
async fn a_refused_republication_keeps_the_snapshot_and_the_session() {
    let h = enrolled().await;
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    let before = h.state.lock().unwrap().store().host_publication(&h.host).unwrap().unwrap();
    let mut edited = with_vllm(&h.host);
    let mut document: Value = serde_json::from_str(&edited.approved_host_config_json).unwrap();
    document["load_report_interval"] = json!("9s");
    edited.approved_host_config_json = document.to_string();
    edited.policy_fingerprint = mllm_config::remote_resources::policy_fingerprint(&document);
    send.send(publish("r1", edited)).await.unwrap();
    let answer = verdict(&mut stream).await;
    assert!(!answer.accepted);
    assert!(answer.reason.contains("only runtime profiles"), "{}", answer.reason);
    let after = h.state.lock().unwrap().store().host_publication(&h.host).unwrap().unwrap();
    assert_eq!(after.fingerprint, before.fingerprint);
    assert!(h.sessions.current_session(&h.host).is_some(), "the session stays");
    h.server.abort();
}

// T34: a host that did not declare the capability may not send it.
#[tokio::test]
async fn an_undeclared_republication_ends_the_session() {
    let h = enrolled().await;
    let mut declared = all();
    declared.retain(|c| c != capabilities::LIVE_PROFILE_UPDATE);
    let (send, mut stream) = h.reconciled(BINARY_VERSION, declared).await;
    send.send(publish("r2", with_vllm(&h.host))).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.message().await {
                Err(status) => return status.code(),
                Ok(None) => return tonic::Code::Ok,
                Ok(Some(_)) => continue,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ended, tonic::Code::PermissionDenied);
    h.server.abort();
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-controller --test live_profiles --locked`
Expected: `session_ready_advertises_live_profile_update` fails (empty capabilities) and the publication tests fail (session ended, no verdict).

- [ ] **Step 3: Add `republish_inventory`.** In `enrollment.rs`, beside `publish_inventory`:

```rust
    /// ADR 0018 §3: a live re-publication from a reconciled session. `Err`
    /// is the operator-safe reason; the previous approved document stays.
    pub fn republish_inventory(
        &self,
        host: &str,
        inventory: &mllm_protocol::pb::ReportInventory,
        previous: &mllm_protocol::pb::ReportInventory,
    ) -> Result<(), String> {
        crate::host_publication::republish(&self.state, host, inventory, previous)
    }
```

- [ ] **Step 4: Handle `PublishProfiles` in the session.** In `agent_sessions.rs`:

(a) Factor the startup shape check into a function next to `same_registration` (`:209`), and call it from the first `ReportInventory` arm in place of the two inline `if` conditions:

```rust
/// SPEC §§4.2, 13: the bounds every published inventory meets (startup and
/// ADR 0018 live re-publication alike).
fn inventory_shape_valid(inventory: &pb::ReportInventory, host: &str) -> bool {
    inventory.domains.len() <= 128
        && inventory.profiles.len() <= 128
        && inventory
            .envelope
            .as_ref()
            .is_some_and(|e| e.host_id == host && mllm_protocol::compatible_peer(&e.protocol_version))
        && inventory.domains.iter().all(|d| {
            bounded_name(&d.domain_id)
                && matches!(d.kind.as_str(), "system" | "device_memory" | "filesystem" | "remote_storage")
                && d.observed_bytes >= -1
        })
        && inventory.profiles.iter().all(|p| {
            bounded_name(&p.name)
                && bounded_name(&p.build_fingerprint)
                && matches!(p.eligibility.as_str(), "unknown" | "qualified" | "unsupported" | "disabled")
                && installation_fields_valid(p)
        })
}
```

(b) Add `Republish(String, Box<pb::ReportInventory>, Box<pb::ReportInventory>)` to `enum After`.

(c) Add a match arm after the reconciled `ReportInventory` arm:

```rust
                                // ADR 0018 §3: a live re-publication, only from a host that
                                // declared `live_profile_update` (ADR 0017). Answered below;
                                // the session carries on whatever the verdict.
                                Some(agent_to_server::Msg::PublishProfiles(request))
                                    if s.view.reconciled && s.capabilities.contains(capabilities::LIVE_PROFILE_UPDATE) =>
                                {
                                    let inventory = request.inventory.ok_or_else(denied)?;
                                    let previous = s.inventory.clone().ok_or_else(denied)?;
                                    if request.request_id.is_empty()
                                        || request.request_id.len() > capabilities::MAX_REQUEST_ID
                                        || !inventory_shape_valid(&inventory, &host)
                                        || inventory.host_boot_id != previous.host_boot_id
                                    {
                                        return Err(denied());
                                    }
                                    after = After::Republish(request.request_id, Box::new(inventory), Box::new(previous));
                                    false
                                }
```

(`capabilities` is `mllm_protocol::capabilities`; import it at the top if the file names it differently.)

(d) Handle it after the lock, beside `After::Refresh`:

```rust
                            After::Republish(request_id, inventory, previous) => {
                                // Store work outside the session table (SPEC §13).
                                let verdict = self.authority.republish_inventory(&host, &inventory, &previous);
                                if verdict.is_ok() {
                                    let mut sessions = self.sessions.lock().map_err(|_| denied())?;
                                    let s = sessions.get_mut(&host).ok_or_else(denied)?;
                                    if s.view.session_id != id { return Err(denied()); }
                                    s.view.profiles = inventory.profiles.iter().map(ProfileView::of).collect();
                                    s.prepared = crate::host_publication::eligible(&inventory);
                                    s.view.eligible = s.prepared && s.placeable;
                                    s.inventory = Some(*inventory);
                                }
                                let (accepted, reason) = match verdict {
                                    Ok(()) => (true, String::new()),
                                    Err(mut reason) => {
                                        reason.truncate(capabilities::MAX_REASON);
                                        (false, reason)
                                    }
                                };
                                if accepted {
                                    // Placement reads the new snapshot from here on.
                                    self.changed();
                                }
                                send_reply(&outgoing, pb::ServerToAgent { msg: Some(server_to_agent::Msg::ProfilesPublished(pb::ProfilesPublished { request_id, accepted, reason })) }).await.map_err(|status| *status)?;
                            }
```

(`String::truncate` panics off a char boundary; the reasons are ASCII, but use `reason.chars().take(MAX_REASON).collect()` if Clippy or a reviewer prefers.)

(e) At `:1075`, fill the new field: `capabilities: capabilities::server_capabilities(),`.

- [ ] **Step 5: Run the tests.**

Run: `cargo test -p mllm-controller --test live_profiles --test agent_sessions --test version_skew --test host_eligibility --locked`
Expected: all pass.

- [ ] **Step 6: Clippy and commit.**

Run: `cargo clippy -p mllm-controller --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-controller
git commit -m "feat(controller): accept live re-publication of runtime profiles

ADR 0018 section 3: a reconciled host that declared live_profile_update
may re-publish its preparation; the server validates it like a startup
publication, swaps the approved snapshot and the session view when it
accepts, keeps both when it refuses, and answers either way without
ending the session. SessionReady now lists the server's capabilities."
```

---

### Task 10: Controller: profile retirement over the session

**Files:**
- Create: `crates/mllm-controller/src/profile_retirement.rs`
- Modify: `crates/mllm-controller/src/lib.rs` (add `pub mod profile_retirement;` beside `pub mod host_publication;`, line 16)
- Modify: `crates/mllm-controller/src/agent_sessions.rs` (struct field beside `drain_hook` at `:358`, its initializer at `:393`, a setter beside `on_host_draining` at `:443`, a match arm and an `After::Retire` handler)
- Test: `crates/mllm-controller/tests/live_profiles.rs` (append)

**Interfaces:**
- Consumes: Task 6 `pb::{RetireProfile, ProfileRetirement}`; `registration::valid_profile_name`.
- Produces (used by Tasks 11, 15, 20):
  - `pub enum RetirementStep { Confirmed, InUse(Vec<String>), Draining(Vec<String>), Holding(Vec<String>), Refused(String) }` with `pub fn outcome(&self) -> &'static str` and `pub fn deployments(&self) -> &[String]`
  - `pub trait ProfileRetirements: Send + Sync { fn begin(&self, host: &str, profile: &str, key: &str, drain: bool) -> RetirementStep; fn poll(&self, host: &str, profile: &str, key: &str) -> Option<RetirementStep>; }` (`poll` is `None` while still waiting)
  - `pub const RETIREMENT_POLL: Duration = Duration::from_secs(1)`
  - `AgentSessions::with_profile_retirements(&self, service: Arc<dyn ProfileRetirements>)`
  - Server behaviour: `RetireProfile` from a capable reconciled host is answered with one `ProfileRetirement`; when that is `draining`, a terminal one (`confirmed` or `holding`) follows on the same session. The retirement key is `"<host>:<request_id>"`, so a host that retries after a reconnect with the same `request_id` resumes the same retirement.

- [ ] **Step 1: Write the failing tests.** Append to `crates/mllm-controller/tests/live_profiles.rs`:

```rust
use mllm_controller::profile_retirement::{ProfileRetirements, RetirementStep};

/// A scripted retirement service: `begin` answers `first`; `poll` answers
/// `None` `waits` times, then `last`.
struct Scripted {
    first: RetirementStep,
    waits: std::sync::atomic::AtomicUsize,
    last: RetirementStep,
    seen: Mutex<Vec<(String, String, String, bool)>>,
}

impl ProfileRetirements for Scripted {
    fn begin(&self, host: &str, profile: &str, key: &str, drain: bool) -> RetirementStep {
        self.seen.lock().unwrap().push((host.into(), profile.into(), key.into(), drain));
        self.first.clone()
    }
    fn poll(&self, _: &str, _: &str, _: &str) -> Option<RetirementStep> {
        if self.waits.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.waits.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            None
        } else {
            Some(self.last.clone())
        }
    }
}

fn retire(request_id: &str, profile: &str, drain: bool) -> pb::AgentToServer {
    pb::AgentToServer {
        msg: Some(agent_to_server::Msg::RetireProfile(pb::RetireProfile {
            request_id: request_id.into(),
            profile: profile.into(),
            drain,
        })),
    }
}

async fn retirement(stream: &mut tonic::Streaming<pb::ServerToAgent>) -> pb::ProfileRetirement {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.message()).await.unwrap().unwrap().unwrap().msg {
            Some(server_to_agent::Msg::ProfileRetirement(r)) => return r,
            Some(server_to_agent::Msg::Heartbeat(_)) => continue,
            other => panic!("expected ProfileRetirement, got {other:?}"),
        }
    }
}

// T16 T32: in use without drain is refused with the list; with drain the
// host hears `draining`, then `confirmed` only when the service confirms.
#[tokio::test]
async fn a_retirement_answers_in_use_or_drains_then_confirms() {
    let h = enrolled().await;
    let service = Arc::new(Scripted {
        first: RetirementStep::InUse(vec!["q14".into()]),
        waits: 0.into(),
        last: RetirementStep::Confirmed,
        seen: Mutex::new(vec![]),
    });
    h.sessions.with_profile_retirements(service.clone());
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    send.send(retire("req-1", "local", false)).await.unwrap();
    let answer = retirement(&mut stream).await;
    assert_eq!((answer.request_id.as_str(), answer.outcome.as_str()), ("req-1", "in_use"));
    assert_eq!(answer.deployments, vec!["q14".to_string()]);
    assert_eq!(service.seen.lock().unwrap()[0], (h.host.clone(), "local".into(), format!("{}:req-1", h.host), false));

    let draining = Arc::new(Scripted {
        first: RetirementStep::Draining(vec!["q14".into()]),
        waits: 2.into(),
        last: RetirementStep::Confirmed,
        seen: Mutex::new(vec![]),
    });
    h.sessions.with_profile_retirements(draining);
    send.send(retire("req-2", "local", true)).await.unwrap();
    assert_eq!(retirement(&mut stream).await.outcome, "draining");
    let last = retirement(&mut stream).await;
    assert_eq!((last.request_id.as_str(), last.outcome.as_str()), ("req-2", "confirmed"));
    h.server.abort();
}

// T32: an unsettled drain answers `holding`, naming what is unsettled.
#[tokio::test]
async fn an_unsettled_drain_answers_holding() {
    let h = enrolled().await;
    h.sessions.with_profile_retirements(Arc::new(Scripted {
        first: RetirementStep::Draining(vec!["q14".into()]),
        waits: 0.into(),
        last: RetirementStep::Holding(vec!["q14".into()]),
        seen: Mutex::new(vec![]),
    }));
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    send.send(retire("req-3", "local", true)).await.unwrap();
    assert_eq!(retirement(&mut stream).await.outcome, "draining");
    let last = retirement(&mut stream).await;
    assert_eq!(last.outcome, "holding");
    assert_eq!(last.deployments, vec!["q14".to_string()]);
    h.server.abort();
}

// T37: an invalid profile name, or a server with no retirement service, is
// refused without ending the session.
#[tokio::test]
async fn a_malformed_or_unserved_retirement_is_refused() {
    let h = enrolled().await;
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    send.send(retire("req-4", "local", false)).await.unwrap();
    let answer = retirement(&mut stream).await;
    assert_eq!(answer.outcome, "refused");
    assert!(!answer.reason.is_empty());
    send.send(retire("req-5", "Not A Name", false)).await.unwrap();
    assert_eq!(retirement(&mut stream).await.outcome, "refused");
    assert!(h.sessions.current_session(&h.host).is_some());
    h.server.abort();
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-controller --test live_profiles --locked retirement`
Expected: FAIL to compile (`mllm_controller::profile_retirement` not found).

- [ ] **Step 3: Implement the service contract.** Create `crates/mllm-controller/src/profile_retirement.rs`:

```rust
//! ADR 0018 §4: the server side of removing a published runtime profile. The
//! session layer only relays; the service (mllm-management's
//! `StoreRetirements`) writes the durable retirement, issues ordinary stops,
//! and confirms on their evidence alone.
use std::time::Duration;

/// How often a draining retirement's progress is read.
pub const RETIREMENT_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetirementStep {
    /// No deployment on the host uses the profile; confirmed.
    Confirmed,
    /// Refused without drain; the named deployments use it.
    InUse(Vec<String>),
    /// Stops issued for the named deployments; a terminal step follows.
    Draining(Vec<String>),
    /// Stops unsettled or failed at the bound; nothing confirmed.
    Holding(Vec<String>),
    /// Could not be started; the phrase says why.
    Refused(String),
}

impl RetirementStep {
    /// `ProfileRetirement.outcome`.
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::InUse(_) => "in_use",
            Self::Draining(_) => "draining",
            Self::Holding(_) => "holding",
            Self::Refused(_) => "refused",
        }
    }
    pub fn deployments(&self) -> &[String] {
        match self {
            Self::InUse(d) | Self::Draining(d) | Self::Holding(d) => d,
            Self::Confirmed | Self::Refused(_) => &[],
        }
    }
    /// The wire message for `request_id`, bounded (ADR 0018 §4).
    pub fn to_wire(&self, request_id: &str) -> mllm_protocol::pb::ProfileRetirement {
        use mllm_protocol::capabilities::{MAX_REASON, MAX_RETIREMENT_DEPLOYMENTS};
        mllm_protocol::pb::ProfileRetirement {
            request_id: request_id.into(),
            outcome: self.outcome().into(),
            deployments: self.deployments().iter().take(MAX_RETIREMENT_DEPLOYMENTS).cloned().collect(),
            reason: match self {
                Self::Refused(reason) => reason.chars().take(MAX_REASON).collect(),
                _ => String::new(),
            },
        }
    }
}

/// ADR 0018 §4. Both calls are blocking store work; the session runs them
/// off its task.
pub trait ProfileRetirements: Send + Sync {
    /// Phase one: write the retirement and check references; with `drain`,
    /// issue the ordinary stops. Idempotent per `key`.
    fn begin(&self, host: &str, profile: &str, key: &str, drain: bool) -> RetirementStep;
    /// A draining retirement's progress: `None` while stops are unsettled,
    /// otherwise the terminal step (`Confirmed` or `Holding`).
    fn poll(&self, host: &str, profile: &str, key: &str) -> Option<RetirementStep>;
}
```

- [ ] **Step 4: Relay it in the session.** In `agent_sessions.rs`:

(a) Field in `AgentSessions` beside `drain_hook`: `retirements: Arc<Mutex<Option<Arc<dyn crate::profile_retirement::ProfileRetirements>>>>,`, initialized `Arc::new(Mutex::new(None))` beside `drain_hook` in `new`.

(b) Setter beside `on_host_draining`:

```rust
    /// ADR 0018 §4: the service that retires runtime profiles. Without one,
    /// every retirement is refused (`refused`), and nothing is removed.
    pub fn with_profile_retirements(&self, service: Arc<dyn crate::profile_retirement::ProfileRetirements>) {
        if let Ok(mut installed) = self.retirements.lock() {
            *installed = Some(service);
        }
    }
```

(c) `enum After` gains `Retire(pb::RetireProfile)`. Match arm after the `PublishProfiles` arm:

```rust
                                // ADR 0018 §4: first phase of removing a published profile.
                                Some(agent_to_server::Msg::RetireProfile(request))
                                    if s.view.reconciled && s.capabilities.contains(capabilities::LIVE_PROFILE_UPDATE) =>
                                {
                                    if request.request_id.is_empty() || request.request_id.len() > capabilities::MAX_REQUEST_ID {
                                        return Err(denied());
                                    }
                                    after = After::Retire(request);
                                    false
                                }
```

(d) The handler:

```rust
                            After::Retire(request) => {
                                use crate::profile_retirement::{RetirementStep, RETIREMENT_POLL};
                                let service = self.retirements.lock().ok().and_then(|s| s.clone());
                                let step = match service.clone() {
                                    None => RetirementStep::Refused("this server cannot retire runtime profiles".into()),
                                    Some(_) if !mllm_config::registration::valid_profile_name(&request.profile) => {
                                        RetirementStep::Refused("not a valid profile name".into())
                                    }
                                    Some(service) => {
                                        let (named, profile, key, drain) =
                                            (host.clone(), request.profile.clone(), format!("{host}:{}", request.request_id), request.drain);
                                        tokio::task::spawn_blocking(move || service.begin(&named, &profile, &key, drain))
                                            .await
                                            .map_err(|_| Status::internal("profile retirement failed"))?
                                    }
                                };
                                if !matches!(step, RetirementStep::Refused(_)) {
                                    // Placement reads the retirement from here on.
                                    self.changed();
                                }
                                send_reply(&outgoing, pb::ServerToAgent { msg: Some(server_to_agent::Msg::ProfileRetirement(step.to_wire(&request.request_id))) }).await.map_err(|status| *status)?;
                                if let (RetirementStep::Draining(_), Some(service)) = (&step, service) {
                                    // Spec design rule 4: the terminal answer waits for stop
                                    // evidence, off this session's loop. A session that ends
                                    // stops the relay only; the retirement itself stands until
                                    // it settles, is retried with the same request id, or expires.
                                    let (outgoing, named, profile, key, request_id) = (
                                        outgoing.clone(), host.clone(), request.profile.clone(),
                                        format!("{host}:{}", request.request_id), request.request_id.clone(),
                                    );
                                    let changed = self.clone_change_notifier();
                                    tokio::spawn(async move {
                                        loop {
                                            tokio::time::sleep(RETIREMENT_POLL).await;
                                            let (service, named, profile, key) = (service.clone(), named.clone(), profile.clone(), key.clone());
                                            let Ok(polled) = tokio::task::spawn_blocking(move || service.poll(&named, &profile, &key)).await else { return };
                                            if let Some(terminal) = polled {
                                                changed();
                                                let _ = send_reply(&outgoing, pb::ServerToAgent { msg: Some(server_to_agent::Msg::ProfileRetirement(terminal.to_wire(&request_id))) }).await;
                                                return;
                                            }
                                            if outgoing.is_closed() {
                                                return;
                                            }
                                        }
                                    });
                                }
                            }
```

Add the helper beside `fn changed` (`:463`); `changes` is the `Arc<watch::Sender<u64>>` field at `:351`:

```rust
    /// `changed()` for a task that outlives this borrow (ADR 0018 §4 relay).
    fn clone_change_notifier(&self) -> impl Fn() + Send + 'static {
        let changes = self.changes.clone();
        move || changes.send_modify(|generation| *generation = generation.wrapping_add(1))
    }
```

- [ ] **Step 5: Run the tests.**

Run: `cargo test -p mllm-controller --test live_profiles --locked`
Expected: all 7 tests pass.

- [ ] **Step 6: Clippy and commit.**

Run: `cargo clippy -p mllm-controller --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-controller
git commit -m "feat(controller): relay profile retirement over the host session

ADR 0018 section 4: a capable host asks the server to retire a profile;
the session answers in_use, confirmed, refused, or draining followed by
confirmed or holding once the retirement service judges the stops'
evidence. The retirement key is stable per request id, so a host that
reconnects resumes the same retirement."
```

---

### Task 11: Management: the retirement service over the ordinary stop path, and `GET /management/v1/engines`

**Files:**
- Create: `crates/mllm-management/src/engines.rs` (`StoreRetirements`)
- Modify: `crates/mllm-management/src/lib.rs:20-30` (add `pub mod engines;`)
- Modify: `crates/mllm-management/src/drain.rs:299-356` (make `Rounds`, `drain_rounds`, `Enumerate`, `StopOne`, `Mark` `pub(crate)`)
- Modify: `crates/mllm-management/src/hosts.rs:19-34` (route `/management/v1/engines`) and a new `engines` handler after `hosts` (`:55-117`)
- Create: `crates/mllm-management/tests/engines.rs`

**Interfaces:**
- Consumes: Task 7 store API; Task 10 `ProfileRetirements`, `RetirementStep`; `OwnedActionSource::{commands, drain_stop}` (`actions.rs:152,161`); `drain::drain_rounds`.
- Produces (used by Tasks 15, 18, 20):
  - `pub struct StoreRetirements` with `pub fn new(source: Arc<OwnedActionSource>) -> Self` and `pub fn with_window(self, window: Duration) -> Self`; `impl ProfileRetirements for StoreRetirements`
  - `pub const RETIREMENT_WINDOW: Duration = Duration::from_secs(900)`
  - `GET /management/v1/engines` → `{"api_version":"1","engines":[{"host_id","host","online","profile","engine","version","custom","executable","fingerprint":{"version","digest","state"},"deep_park","deep_park_probe","published":"published","retiring","deployments":[..]}]}`

- [ ] **Step 1: Write the failing tests.** Create `crates/mllm-management/tests/engines.rs`. Copy verbatim from `crates/mllm-management/tests/drain.rs` its `use` block (lines 6-33), `MANAGEMENT`, `INFERENCE`, `Presence`, `Setup`, `setup`, `body`, `start`, `state_of`, `settled` and `place_on_lab` (lines 35-200); drop what is unused. Then add:

```rust
//! ADR 0018 §4: retiring a runtime profile through the ordinary stop path,
//! and `GET /management/v1/engines`. Fake-engine tests; not qualification.
use mllm_controller::profile_retirement::{ProfileRetirements, RetirementStep};
use mllm_management::engines::StoreRetirements;

fn retirements(setup: &Setup) -> StoreRetirements {
    let host: Value = serde_json::from_str(include_str!("../../mllm-config/tests/fixtures/effective-vllm-golden.json")).unwrap();
    let configuration = Arc::new(SharedConfigurationSource::new(setup.owner.clone(), host["input"]["host"].clone(), "owner").unwrap());
    StoreRetirements::new(Arc::new(OwnedActionSource::new(configuration, setup.worker.commands()).unwrap()))
}

// T16 T32: in use without drain names the deployment and leaves it running;
// with drain the ordinary stop runs and the retirement confirms only after
// the stop succeeded on evidence and nothing holds a runtime.
#[tokio::test]
async fn a_drained_retirement_confirms_only_after_the_stop_settles() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    let service = Arc::new(retirements(&setup));
    let s = service.clone();
    let first = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-1", false)).await.unwrap();
    let RetirementStep::InUse(named) = first else { panic!("{first:?}") };
    assert_eq!(named.len(), 1);
    assert!(setup.owner.lock().unwrap().store().profile_retirement("lab", "local").unwrap().is_none());
    let s = service.clone();
    let draining = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-2", true)).await.unwrap();
    assert!(matches!(draining, RetirementStep::Draining(_)), "{draining:?}");
    let confirmed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let s = service.clone();
            if let Some(step) = tokio::task::spawn_blocking(move || s.poll("lab", "local", "lab:req-2")).await.unwrap() {
                return step;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(confirmed, RetirementStep::Confirmed);
    let store = setup.owner.lock().unwrap();
    assert_eq!(store.store().profile_retirement("lab", "local").unwrap().unwrap().1, "confirmed");
    assert!(store.store().runtime_binding(&id).unwrap().is_none_or(|b| b.state == "released"));
}

// T07: the engines listing shows each host's published profiles with the
// derived custom mark and the deployments using each.
#[tokio::test]
async fn the_engines_listing_shows_published_profiles() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    let mut document: Value = serde_json::from_str(include_str!("../../mllm-config/tests/fixtures/effective-vllm-golden.json")).unwrap();
    let mut host = document["input"]["host"].take();
    host["name"] = json!("lab");
    host["state_dir"] = json!("/home/operator/.local/state/mllm");
    host["identity_dir"] = json!("/home/operator/.local/state/mllm/identity");
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("0.29.0+patched");
    {
        let owner = setup.owner.lock().unwrap();
        owner.store().publish_host_configuration(&mllm_store::host_publication::HostPublication {
            host_id: "lab".into(),
            config_json: host.to_string(),
            boot_id: "boot".into(),
            fingerprint: mllm_config::remote_resources::policy_fingerprint(&host),
            received_at_ms: 1,
        }).unwrap();
    }
    let authority = Arc::new(mllm_controller::enrollment::EnrollmentAuthority::new(
        setup.owner.clone(),
        mllm_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    let router = mllm_management::hosts::hosts_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        setup.owner.clone(),
        mllm_controller::agent_sessions::AgentSessions::new(authority),
    );
    let request = Request::builder()
        .uri("/management/v1/engines")
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .body(Body::empty())
        .unwrap();
    let (status, listing) = body(router.oneshot(request).await.unwrap()).await;
    assert_eq!(status, 200, "{listing}");
    let row = &listing["engines"][0];
    assert_eq!(row["host"], "lab");
    assert_eq!(row["profile"], "local");
    assert_eq!(row["engine"], "vllm");
    assert_eq!(row["version"], "0.29.0+patched");
    assert_eq!(row["custom"], true);
    assert_eq!(row["published"], "published");
    assert_eq!(row["online"], false);
    assert_eq!(row["deployments"].as_array().unwrap().len(), 1);
}
```

(Add `mllm-agent` and `mllm-config` to `[dev-dependencies]` of `crates/mllm-management/Cargo.toml` if they are not already there; both are workspace crates, so `Cargo.lock` gains no package.)

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-management --test engines --locked`
Expected: FAIL to compile (`mllm_management::engines` not found).

- [ ] **Step 3: Share the drain rounds.** In `drain.rs` change `struct Rounds`, its fields, `type Enumerate`, `type StopOne`, `type Mark` and `fn drain_rounds` from private to `pub(crate)`.

- [ ] **Step 4: Implement the service.** Create `crates/mllm-management/src/engines.rs`:

```rust
//! ADR 0018 §4: retiring a runtime profile on one host. The durable
//! retirement is written with the reference check (`Store::
//! begin_profile_retirement`); a drained retirement stops each instance
//! through the ordinary stop path (`OwnedActionSource::drain_stop`: drain up
//! to `switching.drain_timeout`, then terminate, gone evidence required) and
//! confirms only on that evidence. Nothing here releases accounting.
use crate::actions::OwnedActionSource;
use mllm_controller::profile_retirement::{ProfileRetirements, RetirementStep};
use mllm_store::profile_retirement::{RetirementProgress, RetirementStart};
use std::sync::Arc;
use std::time::Duration;

/// ADR 0018 §4 (owner decision 2026-09-25): the same bound as `mllm drain host`.
pub const RETIREMENT_WINDOW: Duration = Duration::from_secs(900);

pub struct StoreRetirements {
    source: Arc<OwnedActionSource>,
    window: Duration,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

impl StoreRetirements {
    pub fn new(source: Arc<OwnedActionSource>) -> Self {
        Self { source, window: RETIREMENT_WINDOW }
    }
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }
}

impl ProfileRetirements for StoreRetirements {
    fn begin(&self, host: &str, profile: &str, key: &str, drain: bool) -> RetirementStep {
        let commands = self.source.commands();
        let now = now_ms();
        let deadline = now + self.window.as_millis() as i64;
        let started = match commands.read(|store| store.begin_profile_retirement(host, profile, key, now, deadline, drain)) {
            Ok(started) => started,
            Err(_) => {
                return RetirementStep::Refused(
                    "another removal of this profile is in progress, or the server's store is unavailable".into(),
                )
            }
        };
        let named = match started {
            RetirementStart::Clear => return RetirementStep::Confirmed,
            RetirementStart::InUse(named) => {
                return RetirementStep::InUse(named.into_iter().map(|c| c.name).collect())
            }
            RetirementStart::Draining(named) => named,
        };
        let names: Vec<String> = named.iter().map(|c| c.name.clone()).collect();
        let mut first = Some(named.iter().map(|c| c.drain()).collect::<Vec<_>>());
        let rounds = crate::drain::drain_rounds(
            &mut || match first.take() {
                Some(named) => Ok(named),
                None => commands
                    .read(|store| store.profile_candidates(host, profile))
                    .map(|found| found.iter().map(|c| c.drain()).collect())
                    .map_err(|_| mllm_controller_failure()),
            },
            // One key per instance under the retirement's key: a retried
            // retirement replays each stop's receipt instead of issuing another.
            &mut |candidate| {
                self.source.drain_stop(
                    &candidate.deployment_id,
                    candidate.instance,
                    candidate.revision,
                    &format!("retire:{key}:{}:{}", candidate.deployment_id, candidate.instance),
                    deadline,
                )
            },
            &mut |issued| {
                commands
                    .read(|store| store.record_profile_retirement_stops(host, profile, key, issued))
                    .map_err(|_| mllm_controller_failure())
            },
        );
        match rounds {
            Ok(rounds) if rounds.issued.is_empty() && !rounds.refused.is_empty() => {
                // Nothing could be stopped: end the retirement unconfirmed.
                let _ = commands.read(|store| store.cancel_profile_retirement(host, profile, key));
                RetirementStep::Holding(names)
            }
            Ok(_) => RetirementStep::Draining(names),
            Err(_) => {
                let _ = commands.read(|store| store.cancel_profile_retirement(host, profile, key));
                RetirementStep::Refused("the stops could not be issued; nothing was removed".into())
            }
        }
    }

    fn poll(&self, host: &str, profile: &str, key: &str) -> Option<RetirementStep> {
        let progress = self
            .source
            .commands()
            .read(|store| store.profile_retirement_progress(host, profile, key, now_ms()));
        match progress {
            Ok(RetirementProgress::Waiting(_)) => None,
            Ok(RetirementProgress::Settled) => Some(RetirementStep::Confirmed),
            Ok(RetirementProgress::Holding(names)) | Ok(RetirementProgress::Expired(names)) => {
                Some(RetirementStep::Holding(names))
            }
            Ok(RetirementProgress::Gone) => {
                Some(RetirementStep::Refused("the retirement ended before it was confirmed".into()))
            }
            // A store that cannot be read confirms nothing; try again.
            Err(_) => None,
        }
    }
}

fn mllm_controller_failure() -> crate::configuration::ConfigurationFailure {
    crate::configuration::ConfigurationFailure::ReconciliationRequired
}
```

- [ ] **Step 5: Add the listing.** In `hosts.rs`, register `.route("/management/v1/engines", get(engines))` next to `/management/v1/hosts`, and add:

```rust
/// ADR 0018: every host's published runtime profiles, from the approved
/// snapshots and the hosts' reported inventories. `custom` is derived from
/// the version (outside the verified set); `deployments` are the instances of
/// the profile holding a runtime on the host now.
async fn engines(State(state): State<Arc<HostState>>) -> Response {
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let owner = state.owner.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let owner = owner.lock().ok()?;
        let store = owner.store();
        let mut rows = Vec::new();
        for host in store.enrolled_hosts().ok()? {
            let Some(publication) = store.host_publication(&host.host_id).ok().flatten() else { continue };
            let Ok(document) = serde_json::from_str::<serde_json::Value>(&publication.config_json) else { continue };
            for (name, profile) in document["runtime_profiles"].as_object().cloned().unwrap_or_default() {
                let deployments: Vec<String> = store
                    .profile_candidates(&host.host_id, &name)
                    .map(|found| found.into_iter().map(|c| c.name).collect())
                    .unwrap_or_default();
                let retiring = store.profile_retirement(&host.host_id, &name).ok().flatten().is_some();
                rows.push((host.clone(), name, profile, deployments, retiring));
            }
        }
        Some(rows)
    })
    .await;
    let Ok(Some(rows)) = rows else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "snapshot_unavailable", true);
    };
    let engines: Vec<_> = rows
        .into_iter()
        .map(|(host, name, profile, deployments, retiring)| {
            let session = state.sessions.inspect(&host.host_id);
            let reported = session.as_ref().and_then(|s| s.profiles.iter().find(|p| p.name == name).cloned());
            let engine = profile["engine"].as_str().unwrap_or("unknown").to_owned();
            let version = reported
                .as_ref()
                .map(|p| p.installation.version.clone())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| profile["build_fingerprint"].as_str().unwrap_or("unknown").to_owned());
            let custom = match engine.as_str() {
                "vllm" => !mllm_config::registration::is_verified(mllm_config::engine_policy::Engine::Vllm, &version),
                "sglang" => !mllm_config::registration::is_verified(mllm_config::engine_policy::Engine::Sglang, &version),
                _ => true,
            };
            let missing = reported.as_ref().is_some_and(|p| p.installation.capabilities_missing.iter().any(|c| c == "deep_park"));
            serde_json::json!({
                "host_id": host.host_id, "host": host.host_name,
                "online": session.as_ref().is_some_and(|s| s.online && !host.revoked),
                "profile": name, "engine": engine, "version": version, "custom": custom,
                "executable": profile["executable"],
                "fingerprint": reported.as_ref().map(|p| serde_json::json!({
                    "version": p.installation.version, "digest": p.installation.digest, "state": p.installation.state,
                })),
                "deep_park": profile["security"]["deep_park"].as_str().unwrap_or("enabled"),
                "deep_park_probe": if missing { "capability_missing" } else { "not_reported_missing" },
                "published": "published",
                "retiring": retiring,
                "deployments": deployments,
            })
        })
        .collect();
    Json(serde_json::json!({"api_version":"1","engines":engines})).into_response()
}
```

(`ProfileView` and `InstallationView` in `mllm_controller::agent_sessions` need `#[derive(Clone)]` and public fields if they lack them; `ProfileView` is already `Serialize`.)

- [ ] **Step 6: Run the tests.**

Run: `cargo test -p mllm-management --all-targets --locked`
Expected: all pass, including the 2 new tests.

- [ ] **Step 7: Clippy and commit.**

Run: `cargo clippy -p mllm-management -p mllm-controller --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-management crates/mllm-controller/src/agent_sessions.rs
git commit -m "feat(management): profile retirement service and engines listing

ADR 0018 section 4: StoreRetirements writes the retirement with its
reference check, stops each instance through the ordinary stop path when
asked to drain, and confirms only on the stops' evidence. GET
/management/v1/engines lists every host's published profiles with the
derived custom mark and the deployments using each."
```

---

### Task 12: The owner-only control socket

**Files:**
- Create: `crates/mllm-agent/src/control_socket.rs`
- Modify: `crates/mllm-agent/src/lib.rs` (add `pub mod control_socket;`)
- Create: `crates/mllm-agent/tests/control_socket.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces (used by Tasks 14, 15, 17, 18, 20):
  - `pub const SOCKET_NAME: &str = "control.sock"`, `pub const MAX_LINE: usize = 64 * 1024`, `pub const MAX_PATH: usize = 107`
  - `pub enum ControlRequest { Add, Remove { profile: String, drain: bool }, List }` with `to_line(&self) -> String` and `parse_line(&str) -> Result<Self, String>`
  - `#[async_trait] pub trait ControlHandler: Send + Sync + 'static { async fn handle(&self, request: ControlRequest) -> serde_json::Value; }`
  - `pub enum ControlError { PathTooLong(PathBuf), InUse(PathBuf), Occupied(PathBuf), Io(String) }` (`Display`)
  - `pub struct ControlServer` with `pub fn bind(path: &Path) -> Result<Self, ControlError>`, `pub fn path(&self) -> &Path`, and `pub async fn serve(self, handler: Arc<dyn ControlHandler>, expected_uid: u32, shutdown: tokio::sync::watch::Receiver<bool>)`
  - `pub enum ClientError { Unreachable(String), Protocol(String), TimedOut }` (`Display`)
  - `pub async fn request(path: &Path, request: &ControlRequest, timeout: Duration) -> Result<serde_json::Value, ClientError>`
  - Reply convention for every handler: `{"ok": true, ...}` or `{"ok": false, "code": "<closed code>", "message": "..."}`.

- [ ] **Step 1: Write the failing tests.** Create `crates/mllm-agent/tests/control_socket.rs`:

```rust
//! ADR 0018 §3: the local control channel. Owner-only socket, peer uid
//! checked, one bounded request and reply per connection, nothing but engine
//! add, remove and list. CPU tests; not qualification.
use mllm_agent::control_socket::*;
use serde_json::{json, Value};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Echo(Mutex<Vec<ControlRequest>>);

#[async_trait::async_trait]
impl ControlHandler for Echo {
    async fn handle(&self, request: ControlRequest) -> Value {
        self.0.lock().unwrap().push(request.clone());
        json!({"ok": true, "request": request.to_line()})
    }
}

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn own_uid() -> u32 {
    unsafe { libc::geteuid() }
}

async fn serving(uid: u32) -> (tempfile::TempDir, std::path::PathBuf, Arc<Echo>, tokio::sync::watch::Sender<bool>) {
    let dir = private_dir();
    let path = dir.path().join(SOCKET_NAME);
    let server = ControlServer::bind(&path).unwrap();
    let echo = Arc::new(Echo(Mutex::new(vec![])));
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(server.serve(echo.clone(), uid, shutdown));
    (dir, path, echo, stop)
}

// T37: the socket is mode 0600 and a request from the owner round-trips.
#[tokio::test]
async fn the_owner_is_answered_over_a_0600_socket() {
    let (_dir, path, echo, _stop) = serving(own_uid()).await;
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let reply = request(&path, &ControlRequest::Remove { profile: "vllm".into(), drain: true }, Duration::from_secs(5)).await.unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(echo.0.lock().unwrap().len(), 1);
}

// T37: a connection from another user id is closed unanswered and the
// handler never runs.
#[tokio::test]
async fn another_user_id_is_refused() {
    let (_dir, path, echo, _stop) = serving(own_uid().wrapping_add(1)).await;
    let result = request(&path, &ControlRequest::List, Duration::from_secs(5)).await;
    assert!(result.is_err(), "{result:?}");
    assert!(echo.0.lock().unwrap().is_empty());
}

// T37: only the three operations, version 1, one bounded line.
#[tokio::test]
async fn only_bounded_known_requests_are_accepted() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_dir, path, echo, _stop) = serving(own_uid()).await;
    for line in [r#"{"v":1,"op":"shell","cmd":"id"}"#, r#"{"v":2,"op":"add"}"#, r#"{"v":1,"op":"remove"}"#, "not json"] {
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).await.unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["ok"], false, "{line}");
        assert_eq!(reply["code"], "invalid_request", "{line}");
    }
    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let _ = stream.write_all(&vec![b'x'; MAX_LINE + 10]).await;
    let mut reply = String::new();
    let _ = BufReader::new(stream).read_line(&mut reply).await;
    assert!(reply.is_empty() || reply.contains("invalid_request"), "{reply}");
    assert!(echo.0.lock().unwrap().is_empty());
}

// T33: a stale socket left by a crashed role is replaced; a live one is not;
// a regular file at the path is never removed.
#[tokio::test]
async fn a_stale_socket_is_replaced_but_a_live_one_or_a_file_is_not() {
    let (dir, path, _echo, _stop) = serving(own_uid()).await;
    assert!(matches!(ControlServer::bind(&path), Err(ControlError::InUse(_))));
    let stale = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert!(ControlServer::bind(&stale).is_ok());
    let file = dir.path().join("file.sock");
    std::fs::write(&file, "keep").unwrap();
    assert!(matches!(ControlServer::bind(&file), Err(ControlError::Occupied(_))));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
}

// T37 (Review Focus 3): a socket path over the sun_path limit is refused
// with its path, and the client says so instead of panicking.
#[tokio::test]
async fn an_overlong_socket_path_is_refused_cleanly() {
    let dir = private_dir();
    let deep = dir.path().join("d".repeat(120));
    std::fs::create_dir_all(&deep).unwrap();
    let path = deep.join(SOCKET_NAME);
    let refused = ControlServer::bind(&path);
    assert!(matches!(refused, Err(ControlError::PathTooLong(_))));
    let error = request(&path, &ControlRequest::List, Duration::from_secs(1)).await.unwrap_err();
    assert!(matches!(error, ClientError::Unreachable(ref m) if m.contains(SOCKET_NAME)), "{error}");
}
```

(Add `async-trait` is already a dependency of `mllm-agent`; add `tempfile` and `libc` to its `[dev-dependencies]` only if the test build asks for them.)

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p mllm-agent --test control_socket --locked`
Expected: FAIL to compile (`mllm_agent::control_socket` not found).

- [ ] **Step 3: Implement.** Add `pub mod control_socket;` to `crates/mllm-agent/src/lib.rs` and create `crates/mllm-agent/src/control_socket.rs`:

```rust
//! ADR 0018 §3: the role's local control channel, `<state_dir>/control.sock`.
//! Mode 0600 inside the role's 0700 state directory; a connection is served
//! only when `SO_PEERCRED` names the user id running mllm. One JSON request
//! line and one JSON reply line per connection, each at most 64 KiB. It
//! carries engine add, remove and list only, and never reaches an engine.
use serde_json::{json, Value};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub const SOCKET_NAME: &str = "control.sock";
pub const MAX_LINE: usize = 64 * 1024;
/// `sun_path` holds 108 bytes including the terminating NUL.
pub const MAX_PATH: usize = 107;
/// How long a connection may take to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections served at once; more wait in the listen backlog.
const CONCURRENT: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRequest {
    /// Re-read the role document merged with engines.yaml and publish it.
    Add,
    /// Retire a published profile, then rewrite the document and publish.
    Remove { profile: String, drain: bool },
    /// Report what the role has published and what uses it.
    List,
}

impl ControlRequest {
    pub fn to_line(&self) -> String {
        match self {
            Self::Add => json!({"v": 1, "op": "add"}),
            Self::Remove { profile, drain } => json!({"v": 1, "op": "remove", "profile": profile, "drain": drain}),
            Self::List => json!({"v": 1, "op": "list"}),
        }
        .to_string()
    }

    pub fn parse_line(line: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(line).map_err(|_| "not a JSON object".to_owned())?;
        let object = value.as_object().ok_or("not a JSON object")?;
        if object.get("v") != Some(&json!(1)) {
            return Err("unsupported request version".into());
        }
        let known = |keys: &[&str]| object.keys().all(|k| keys.contains(&k.as_str()));
        match object.get("op").and_then(Value::as_str) {
            Some("add") if known(&["v", "op"]) => Ok(Self::Add),
            Some("list") if known(&["v", "op"]) => Ok(Self::List),
            Some("remove") if known(&["v", "op", "profile", "drain"]) => {
                let profile = object.get("profile").and_then(Value::as_str).ok_or("remove needs a profile")?;
                let drain = object.get("drain").and_then(Value::as_bool).ok_or("remove needs drain")?;
                if profile.is_empty() || profile.len() > 64 {
                    return Err("invalid profile name".into());
                }
                Ok(Self::Remove { profile: profile.into(), drain })
            }
            _ => Err("unknown operation".into()),
        }
    }
}

#[async_trait::async_trait]
pub trait ControlHandler: Send + Sync + 'static {
    async fn handle(&self, request: ControlRequest) -> Value;
}

#[derive(Debug)]
pub enum ControlError {
    PathTooLong(PathBuf),
    InUse(PathBuf),
    Occupied(PathBuf),
    Io(String),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathTooLong(p) => write!(f, "{} is longer than the {MAX_PATH}-byte socket path limit; use a shorter state_dir", p.display()),
            Self::InUse(p) => write!(f, "{} is served by another running role", p.display()),
            Self::Occupied(p) => write!(f, "{} exists and is not a socket; it was left untouched", p.display()),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

pub struct ControlServer {
    path: PathBuf,
    listener: tokio::net::UnixListener,
}

impl ControlServer {
    /// Bind `path`. A stale socket (nothing accepting) is replaced; a live
    /// one or any other file is refused and left alone.
    pub fn bind(path: &Path) -> Result<Self, ControlError> {
        if path.as_os_str().len() > MAX_PATH {
            return Err(ControlError::PathTooLong(path.to_path_buf()));
        }
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(ControlError::Occupied(path.to_path_buf()));
            }
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(ControlError::InUse(path.to_path_buf()));
            }
            std::fs::remove_file(path).map_err(|e| ControlError::Io(e.to_string()))?;
        }
        let std_listener = std::os::unix::net::UnixListener::bind(path).map_err(|e| ControlError::Io(e.to_string()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| ControlError::Io(e.to_string()))?;
        std_listener.set_nonblocking(true).map_err(|e| ControlError::Io(e.to_string()))?;
        let listener = tokio::net::UnixListener::from_std(std_listener).map_err(|e| ControlError::Io(e.to_string()))?;
        Ok(Self { path: path.to_path_buf(), listener })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serve until `shutdown` turns true, then remove the socket.
    pub async fn serve(self, handler: Arc<dyn ControlHandler>, expected_uid: u32, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let permits = Arc::new(tokio::sync::Semaphore::new(CONCURRENT));
        loop {
            let accepted = tokio::select! {
                _ = shutdown.changed() => break,
                accepted = self.listener.accept() => accepted,
            };
            let Ok((stream, _)) = accepted else { continue };
            // ADR 0018 §3: only the user id running mllm; anyone else is
            // closed unanswered, before anything is read.
            if !matches!(stream.peer_cred(), Ok(cred) if cred.uid() == expected_uid) {
                drop(stream);
                continue;
            }
            let Ok(permit) = permits.clone().acquire_owned().await else { break };
            let handler = handler.clone();
            tokio::spawn(async move {
                let _permit = permit;
                serve_one(stream, handler).await;
            });
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve_one(stream: tokio::net::UnixStream, handler: Arc<dyn ControlHandler>) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(MAX_LINE as u64 + 1));
    let read = tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line)).await;
    let reply = match read {
        Ok(Ok(_)) if line.len() <= MAX_LINE && line.ends_with('\n') => match ControlRequest::parse_line(line.trim_end()) {
            Ok(request) => handler.handle(request).await,
            Err(message) => json!({"ok": false, "code": "invalid_request", "message": message}),
        },
        _ => json!({"ok": false, "code": "invalid_request", "message": "one request line of at most 64 KiB"}),
    };
    let mut text = reply.to_string();
    text.truncate(MAX_LINE - 1);
    text.push('\n');
    let _ = write.write_all(text.as_bytes()).await;
    let _ = write.shutdown().await;
}

#[derive(Debug)]
pub enum ClientError {
    Unreachable(String),
    Protocol(String),
    TimedOut,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) | Self::Protocol(m) => f.write_str(m),
            Self::TimedOut => f.write_str("the role did not answer in time"),
        }
    }
}

/// Send one request and read its reply within `timeout`.
pub async fn request(path: &Path, request: &ControlRequest, timeout: Duration) -> Result<Value, ClientError> {
    if path.as_os_str().len() > MAX_PATH {
        return Err(ClientError::Unreachable(format!(
            "{} is longer than the {MAX_PATH}-byte socket path limit",
            path.display()
        )));
    }
    let exchange = async {
        let mut stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|e| ClientError::Unreachable(format!("{}: {e}", path.display())))?;
        stream
            .write_all(format!("{}\n", request.to_line()).as_bytes())
            .await
            .map_err(|e| ClientError::Unreachable(format!("{}: {e}", path.display())))?;
        let mut line = String::new();
        BufReader::new(stream.take(MAX_LINE as u64))
            .read_line(&mut line)
            .await
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        if line.is_empty() {
            return Err(ClientError::Protocol("the role closed the connection without answering".into()));
        }
        serde_json::from_str(line.trim_end()).map_err(|_| ClientError::Protocol("the role's answer is not JSON".into()))
    };
    tokio::time::timeout(timeout, exchange).await.map_err(|_| ClientError::TimedOut)?
}
```

- [ ] **Step 4: Run the tests.**

Run: `cargo test -p mllm-agent --test control_socket --locked`
Expected: 5 passed.

- [ ] **Step 5: Clippy and commit.**

Run: `cargo clippy -p mllm-agent --all-targets --locked -- -D warnings`

```bash
git add crates/mllm-agent/src/lib.rs crates/mllm-agent/src/control_socket.rs crates/mllm-agent/tests/control_socket.rs crates/mllm-agent/Cargo.toml
git commit -m "feat(agent): owner-only local control socket

ADR 0018 section 3: <state_dir>/control.sock, mode 0600, served only to
the user id running mllm (SO_PEERCRED), one bounded JSON request and
reply per connection, and only engine add, remove and list. A stale
socket is replaced; a live one or a regular file is never touched."
```

---

### Task 13: Host live reload and removal: swappable profiles, the session channel, the control handler, role wiring

One deliverable: on a running host, `add` over the socket publishes a changed document live, and `remove` retires, rewrites and publishes. It needs the swappable profile state, the session channel and the handler together, and the role wiring that connects them.

**Files:**
- Create: `crates/mllm-agent/src/profiles.rs`, `crates/mllm-agent/src/host_control.rs`
- Modify: `crates/mllm-agent/src/lib.rs` (`pub mod profiles; pub mod host_control;`)
- Modify: `crates/mllm-agent/src/native_execution.rs` (fields `:76,:81,:92`; `new` `:132-195`; document reads `:335,:343,:526,:576,:583`; `inventory()` `:1422-1455`; unit tests `:2257-2290`), `crates/mllm-agent/src/native_execution/refusal.rs:87,127`
- Modify: `crates/mllm-agent/src/session.rs` (new `ProfileUpdates`; `run_session_with_updates`; `connect_once` `:276`; select loop `:473-607`)
- Modify: `crates/mllm-cli/src/remote_roles.rs` (`serve_host` `:616-790`, `serve_server` after `actions` `:424`; `execute` passes the host document path)
- Test: `crates/mllm-agent/src/profiles.rs` (unit), `crates/mllm-controller/tests/live_profiles.rs` (append)

**Interfaces:**
- Consumes: Task 2 `EnginesFile`, `engines_beside`, `lock_engines`, `write_engines`, `HostConfig::load`; Task 3 `only_profiles_differ`; Task 6 messages; Tasks 9–10 server behaviour; Task 11 `StoreRetirements`; Task 12 `ControlHandler`, `ControlRequest`, `ControlServer`, `SOCKET_NAME`.
- Produces (used by Tasks 14 and 15):
  - `pub fn profile_statuses(config: &HostConfig) -> Vec<pb::RuntimeProfileStatus>`
  - `pub struct ProfileSet { pub config: HostConfig, pub inventory: pb::ReportInventory, pub installations: Arc<InstallationRegistry>, pub fingerprint: String }` with `new(config, inventory)` and `measure(config, base: &pb::ReportInventory)`
  - `pub struct HostProfiles` with `new(ProfileSet) -> Arc<Self>`, `accepted()`, `pending()`, `for_fingerprint(&str)`, `stage(&str, ProfileSet) -> Result<Arc<ProfileSet>, Busy>`, `settle(&str, bool) -> bool`, `publishing()`, `replace(ProfileSet)`
  - `NativeHostExecution::profiles(&self) -> Arc<HostProfiles>`
  - `pub struct ProfileUpdates` with `new(Arc<HostProfiles>) -> Arc<Self>`, `profiles()`, `connected()`, `server_supports()`, `async publish(ProfileSet, Duration) -> PublishOutcome`, `async retire(&str, bool, Duration) -> RetireOutcome`
  - `pub enum PublishOutcome { Accepted, Rejected(String), RestartRequired, NotConnected, SessionEnded, Busy }`, `pub enum RetireOutcome { Confirmed, InUse(Vec<String>), Holding(Vec<String>), Refused(String), RestartRequired, NotConnected, SessionEnded }`
  - `pub async fn run_session_with_updates(identity, journal, inventory, shutdown, execution, drain, updates: Option<Arc<ProfileUpdates>>) -> Result<(), HostRevoked>`
  - `pub struct HostControl` with `new(document: PathBuf, running: HostConfig, updates: Arc<ProfileUpdates>, journal: Arc<HostJournal>) -> Arc<Self>` (`document` is the role's `host.yaml`; its engines file is `engines_beside(document)`); `impl ControlHandler`
  - `pub const PUBLISH_BOUND: Duration = Duration::from_secs(30)`, `pub const RETIRE_BOUND: Duration = Duration::from_secs(960)`
  - Control replies (the CLI in Task 14 reads exactly these): add → `{"ok":true,"published":"published"|"unchanged"|"restart_required"|"pending_session"}` or `{"ok":false,"code":"publish_rejected"|"invalid_config","message":..}`; remove → `{"ok":true,"removed":NAME,"published":..}` or `{"ok":false,"code":"profile_in_use","deployments":[..],"message":..}` / `agent_unreachable` / `publish_rejected`; list → `{"ok":true,"connected":bool,"live_profile_update":bool,"accepted":{NAME:{...}},"users":{NAME:[..]}}`

- [ ] **Step 1: Write the failing unit tests.** Create `crates/mllm-agent/src/profiles.rs` with only its test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn set(fingerprint: &str) -> ProfileSet {
        let dir = std::path::Path::new("/home/operator/host");
        let config = HostConfig::parse(&HostConfig::template(dir)).unwrap();
        let inventory = pb::ReportInventory { policy_fingerprint: fingerprint.into(), ..Default::default() };
        ProfileSet::new(config, inventory)
    }

    // T34 (ADR 0018 §3): a plan is authorized against the accepted, pending
    // or previous document, so an in-flight or redelivered command survives a
    // publication; older ones are forgotten.
    #[test]
    fn a_plan_is_authorized_against_accepted_pending_or_previous() {
        let profiles = HostProfiles::new(set("a"));
        profiles.stage("r1", set("b")).unwrap();
        assert!(profiles.publishing());
        for fp in ["a", "b"] {
            assert!(profiles.for_fingerprint(fp).is_some(), "{fp}");
        }
        assert!(profiles.settle("r1", true));
        assert_eq!(profiles.accepted().fingerprint, "b");
        assert!(profiles.for_fingerprint("a").is_some(), "previous kept");
        profiles.stage("r2", set("c")).unwrap();
        profiles.settle("r2", true);
        assert!(profiles.for_fingerprint("a").is_none(), "only one previous is kept");
        assert!(profiles.for_fingerprint("b").is_some());
    }

    // T07: a refused publication drops only the pending set.
    #[test]
    fn a_refused_publication_drops_only_the_pending_set() {
        let profiles = HostProfiles::new(set("a"));
        profiles.stage("r1", set("b")).unwrap();
        assert!(profiles.settle("r1", false));
        assert_eq!(profiles.accepted().fingerprint, "a");
        assert!(!profiles.publishing());
        assert!(profiles.for_fingerprint("b").is_none());
        assert!(!profiles.settle("unknown", true));
    }

    // ADR 0018 §3: one publication at a time.
    #[test]
    fn one_publication_at_a_time() {
        let profiles = HostProfiles::new(set("a"));
        profiles.stage("r1", set("b")).unwrap();
        assert!(profiles.stage("r2", set("c")).is_err());
    }
}
```

- [ ] **Step 2: Run them to verify they fail.**

Run: `cargo test -p mllm-agent --lib profiles --locked`
Expected: FAIL to compile (`ProfileSet`, `HostProfiles` not found).

- [ ] **Step 3: Implement `profiles.rs`** above the test module:

```rust
//! ADR 0018 §3: the host's runtime profiles as the server accepted them, the
//! one it is being asked to accept, and the one before. Launch plans are
//! authorized against whichever of these their fingerprint names, so a
//! publication never strands a command already sent or being redelivered.
use crate::installation::{register_profile, InstallationMeasurer, InstallationRegistry};
use mllm_config::remote_roles::HostConfig;
use mllm_protocol::pb;
use std::sync::{Arc, RwLock};

/// ADR 0008: each declared profile measured (version, digest); unmeasurable
/// is never a refusal. Moved from `mllm-cli`'s host start.
pub fn profile_statuses(config: &HostConfig) -> Vec<pb::RuntimeProfileStatus> {
    let measurer = InstallationMeasurer::new();
    config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let mut status = pb::RuntimeProfileStatus {
                name: name.clone(),
                build_fingerprint: profile["build_fingerprint"].as_str().unwrap_or("unknown").into(),
                eligibility: "unknown".into(),
                reason: String::new(),
                ..Default::default()
            };
            register_profile(&measurer, profile, &mut status);
            status
        })
        .collect()
}

pub struct ProfileSet {
    pub config: HostConfig,
    pub inventory: pb::ReportInventory,
    pub installations: Arc<InstallationRegistry>,
    pub fingerprint: String,
}

impl ProfileSet {
    pub fn new(config: HostConfig, inventory: pb::ReportInventory) -> Self {
        let installations = Arc::new(InstallationRegistry::from_inventory(&inventory));
        let fingerprint = inventory.policy_fingerprint.clone();
        Self { config, inventory, installations, fingerprint }
    }

    /// ADR 0018 §3: the host measures its profiles again and describes the
    /// new document; domains, boot id and launch claims are unchanged.
    pub fn measure(config: HostConfig, base: &pb::ReportInventory) -> Self {
        let mut inventory = base.clone();
        inventory.profiles = profile_statuses(&config);
        inventory.approved_host_config_json = config.document.to_string();
        inventory.policy_fingerprint = mllm_config::remote_resources::policy_fingerprint(&config.document);
        Self::new(config, inventory)
    }
}

#[derive(Debug)]
pub struct Busy;

struct State {
    accepted: Arc<ProfileSet>,
    previous: Option<Arc<ProfileSet>>,
    pending: Option<(String, Arc<ProfileSet>)>,
}

pub struct HostProfiles {
    state: RwLock<State>,
}

impl HostProfiles {
    pub fn new(set: ProfileSet) -> Arc<Self> {
        Arc::new(Self { state: RwLock::new(State { accepted: Arc::new(set), previous: None, pending: None }) })
    }
    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    pub fn accepted(&self) -> Arc<ProfileSet> {
        self.read().accepted.clone()
    }
    pub fn pending(&self) -> Option<(String, Arc<ProfileSet>)> {
        self.read().pending.clone()
    }
    pub fn publishing(&self) -> bool {
        self.read().pending.is_some()
    }
    pub fn for_fingerprint(&self, fingerprint: &str) -> Option<Arc<ProfileSet>> {
        let state = self.read();
        std::iter::once(&state.accepted)
            .chain(state.pending.as_ref().map(|(_, set)| set))
            .chain(state.previous.as_ref())
            .find(|set| set.fingerprint == fingerprint)
            .cloned()
    }
    pub fn stage(&self, request_id: &str, set: ProfileSet) -> Result<Arc<ProfileSet>, Busy> {
        let mut state = self.write();
        if state.pending.is_some() {
            return Err(Busy);
        }
        let set = Arc::new(set);
        state.pending = Some((request_id.to_owned(), set.clone()));
        Ok(set)
    }
    /// The server's verdict on `request_id`: promote or drop. `false` when
    /// nothing was pending under that id.
    pub fn settle(&self, request_id: &str, accepted: bool) -> bool {
        let mut state = self.write();
        match state.pending.take() {
            Some((id, set)) if id == request_id => {
                if accepted {
                    let old = std::mem::replace(&mut state.accepted, set);
                    state.previous = Some(old);
                }
                true
            }
            other => {
                state.pending = other;
                false
            }
        }
    }
    /// Startup and unit tests: replace the accepted set outright.
    pub fn replace(&self, set: ProfileSet) {
        let mut state = self.write();
        state.accepted = Arc::new(set);
        state.previous = None;
        state.pending = None;
    }
}
```

- [ ] **Step 4: Run the unit tests.**

Run: `cargo test -p mllm-agent --lib profiles --locked`
Expected: 3 passed.

- [ ] **Step 5: Route the native executor through `HostProfiles`.** In `native_execution.rs`:

- Replace the fields `inventory: pb::ReportInventory` and `installations: Arc<InstallationRegistry>` with `profiles: Arc<crate::profiles::HostProfiles>`; keep `config: HostConfig` for role-local settings (state, logs, ingress, load interval).
- In `new`, after setting `inventory.launch_claims`, build `let profiles = crate::profiles::HostProfiles::new(crate::profiles::ProfileSet::new(config.clone(), inventory));` and store it.
- Add `pub fn profiles(&self) -> Arc<crate::profiles::HostProfiles> { self.profiles.clone() }`.
- At `:335` replace the fingerprint comparison and the document read at `:343` with:

```rust
        // ADR 0018 §3: authorized against the document the plan names:
        // accepted, being published, or the one before.
        let set = self.profiles.for_fingerprint(&plan.host_policy_fingerprint);
        if command.identity.controller_id != self.controller_id
            || command.identity.member.host_id != self.host_id
            || (legacy.is_none() && set.is_none())
        {
            return Err(JournalError::Unauthorized);
        }
        let set = set.unwrap_or_else(|| self.profiles.accepted());
        // ... later, where `self.config.document` was read:
        let host = mllm_config::remote_resources::local_host_document(&set.config.document)
            .map_err(|_| JournalError::Unauthorized)?;
```

  and pass `&set.installations` wherever this launch path used `self.installations`.
- At `:526` use `self.profiles.for_fingerprint(&plan.host_policy_fingerprint).is_some()` as the guard; at `:576-583` (`locate`) use `let Some(set) = self.profiles.for_fingerprint(&plan.host_policy_fingerprint) else { return Err("unauthorized") };` and `set.config.document`.
- In `refusal.rs:87,127` take the registry from `self.profiles.accepted().installations` for Park (a running engine's profile never changes: removal waits for it to stop).
- `inventory()` (`:1422`): `let set = self.profiles.accepted(); let mut inventory = set.inventory.clone(); set.installations.overlay(&mut inventory.profiles);` then the domain refresh as today.
- Unit tests at `:2257-2290` that mutated `host.config.document`, `host.inventory.profiles` or `host.installations` build the changed `HostConfig` and inventory and call `host.profiles.replace(ProfileSet::new(config, inventory))` instead.

Run: `cargo test -p mllm-agent --all-targets --locked`
Expected: all existing native execution tests pass unchanged in behaviour.

- [ ] **Step 6: Write the failing session and handler tests.** Append to `crates/mllm-controller/tests/live_profiles.rs`:

```rust
use mllm_agent::control_socket::{ControlHandler, ControlRequest};
use mllm_agent::host_control::HostControl;
use mllm_agent::profiles::{HostProfiles, ProfileSet};
use mllm_agent::session::{run_session_with_updates, ProfileUpdates};
use mllm_config::registration::EnginesFile;

/// A running agent session with live updates, its host document on disk, and
/// the control handler over both.
struct Agent {
    updates: Arc<ProfileUpdates>,
    control: Arc<HostControl>,
    document: std::path::PathBuf,
    _dir: tempfile::TempDir,
    stop: tokio::sync::watch::Sender<bool>,
}

async fn agent(h: &Harness) -> Agent {
    let dir = directory();
    let document = dir.path().join("host.yaml");
    std::fs::write(&document, prepared_document().to_string()).unwrap();
    let config = mllm_config::remote_roles::HostConfig::parse(&prepared_document().to_string()).unwrap();
    let profiles = HostProfiles::new(ProfileSet::new(config.clone(), inventory(&h.host)));
    let updates = ProfileUpdates::new(profiles);
    let journal = mllm_agent::journal::HostJournal::open(dir.path(), &h.identity.controller_id(), &h.host).unwrap();
    let control = HostControl::new(document.clone(), config, updates.clone(), journal.clone());
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let (identity, inv, u) = (h.identity.clone(), inventory(&h.host), updates.clone());
    tokio::spawn(async move {
        let _ = run_session_with_updates(&identity, journal, inv, shutdown, None, None, Some(u)).await;
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !updates.connected() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    Agent { updates, control, document, _dir: dir, stop }
}

/// `engine add`'s write: `vllm` into the engines file beside `document`.
fn add_vllm_to(document: &std::path::Path) {
    use mllm_config::registration::{engines_beside, lock_engines, write_engines, EnginesFile};
    let path = engines_beside(document);
    let lock = lock_engines(&path).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    let with: Value = serde_json::from_str(&with_vllm("x").approved_host_config_json).unwrap();
    engines.profiles.insert("vllm".into(), with["runtime_profiles"]["vllm"].clone());
    write_engines(&engines, &lock, Some(&prepared_document())).unwrap();
}

// T07 T34: an added profile is published live and promoted on acceptance.
#[tokio::test]
async fn an_added_profile_is_published_and_promoted() {
    let h = enrolled().await;
    let a = agent(&h).await;
    add_vllm_to(&a.document);
    let host_before = std::fs::read(&a.document).unwrap();
    let reply = a.control.handle(ControlRequest::Add).await;
    assert_eq!(reply["published"], "published", "{reply}");
    assert!(a.updates.profiles().accepted().config.profiles.contains_key("vllm"));
    assert_eq!(std::fs::read(&a.document).unwrap(), host_before, "host.yaml is never rewritten");
    let approved = h.state.lock().unwrap().store().host_publication(&h.host).unwrap().unwrap();
    assert!(approved.config_json.contains("\"vllm\""));
    // Nothing changed since: not published again.
    assert_eq!(a.control.handle(ControlRequest::Add).await["published"], "unchanged");
    a.stop.send(true).unwrap();
    h.server.abort();
}

// T03 (Review Focus 1): an edit outside runtime_profiles is not published live.
#[tokio::test]
async fn a_reload_refuses_a_document_changed_outside_profiles() {
    let h = enrolled().await;
    let a = agent(&h).await;
    let mut doc: Value = prepared_document();
    doc["load_report_interval"] = json!("9s");
    std::fs::write(&a.document, doc.to_string()).unwrap();
    let before = h.state.lock().unwrap().store().host_publication(&h.host).unwrap().unwrap().fingerprint;
    let reply = a.control.handle(ControlRequest::Add).await;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["code"], "publish_rejected");
    assert!(reply["message"].as_str().unwrap().contains("restart the role"), "{reply}");
    assert!(!a.updates.profiles().publishing());
    assert_eq!(h.state.lock().unwrap().store().host_publication(&h.host).unwrap().unwrap().fingerprint, before);
    a.stop.send(true).unwrap();
    h.server.abort();
}

// T16: a published profile is removed only after the server confirms; the
// engines file is rewritten (host.yaml never) and the removal published.
#[tokio::test]
async fn a_confirmed_removal_rewrites_and_publishes() {
    let h = enrolled().await;
    h.sessions.with_profile_retirements(Arc::new(Scripted {
        first: RetirementStep::Confirmed, waits: 0.into(), last: RetirementStep::Confirmed, seen: Mutex::new(vec![]),
    }));
    let a = agent(&h).await;
    add_vllm_to(&a.document);
    assert_eq!(a.control.handle(ControlRequest::Add).await["published"], "published");
    // The scripted service confirms without the store row the publication
    // transaction needs; write it as StoreRetirements would.
    h.state.lock().unwrap().store().begin_profile_retirement(&h.host, "vllm", "k", 1, i64::MAX / 2, false).unwrap();
    let reply = a.control.handle(ControlRequest::Remove { profile: "vllm".into(), drain: false }).await;
    assert_eq!(reply["removed"], "vllm", "{reply}");
    let engines = mllm_config::registration::EnginesFile::load(&mllm_config::registration::engines_beside(&a.document)).unwrap();
    assert!(!engines.profiles.contains_key("vllm"));
    assert_eq!(engines.revision, 2);
    assert!(!a.updates.profiles().accepted().config.profiles.contains_key("vllm"));
    a.stop.send(true).unwrap();
    h.server.abort();
}

// T16 T32: removal while in use writes nothing.
#[tokio::test]
async fn a_removal_in_use_writes_nothing() {
    let h = enrolled().await;
    h.sessions.with_profile_retirements(Arc::new(Scripted {
        first: RetirementStep::InUse(vec!["q14".into()]), waits: 0.into(), last: RetirementStep::Confirmed, seen: Mutex::new(vec![]),
    }));
    let a = agent(&h).await;
    add_vllm_to(&a.document);
    assert_eq!(a.control.handle(ControlRequest::Add).await["published"], "published");
    let engines = mllm_config::registration::engines_beside(&a.document);
    let before = std::fs::read(&engines).unwrap();
    let reply = a.control.handle(ControlRequest::Remove { profile: "vllm".into(), drain: false }).await;
    assert_eq!(reply["code"], "profile_in_use");
    assert_eq!(reply["deployments"], json!(["q14"]));
    assert_eq!(std::fs::read(&engines).unwrap(), before);
    // A profile declared in host.yaml is the operator's: never removed here.
    let theirs = a.control.handle(ControlRequest::Remove { profile: "local".into(), drain: false }).await;
    assert_eq!(theirs["code"], "invalid_config", "{theirs}");
    a.stop.send(true).unwrap();
    h.server.abort();
}

// Owner decision 2026-09-25: without a session, a published profile is not removed.
#[tokio::test]
async fn a_removal_without_a_session_writes_nothing() {
    let dir = directory();
    let document = dir.path().join("host.yaml");
    std::fs::write(&document, prepared_document().to_string()).unwrap();
    let journal = mllm_agent::journal::HostJournal::open(dir.path(), "c", "h").unwrap();
    add_vllm_to(&document);
    let config = mllm_config::remote_roles::HostConfig::load(&document).unwrap();
    let set = ProfileSet::new(config.clone(), with_vllm("h"));
    let updates = ProfileUpdates::new(HostProfiles::new(set));
    let control = HostControl::new(document.clone(), config, updates, journal);
    let reply = control.handle(ControlRequest::Remove { profile: "vllm".into(), drain: true }).await;
    assert_eq!(reply["code"], "agent_unreachable");
    assert!(EnginesFile::load(&mllm_config::registration::engines_beside(&document)).unwrap().profiles.contains_key("vllm"));
}
```

(`PendingEnrollment` must be `Clone` for `h.identity.clone()`; if it is not, restructure `agent()` to take the harness's identity by moving a freshly loaded one from its storage directory: `PendingEnrollment::load(&IdentityDirectory::open(path))`.)

- [ ] **Step 7: Run them to verify they fail.**

Run: `cargo test -p mllm-controller --test live_profiles --locked added_profile reload_refuses confirmed_removal removal_in_use removal_without`
Expected: FAIL to compile (`host_control`, `ProfileUpdates`, `run_session_with_updates` not found).

- [ ] **Step 8: Add `ProfileUpdates` to `session.rs`.**

```rust
/// ADR 0018 §3, §4: requests from the local control handler to the live
/// session, and what the session learned of its server.
pub struct ProfileUpdates {
    profiles: Arc<crate::profiles::HostProfiles>,
    sender: mpsc::Sender<ProfileRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<ProfileRequest>>,
    connected: watch::Sender<bool>,
    server: watch::Sender<std::collections::BTreeSet<String>>,
}

enum ProfileRequest {
    Publish { request_id: String, reply: tokio::sync::oneshot::Sender<PublishOutcome> },
    Retire { request_id: String, profile: String, drain: bool, reply: mpsc::Sender<pb::ProfileRetirement> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome { Accepted, Rejected(String), RestartRequired, NotConnected, SessionEnded, Busy }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetireOutcome { Confirmed, InUse(Vec<String>), Holding(Vec<String>), Refused(String), RestartRequired, NotConnected, SessionEnded }

impl ProfileUpdates {
    pub fn new(profiles: Arc<crate::profiles::HostProfiles>) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(4);
        Arc::new(Self {
            profiles,
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            connected: watch::channel(false).0,
            server: watch::channel(Default::default()).0,
        })
    }
    pub fn profiles(&self) -> &Arc<crate::profiles::HostProfiles> {
        &self.profiles
    }
    pub fn connected(&self) -> bool {
        *self.connected.borrow()
    }
    pub fn server_supports(&self) -> bool {
        self.server.borrow().contains(mllm_protocol::capabilities::LIVE_PROFILE_UPDATE)
    }

    /// ADR 0018 §3: stage `set`, send it, and settle on the server's verdict.
    pub async fn publish(&self, set: crate::profiles::ProfileSet, bound: Duration) -> PublishOutcome {
        if !self.connected() {
            return PublishOutcome::NotConnected;
        }
        if !self.server_supports() {
            return PublishOutcome::RestartRequired;
        }
        let request_id = ulid::Ulid::new().to_string();
        if self.profiles.stage(&request_id, set).is_err() {
            return PublishOutcome::Busy;
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = self.sender.send(ProfileRequest::Publish { request_id: request_id.clone(), reply }).await;
        let outcome = match (sent, tokio::time::timeout(bound, answer).await) {
            (Ok(()), Ok(Ok(outcome))) => outcome,
            _ => PublishOutcome::SessionEnded,
        };
        if outcome != PublishOutcome::Accepted {
            // Accepted and refused verdicts were settled by the session loop;
            // anything else leaves nothing staged.
            self.profiles.settle(&request_id, false);
        }
        outcome
    }

    /// ADR 0018 §4: ask the server to retire `profile`; wait for a terminal
    /// answer (`draining` is progress, not an answer).
    pub async fn retire(&self, profile: &str, drain: bool, bound: Duration) -> RetireOutcome {
        if !self.connected() {
            return RetireOutcome::NotConnected;
        }
        if !self.server_supports() {
            return RetireOutcome::RestartRequired;
        }
        let (reply, mut answers) = mpsc::channel(4);
        let request = ProfileRequest::Retire { request_id: ulid::Ulid::new().to_string(), profile: profile.into(), drain, reply };
        if self.sender.send(request).await.is_err() {
            return RetireOutcome::SessionEnded;
        }
        let wait = async {
            while let Some(answer) = answers.recv().await {
                match answer.outcome.as_str() {
                    "draining" => continue,
                    "confirmed" => return RetireOutcome::Confirmed,
                    "in_use" => return RetireOutcome::InUse(answer.deployments),
                    "holding" => return RetireOutcome::Holding(answer.deployments),
                    _ => return RetireOutcome::Refused(answer.reason),
                }
            }
            RetireOutcome::SessionEnded
        };
        tokio::time::timeout(bound, wait).await.unwrap_or(RetireOutcome::SessionEnded)
    }
}
```

Wire it into the loop (`connect_once` gains `updates: Option<Arc<ProfileUpdates>>`; `run_session_with_drain` calls the new `run_session_with_updates(.., None)`, which is the old body with `updates` threaded to `connect_once`):

```rust
    // ADR 0018: requests from the control handler, held for this session.
    let mut requests = match &updates { Some(u) => Some(u.receiver.lock().await), None => None };
    let mut publishing: std::collections::BTreeMap<String, tokio::sync::oneshot::Sender<PublishOutcome>> = Default::default();
    let mut retiring: std::collections::BTreeMap<String, mpsc::Sender<pb::ProfileRetirement>> = Default::default();
    struct Disconnected(Option<Arc<ProfileUpdates>>);
    impl Drop for Disconnected {
        fn drop(&mut self) {
            if let Some(u) = &self.0 { u.connected.send_replace(false); }
        }
    }
    let _disconnected = Disconnected(updates.clone());
```

New `select!` arm (before `stream.message()`):

```rust
            request = async { match requests.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } }, if fence.is_some() => {
                match request {
                    Some(ProfileRequest::Publish { request_id, reply }) => {
                        let staged = updates.as_ref().and_then(|u| u.profiles.pending()).filter(|(id, _)| *id == request_id);
                        let Some((_, set)) = staged else { let _ = reply.send(PublishOutcome::SessionEnded); continue };
                        let mut inventory = set.inventory.clone();
                        inventory.envelope = Some(pb::Envelope { host_id: host.clone(), protocol_version: mllm_protocol::PROTOCOL_VERSION.into(), ..Default::default() });
                        reports.try_send(frame(agent_to_server::Msg::PublishProfiles(pb::PublishProfiles { request_id: request_id.clone(), inventory: Some(inventory) })))
                            .map_err(end("outbound report queue is full"))?;
                        publishing.insert(request_id, reply);
                    }
                    Some(ProfileRequest::Retire { request_id, profile, drain, reply }) => {
                        reports.try_send(frame(agent_to_server::Msg::RetireProfile(pb::RetireProfile { request_id: request_id.clone(), profile, drain })))
                            .map_err(end("outbound report queue is full"))?;
                        retiring.insert(request_id, reply);
                    }
                    None => {}
                }
                continue;
            },
```

Gate the observation tick with `if fence.is_some() && !updates.as_ref().is_some_and(|u| u.profiles.publishing())`. In the `SessionReady` arm, after `fence = Some(connected);`:

```rust
                if let Some(u) = &updates {
                    u.server.send_replace(ready.capabilities.iter().cloned().collect());
                    u.connected.send_replace(true);
                }
```

New incoming arms before the final `_ =>`:

```rust
            // ADR 0018 §3: settle before the next inventory tick.
            Some(server_to_agent::Msg::ProfilesPublished(verdict)) if fence.is_some() => {
                if let Some(u) = &updates { u.profiles.settle(&verdict.request_id, verdict.accepted); }
                if let Some(reply) = publishing.remove(&verdict.request_id) {
                    let _ = reply.send(if verdict.accepted { PublishOutcome::Accepted } else { PublishOutcome::Rejected(verdict.reason) });
                }
            }
            Some(server_to_agent::Msg::ProfileRetirement(answer)) if fence.is_some() => {
                let terminal = answer.outcome != "draining";
                let id = answer.request_id.clone();
                if let Some(reply) = retiring.get(&id) { let _ = reply.try_send(answer); }
                if terminal { retiring.remove(&id); }
            }
```

- [ ] **Step 9: Implement `host_control.rs`.**

```rust
//! ADR 0018 §3, §4: the host's answers to `mllm engine add`, `remove` and
//! `list` over the control socket. Add re-reads host.yaml merged with its
//! engines.yaml and publishes it live; remove retires on the server first and
//! rewrites engines.yaml (never host.yaml) only
//! after confirmation. Nothing here reaches an engine.
use crate::control_socket::{ControlHandler, ControlRequest};
use crate::journal::HostJournal;
use crate::profiles::ProfileSet;
use crate::session::{ProfileUpdates, PublishOutcome, RetireOutcome};
use mllm_config::registration::{engines_beside, lock_engines, only_profiles_differ, write_engines, EnginesFile};
use mllm_config::remote_roles::HostConfig;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const PUBLISH_BOUND: Duration = Duration::from_secs(30);
pub const RETIRE_BOUND: Duration = Duration::from_secs(960);

pub struct HostControl {
    document: PathBuf,
    running: HostConfig,
    updates: Arc<ProfileUpdates>,
    journal: Arc<HostJournal>,
}

fn refused(code: &str, message: impl Into<String>) -> Value {
    json!({"ok": false, "code": code, "message": message.into()})
}

impl HostControl {
    pub fn new(document: PathBuf, running: HostConfig, updates: Arc<ProfileUpdates>, journal: Arc<HostJournal>) -> Arc<Self> {
        Arc::new(Self { document, running, updates, journal })
    }

    /// Re-read the document and publish it if it changed (profiles only).
    async fn reload(&self) -> Value {
        let loaded = HostConfig::load(&self.document);
        let config = match loaded {
            Ok(config) => config,
            Err(error) => return refused("invalid_config", format!("{}: {}", error.path, error.detail)),
        };
        if !only_profiles_differ(&self.running.document, &config.document) {
            return refused(
                "publish_rejected",
                "the document changed outside runtime_profiles; only runtime profiles change live; restart the role to apply the rest",
            );
        }
        let accepted = self.updates.profiles().accepted();
        let set = tokio::task::spawn_blocking({
            let base = accepted.inventory.clone();
            move || ProfileSet::measure(config, &base)
        })
        .await;
        let Ok(set) = set else { return refused("internal", "measuring the profiles failed") };
        if set.fingerprint == accepted.fingerprint {
            return json!({"ok": true, "published": "unchanged"});
        }
        match self.updates.publish(set, PUBLISH_BOUND).await {
            PublishOutcome::Accepted => json!({"ok": true, "published": "published"}),
            PublishOutcome::Rejected(reason) => refused("publish_rejected", reason),
            PublishOutcome::RestartRequired => json!({"ok": true, "published": "restart_required"}),
            PublishOutcome::NotConnected => json!({"ok": true, "published": "pending_session"}),
            PublishOutcome::SessionEnded => refused("agent_unreachable", "the control session ended before the server answered; retry"),
            PublishOutcome::Busy => refused("publish_rejected", "another publication is in progress; retry"),
        }
    }

    fn write_without(&self, profile: &str) -> Result<u64, Value> {
        let path = engines_beside(&self.document);
        let lock = lock_engines(&path).map_err(|e| refused("internal", e.detail))?;
        let mut engines = EnginesFile::load(&path).map_err(|e| refused("invalid_config", e.detail))?;
        engines.profiles.remove(profile);
        write_engines(&engines, &lock, None).map_err(|e| refused("internal", e.detail))
    }

    async fn remove(&self, profile: &str, drain: bool) -> Value {
        // ADR 0018 §2: only what `engine add` registered is removed here; a
        // profile the operator declared in host.yaml stays theirs to edit.
        match EnginesFile::load(&engines_beside(&self.document)) {
            Ok(engines) if engines.profiles.contains_key(profile) => {}
            Ok(_) if self.running.profiles.contains_key(profile) => {
                return refused("invalid_config", format!("{profile} is declared in {}; edit that file and restart the role", self.document.display()))
            }
            Ok(_) => return refused("invalid_config", format!("no registered profile named {profile}")),
            Err(e) => return refused("invalid_config", e.detail),
        }
        let published = self.updates.profiles().accepted().config.profiles.contains_key(profile);
        if published {
            // Owner decision 2026-09-25: never without the server's confirmation.
            match self.updates.retire(profile, drain, RETIRE_BOUND).await {
                RetireOutcome::Confirmed => {}
                RetireOutcome::InUse(deployments) | RetireOutcome::Holding(deployments) => {
                    return json!({"ok": false, "code": "profile_in_use", "deployments": deployments,
                        "message": "deployments on this host use the profile; stop them, or use --drain"});
                }
                RetireOutcome::Refused(reason) => return refused("publish_rejected", reason),
                RetireOutcome::RestartRequired => return refused("publish_rejected",
                    "the server does not support live profile updates; stop the deployments, stop the role, edit the document, then start it"),
                RetireOutcome::NotConnected | RetireOutcome::SessionEnded => return refused("agent_unreachable",
                    "the host has no control session; nothing was removed"),
            }
        }
        if let Err(reply) = self.write_without(profile) {
            return reply;
        }
        let mut reply = self.reload().await;
        if reply["ok"] == true {
            reply["removed"] = profile.into();
        }
        reply
    }

    fn list(&self) -> Value {
        let accepted = self.updates.profiles().accepted();
        let mut users = serde_json::Map::new();
        for claimed in self.journal.claimed_launches("").unwrap_or_default() {
            if let mllm_protocol::execution::MemberAction::LaunchSingle(plan) = &claimed.command.action {
                let name = serde_json::from_str::<Value>(&plan.deployment_config)
                    .ok()
                    .and_then(|d| d["name"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| claimed.command.identity.deployment_id.clone());
                users.entry(plan.profile_name.clone()).or_insert_with(|| json!([])).as_array_mut().unwrap().push(name.into());
            }
        }
        let mut profiles = serde_json::Map::new();
        for status in &accepted.inventory.profiles {
            let probe = match accepted.installations.last_capabilities(&status.name) {
                Some(report) if report.available("deep_park") == Some(false) => "capability_missing",
                Some(_) => "available",
                None => "unknown",
            };
            let declared = &accepted.config.profiles[&status.name];
            profiles.insert(status.name.clone(), json!({
                "engine": declared["engine"], "executable": declared["executable"],
                "build_fingerprint": status.build_fingerprint,
                "installation": {"version": status.installation_version, "digest": status.installation_digest, "state": status.installation_state},
                "deep_park": declared["security"]["deep_park"], "deep_park_probe": probe,
            }));
        }
        json!({"ok": true, "connected": self.updates.connected(), "live_profile_update": self.updates.server_supports(),
            "accepted": profiles, "users": users})
    }
}

#[async_trait::async_trait]
impl ControlHandler for HostControl {
    async fn handle(&self, request: ControlRequest) -> Value {
        match request {
            ControlRequest::Add => self.reload().await,
            ControlRequest::Remove { profile, drain } => self.remove(&profile, drain).await,
            ControlRequest::List => self.list(),
        }
    }
}
```

- [ ] **Step 10: Wire the host and server roles.** In `crates/mllm-cli/src/remote_roles.rs`:

- `serve_host(config: HostConfig, document: PathBuf)`; `execute` passes the resolved `--config` path it already loaded with `HostConfig::load` (Task 2), so the running configuration is host.yaml merged with its engines.yaml.
- Replace the inline profile measurement (`:668-683`) with `let profiles = tokio::task::spawn_blocking({ let c = config.clone(); move || mllm_agent::profiles::profile_statuses(&c) }).await.map_err(|_| unavailable())?;`.
- After `execution` is built: `let host_profiles = match &execution_native { Some(e) => e.profiles(), None => mllm_agent::profiles::HostProfiles::new(mllm_agent::profiles::ProfileSet::new(config.clone(), inventory.clone())) };` (keep a typed `Arc<NativeHostExecution>` before erasing it to `Arc<dyn SessionExecution>`), then `let updates = mllm_agent::session::ProfileUpdates::new(host_profiles);`.
- Call `run_session_with_updates(&identity, journal.clone(), inventory, receiver, execution, Some(drain_signal.clone()), Some(updates.clone()))`.
- Control socket:

```rust
    // ADR 0018 §3: the local control channel. A socket that cannot be bound
    // (path too long, another role) is reported and the role runs without it.
    let (control_stop, control_shutdown) = tokio::sync::watch::channel(false);
    match mllm_agent::control_socket::ControlServer::bind(&config.state_dir.join(mllm_agent::control_socket::SOCKET_NAME)) {
        Ok(server) => {
            let handler = mllm_agent::host_control::HostControl::new(document.clone(), config.clone(), updates.clone(), journal.clone());
            tokio::spawn(server.serve(handler, unsafe { libc::geteuid() }, control_shutdown));
        }
        Err(error) => eprintln!("host control socket unavailable: {error}"),
    }
```

  and `let _ = control_stop.send(true);` where the role shuts down (beside `shutdown.send(true)`).
- In `serve_server`, right after `actions` is built:

```rust
    // ADR 0018 §4: retiring a host's runtime profile through the ordinary stop path.
    sessions.with_profile_retirements(Arc::new(mllm_management::engines::StoreRetirements::new(actions.clone())));
```

Add to `crates/mllm-cli/tests/remote_roles.rs`, using its `enrolled_server`, `Service` and `hosts` helpers:

```rust
// T37 (ADR 0018 §3): a started host serves an owner-only control socket that
// answers `list`, and the server has a retirement service installed.
#[test]
fn a_started_host_serves_its_control_socket() {
    use std::os::unix::fs::PermissionsExt;
    let temp = root();
    let (mut server, server_root, server_config, host_root, host_config) =
        enrolled_server(temp.path(), "socket-spark");
    let mut host = Service::start(&host_root, "host", &host_config);
    hosts(&server_root, &server_config, true, 1);
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&host_config).unwrap()).unwrap();
    let socket = Path::new(state["state_dir"].as_str().unwrap()).join("control.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let reply = runtime
        .block_on(mllm_agent::control_socket::request(
            &socket,
            &mllm_agent::control_socket::ControlRequest::List,
            std::time::Duration::from_secs(5),
        ))
        .unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["connected"], true, "{reply}");
    assert_eq!(reply["live_profile_update"], true, "{reply}");
    host.stop();
    server.stop();
}
```

(`mllm-agent` and `tokio` are already dependencies of `mllm-cli`; add them to its `[dev-dependencies]` only if the test build asks.)

- [ ] **Step 11: Run the tests.**

Run: `cargo test -p mllm-agent --all-targets --locked && cargo test -p mllm-controller --test live_profiles --locked && cargo test -p mllm-cli --test remote_roles --locked`
Expected: all pass (12 tests in `live_profiles`).

- [ ] **Step 12: Core suite, workspace, Clippy.**

Run: the three commands in Global Constraints.
Expected: all pass, no warnings.

- [ ] **Step 13: Commit**

```bash
git add crates/mllm-agent crates/mllm-controller/tests/live_profiles.rs crates/mllm-cli/src/remote_roles.rs crates/mllm-cli/tests/remote_roles.rs
git commit -m "feat(agent): live engine add and remove on a running host

ADR 0018 sections 3 and 4: the host keeps its accepted, pending and
previous profile sets and authorizes plans against whichever the plan
names; the session carries PublishProfiles and RetireProfile when the
server supports them; the control handler reloads host.yaml merged with
engines.yaml (profiles only), publishes it, and removes a published profile only after the
server confirms its retirement. The host role serves the socket and the
server installs the retirement service."
```

---

### Task 14: CLI: `mllm engine detect|add|list|remove`, `mllm list engines`, exit codes 16–23

One deliverable: the operator-facing commands, their grammar, their closed codes and exit codes, and the install guide's exit-code table.

**Files:**
- Create: `crates/mllm-cli/src/engine.rs`, `crates/mllm-cli/src/engine/target.rs`, `crates/mllm-cli/tests/engine_cli.rs`
- Modify: `crates/mllm-cli/src/lib.rs` (`pub mod engine;`), `crates/mllm-cli/src/grammar.rs` (`ListResource` `:40-44`, `Command` `:53-140`, `label` `:142-213`, `CliCommand` `:233-332`, `ListArgs` `:473-477`, `From<CliCommand>` `:550-688`), `crates/mllm-cli/src/output.rs` (`ExitCode` `:11-34`, `exit_code` `:125-141`), `crates/mllm-cli/src/main.rs` (new branch beside `Drain`, `:60-84`), `crates/mllm-cli/src/remote_roles.rs` (`supports` `:809-824`, list arm `:966-990`), `crates/mllm-cli/tests/grammar.rs`, `crates/mllm-cli/tests/errors.rs`, `docs/operations/install.md` (exit-code table near `:106`)

**Interfaces:**
- Consumes: Tasks 2–5 (`EnginesFile`, `engines_path`, `config_home`, `lock_engines`, `write_engines`, profile spec, resolve, check_version, detect), Task 12 (`request`, `ControlRequest`, `SOCKET_NAME`), Task 13 reply shapes, `mllm_agent::installation::{InstallationMeasurer, probe_capabilities, PROBE_TIMEOUT}`, `crate::managed_runtime::prepare_for_role`.
- Produces (used by Task 15):
  - `pub enum DeepParkChoice { Enabled, Disabled }`, `pub enum DriftChoice { Warn, Refuse }` (clap `ValueEnum`)
  - `Command::EngineDetect { paths: Vec<PathBuf> }`, `Command::EngineAdd { path: Option<PathBuf>, name: Option<String>, deep_park: Option<DeepParkChoice>, drift: DriftChoice, args: Vec<String> }`, `Command::EngineList`, `Command::EngineRemove { name: String, drain: bool }`, `ListResource::Engines`
  - `ExitCode::{ENGINE_NOT_FOUND(16), ENGINE_UNSUPPORTED(17), ENGINE_VERSION_FAILED(18), PROFILE_EXISTS(19), PROFILE_IN_USE(20), PUBLISH_REJECTED(21), AGENT_UNREACHABLE(22), NOT_INTERACTIVE(23)}`
  - `pub enum RoleKind { Host, Standalone }`, `pub struct Target { pub role_document: PathBuf, pub kind: RoleKind, pub engines: PathBuf, pub state_dir: PathBuf, pub socket: PathBuf }`, `pub fn resolve_target(explicit: Option<&Path>, state_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Target, StructuredError>`
  - `pub async fn execute(command: &Command, config: Option<&Path>, state_dir: &Path) -> Result<Value, StructuredError>`
  - `pub fn is_engine_command(command: &Command) -> bool`

- [ ] **Step 1: Write the failing grammar and exit-code tests.** Append to `crates/mllm-cli/tests/grammar.rs`:

```rust
use mllm_cli::grammar::{DeepParkChoice, DriftChoice};

// T01 (ADR 0018 §1): the engine commands and `list engines` parse strictly.
#[test]
fn engine_commands_parse() {
    assert_eq!(
        parse(["mllm", "engine", "add", "/v", "--name", "vllm-patched", "--deep-park", "disabled",
               "--drift", "refuse", "--arg", "--max-num-seqs", "--arg", "8"]).unwrap(),
        Command::EngineAdd {
            path: Some("/v".into()),
            name: Some("vllm-patched".into()),
            deep_park: Some(DeepParkChoice::Disabled),
            drift: DriftChoice::Refuse,
            args: vec!["--max-num-seqs".into(), "8".into()],
        }
    );
    assert_eq!(
        parse(["mllm", "engine", "add"]).unwrap(),
        Command::EngineAdd { path: None, name: None, deep_park: None, drift: DriftChoice::Warn, args: vec![] }
    );
    assert_eq!(parse(["mllm", "engine", "detect", "--path", "/a", "--path", "/b"]).unwrap(),
        Command::EngineDetect { paths: vec!["/a".into(), "/b".into()] });
    assert_eq!(parse(["mllm", "engine", "list"]).unwrap(), Command::EngineList);
    assert_eq!(parse(["mllm", "engine", "remove", "vllm", "--drain"]).unwrap(),
        Command::EngineRemove { name: "vllm".into(), drain: true });
    assert_eq!(parse(["mllm", "list", "engines"]).unwrap(), Command::List { resource: ListResource::Engines });
    assert!(parse(["mllm", "engine", "add", "--deep-park", "maybe"]).is_err());
    assert!(parse(["mllm", "engine", "remove"]).is_err());
    assert_eq!(parse(["mllm", "engine", "remove", "vllm"]).unwrap().label(), "engine remove vllm");
    assert_eq!(parse(["mllm", "list", "engines"]).unwrap().label(), "list engines");
}
```

Append to `crates/mllm-cli/tests/errors.rs`:

```rust
// T01 (ADR 0018 §6): the engine codes exit 16 to 23; 9 stays unused.
#[test]
fn engine_codes_have_their_exit_codes() {
    for (code, exit) in [
        ("engine_not_found", 16), ("engine_unsupported", 17), ("engine_version_failed", 18),
        ("profile_exists", 19), ("profile_in_use", 20), ("publish_rejected", 21),
        ("agent_unreachable", 22), ("not_interactive", 23),
    ] {
        let error = StructuredError { code, message: String::new() };
        assert_eq!(error.exit_code(), ExitCode(exit), "{code}");
        assert_ne!(error.exit_code(), ExitCode(9));
    }
}
```

and add `16, 17, 18, 19, 20, 21, 22, 23` to the `assert_ne!` list in `a_start_with_no_eligible_host_exits_15`.

- [ ] **Step 2: Run them to verify they fail.**

Run: `cargo test -p mllm-cli --test grammar --test errors --locked`
Expected: FAIL to compile (`EngineAdd`, `DeepParkChoice`, `ListResource::Engines` not found).

- [ ] **Step 3: Grammar.** In `grammar.rs`:

```rust
/// ADR 0018 §1: `--deep-park` on `engine add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DeepParkChoice { Enabled, Disabled }

/// ADR 0018 §1: `--drift` on `engine add` (ADR 0008 `installation_drift`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum DriftChoice { #[default] Warn, Refuse }
```

`ListResource` gains `Engines`; `Command` gains:

```rust
    /// ADR 0018 §1: installations found on this machine; executes nothing.
    EngineDetect { paths: Vec<PathBuf> },
    /// ADR 0018 §1: register an installation as a runtime profile.
    EngineAdd { path: Option<PathBuf>, name: Option<String>, deep_park: Option<DeepParkChoice>, drift: DriftChoice, args: Vec<String> },
    /// ADR 0018 §1: this machine's runtime profiles.
    EngineList,
    /// ADR 0018 §4: remove a runtime profile, stopping its deployments with `drain`.
    EngineRemove { name: String, drain: bool },
```

`label()` arms: `EngineDetect { .. } => "engine detect".into()`, `EngineAdd { .. } => "engine add".into()`, `EngineList => "engine list".into()`, `EngineRemove { name, .. } => format!("engine remove {name}")`; the existing `List` arm already lowercases `Engines` to `list engines`.

`CliCommand` gains `Engine { #[command(subcommand)] action: EngineArgs }` and:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum EngineArgs {
    /// List vLLM and SGLang installations on this machine (reads metadata only).
    Detect {
        /// Also scan this directory (repeatable).
        #[arg(long = "path")]
        paths: Vec<PathBuf>,
    },
    /// Register an installation as a runtime profile and publish it.
    Add {
        /// A venv directory, its bin/vllm, or its bin/python3. Omit to pick interactively.
        path: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        deep_park: Option<DeepParkChoice>,
        #[arg(long, value_enum, default_value_t = DriftChoice::Warn)]
        drift: DriftChoice,
        /// A host-fixed engine argument (repeatable).
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// This machine's runtime profiles and whether the server accepted them.
    List,
    /// Remove a runtime profile.
    Remove {
        name: String,
        /// Stop the deployments on this machine that use it first.
        #[arg(long)]
        drain: bool,
    },
}
```

`ListArgs` gains `Engines`; `From<CliCommand>` maps `ListArgs::Engines => Command::List { resource: ListResource::Engines }` and:

```rust
            CliCommand::Engine { action } => match action {
                EngineArgs::Detect { paths } => Command::EngineDetect { paths },
                EngineArgs::Add { path, name, deep_park, drift, args } => Command::EngineAdd { path, name, deep_park, drift, args },
                EngineArgs::List => Command::EngineList,
                EngineArgs::Remove { name, drain } => Command::EngineRemove { name, drain },
            },
```

- [ ] **Step 4: Exit codes.** In `output.rs` `impl ExitCode`, after `HOST_INELIGIBLE`:

```rust
    /// ADR 0018 §6: engine registration's closed codes (9 stays unused).
    pub const ENGINE_NOT_FOUND: Self = Self(16);
    pub const ENGINE_UNSUPPORTED: Self = Self(17);
    pub const ENGINE_VERSION_FAILED: Self = Self(18);
    pub const PROFILE_EXISTS: Self = Self(19);
    pub const PROFILE_IN_USE: Self = Self(20);
    pub const PUBLISH_REJECTED: Self = Self(21);
    pub const AGENT_UNREACHABLE: Self = Self(22);
    pub const NOT_INTERACTIVE: Self = Self(23);
```

and in `StructuredError::exit_code` before the `_` arm:

```rust
            "engine_not_found" => ExitCode::ENGINE_NOT_FOUND,
            "engine_unsupported" => ExitCode::ENGINE_UNSUPPORTED,
            "engine_version_failed" => ExitCode::ENGINE_VERSION_FAILED,
            "profile_exists" => ExitCode::PROFILE_EXISTS,
            "profile_in_use" => ExitCode::PROFILE_IN_USE,
            "publish_rejected" => ExitCode::PUBLISH_REJECTED,
            "agent_unreachable" => ExitCode::AGENT_UNREACHABLE,
            "not_interactive" => ExitCode::NOT_INTERACTIVE,
```

In `docs/operations/install.md`'s exit-code table add one row per code: `16 engine_not_found`, `17 engine_unsupported`, `18 engine_version_failed`, `19 profile_exists`, `20 profile_in_use`, `21 publish_rejected`, `22 agent_unreachable`, `23 not_interactive`, each with the remedy from the spec's Errors table.

Run: `cargo test -p mllm-cli --test grammar --test errors --locked`
Expected: pass (the `From` match is exhaustive, so compile errors point at any missed arm).

- [ ] **Step 5: Write the failing command tests.** Create `crates/mllm-cli/tests/engine_cli.rs`:

```rust
//! ADR 0018: `mllm engine` against fake environments and a scripted role
//! socket. CPU tests only; they are not qualification.
use mllm_agent::control_socket::{ControlHandler, ControlRequest, ControlServer};
use mllm_cli::engine::{execute, resolve_target};
use mllm_cli::grammar::{Command, DeepParkChoice, DriftChoice};
use mllm_config::registration::{engines_beside, EnginesFile};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A vLLM venv: `vllm --version` prints `reported`; the interpreter answers
/// the capability probe with `deep_park_missing` labels.
fn vllm_env(root: &Path, version: &str, reported: &str, deep_park_missing: &[&str]) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("vllm")).unwrap();
    std::fs::create_dir_all(site.join(format!("vllm-{version}.dist-info"))).unwrap();
    std::fs::write(site.join(format!("vllm-{version}.dist-info/METADATA")), format!("Name: vllm\nVersion: {version}\n")).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(&root.join("bin/vllm"), &format!("echo {reported}"));
    let report = json!({"schema": "mllm/engine-capabilities/v1", "engine": "vllm",
        "capabilities": {"core": [], "deep_park": deep_park_missing, "metrics": []}});
    script(&root.join("bin/python3"), &format!("echo '{report}'"));
    root.to_path_buf()
}

/// A host document whose state directory is private and short.
fn host_doc(dir: &Path) -> PathBuf {
    let state = dir.join("s");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("host.yaml");
    std::fs::write(&path, mllm_config::remote_roles::HostConfig::template(&state)).unwrap();
    path
}

fn engines_of(document: &Path) -> EnginesFile {
    EnginesFile::load(&engines_beside(document)).unwrap()
}

struct Role(Mutex<Vec<ControlRequest>>, Value);
#[async_trait::async_trait]
impl ControlHandler for Role {
    async fn handle(&self, request: ControlRequest) -> Value {
        self.0.lock().unwrap().push(request);
        self.1.clone()
    }
}

async fn role(document: &Path, reply: Value) -> (Arc<Role>, tokio::sync::watch::Sender<bool>) {
    let target = resolve_target(Some(document), Path::new("/nonexistent"), &|k| (k == "HOME").then(|| "/home/u".into())).unwrap();
    let server = ControlServer::bind(&target.socket).unwrap();
    let handler = Arc::new(Role(Mutex::new(vec![]), reply));
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(server.serve(handler.clone(), unsafe { libc::geteuid() }, shutdown));
    (handler, stop)
}

fn add(path: &Path) -> Command {
    Command::EngineAdd { path: Some(path.into()), name: None, deep_park: None, drift: DriftChoice::Warn, args: vec![] }
}

// T07 (ADR 0018 §1–§3): add writes the profile into engines.yaml beside the
// host document, which is never touched, and asks the role to publish it.
#[tokio::test]
async fn add_writes_the_profile_and_publishes() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let host_before = std::fs::read(&document).unwrap();
    let (role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    let out = execute(&add(&env), Some(&document), dir.path()).await.unwrap();
    assert_eq!(out["published"], "published");
    assert_eq!(out["custom"], false);
    let engines = engines_of(&document);
    assert_eq!(engines.revision, 1);
    let profile = &engines.profiles["vllm"];
    assert_eq!(profile["executable"], env.join("bin/vllm").to_string_lossy().as_ref());
    assert_eq!(profile["build_fingerprint"], "0.29.0");
    assert_eq!(profile["security"]["deep_park"], "enabled");
    assert_eq!(std::fs::read(&document).unwrap(), host_before, "host.yaml is never rewritten");
    assert_eq!(*role.0.lock().unwrap(), vec![ControlRequest::Add]);
}

// T03 (owner decision 2026-09-25): a name registered already, or declared in
// the host document, is refused and nothing is written.
#[tokio::test]
async fn add_refuses_an_existing_name() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    execute(&add(&env), Some(&document), dir.path()).await.unwrap();
    let before = std::fs::read(engines_beside(&document)).unwrap();
    let error = execute(&add(&env), Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "profile_exists");
    assert_eq!(std::fs::read(engines_beside(&document)).unwrap(), before);
    let mut host: Value = serde_json::from_str(&std::fs::read_to_string(&document).unwrap()).unwrap();
    host["runtime_profiles"]["sg"] = engines_of(&document).profiles["vllm"].clone();
    std::fs::write(&document, host.to_string()).unwrap();
    let named = Command::EngineAdd { path: Some(env.clone()), name: Some("sg".into()), deep_park: None, drift: DriftChoice::Warn, args: vec![] };
    assert_eq!(execute(&named, Some(&document), dir.path()).await.unwrap_err().code, "profile_exists");
}

// ADR 0018 §3: a refused publication is reported; the profile stays written.
#[tokio::test]
async fn add_reports_a_rejected_publication() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": false, "code": "publish_rejected", "message": "no"})).await;
    let error = execute(&add(&env), Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "publish_rejected");
    assert!(engines_of(&document).profiles.contains_key("vllm"));
}

// ADR 0018 §3: without a running role engines.yaml is written and the
// command says the profile takes effect at the next start.
#[tokio::test]
async fn add_without_a_running_role_is_agent_unreachable() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let error = execute(&add(&env), Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "agent_unreachable");
    assert!(error.message.contains("revision 1"), "{}", error.message);
    assert!(engines_of(&document).profiles.contains_key("vllm"));
}

// T34 (ADR 0017 fallback): a peer without live_profile_update means restart.
#[tokio::test]
async fn add_restart_required_is_success_with_notice() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "restart_required"})).await;
    let out = execute(&add(&env), Some(&document), dir.path()).await.unwrap();
    assert_eq!(out["published"], "restart_required");
}

// T37: a version check that disagrees with the metadata writes nothing.
#[tokio::test]
async fn add_version_mismatch_writes_nothing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.28.0", &[]);
    let document = host_doc(dir.path());
    let error = execute(&add(&env), Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "engine_version_failed");
    assert!(!engines_beside(&document).exists());
}

// T21 (owner decision 2026-09-25): a probe that reports deep park missing
// writes it disabled, unless the operator asked for enabled.
#[tokio::test]
async fn add_disables_deep_park_when_the_probe_reports_it_missing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &["sleep_mode"]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    let out = execute(&add(&env), Some(&document), dir.path()).await.unwrap();
    assert_eq!(out["deep_park_probe"], "capability_missing");
    assert_eq!(engines_of(&document).profiles["vllm"]["security"]["deep_park"], "disabled");
    let asked = Command::EngineAdd { path: Some(env.clone()), name: Some("vllm-deep".into()),
        deep_park: Some(DeepParkChoice::Enabled), drift: DriftChoice::Warn, args: vec![] };
    execute(&asked, Some(&document), dir.path()).await.unwrap();
    assert_eq!(engines_of(&document).profiles["vllm-deep"]["security"]["deep_park"], "enabled");
}

// T01: add without a path needs a terminal (tests run without one).
#[tokio::test]
async fn add_without_a_path_needs_a_terminal() {
    let dir = private_dir();
    let document = host_doc(dir.path());
    let command = Command::EngineAdd { path: None, name: None, deep_park: None, drift: DriftChoice::Warn, args: vec![] };
    assert_eq!(execute(&command, Some(&document), dir.path()).await.unwrap_err().code, "not_interactive");
}

// T37: detect lists the fake environment and runs nothing in it.
#[tokio::test]
async fn detect_lists_candidates_and_runs_nothing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("envs/v"), "0.29.0", "0.29.0", &[]);
    let marker = dir.path().join("ran");
    script(&env.join("bin/vllm"), &format!("touch {}", marker.display()));
    let out = execute(&Command::EngineDetect { paths: vec![dir.path().join("envs")] }, None, dir.path()).await.unwrap();
    let found = out["candidates"].as_array().unwrap();
    assert!(found.iter().any(|c| c["env"] == env.to_string_lossy().as_ref() && c["engine"] == "vllm"), "{out}");
    assert!(!marker.exists());
}

// Owner decision 2026-09-25: which role document and which engines file.
#[test]
fn target_resolution_follows_the_documented_order() {
    let dir = private_dir();
    let explicit = host_doc(dir.path());
    let config_home = dir.path().join("cfg");
    std::fs::create_dir_all(config_home.join("mllm")).unwrap();
    std::fs::copy(&explicit, config_home.join("mllm/host.yaml")).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(state.join("config")).unwrap();
    std::fs::write(state.join("config/standalone.yaml"), "schema_version: 1\nkind: standalone\nname: local\n").unwrap();
    let cfg = config_home.to_string_lossy().into_owned();
    let env = |key: &str| (key == "XDG_CONFIG_HOME").then(|| cfg.clone());
    // --config dir/x.yaml → dir/engines.yaml.
    let t = resolve_target(Some(&explicit), &state, &env).unwrap();
    assert_eq!((t.role_document.clone(), t.engines.clone()), (explicit.clone(), dir.path().join("engines.yaml")));
    // Both implicit documents: ambiguous.
    assert_eq!(resolve_target(None, &state, &env).unwrap_err().code, "invalid_config");
    // Implicit standalone: its document in the state dir, engines in the config home.
    std::fs::remove_file(config_home.join("mllm/host.yaml")).unwrap();
    let t = resolve_target(None, &state, &env).unwrap();
    assert_eq!(t.kind, mllm_cli::engine::RoleKind::Standalone);
    assert_eq!(t.engines, config_home.join("mllm/engines.yaml"));
    // Implicit host: the same engines file.
    std::fs::remove_file(state.join("config/standalone.yaml")).unwrap();
    std::fs::copy(&explicit, config_home.join("mllm/host.yaml")).unwrap();
    let t = resolve_target(None, &state, &env).unwrap();
    assert_eq!((t.kind, t.engines), (mllm_cli::engine::RoleKind::Host, config_home.join("mllm/engines.yaml")));
    // $MLLM_CONFIG counts as naming the document.
    let path = explicit.to_string_lossy().into_owned();
    let named = |key: &str| match key { "MLLM_CONFIG" => Some(path.clone()), "XDG_CONFIG_HOME" => Some(cfg.clone()), _ => None };
    assert_eq!(resolve_target(None, &state, &named).unwrap().engines, dir.path().join("engines.yaml"));
}

// T16 T32: removal in use is refused with the list, nothing written.
#[tokio::test]
async fn remove_in_use_is_refused_with_the_list() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    {
        let (_role, stop) = role(&document, json!({"ok": true, "published": "published"})).await;
        execute(&add(&env), Some(&document), dir.path()).await.unwrap();
        stop.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (_role, _stop) = role(&document, json!({"ok": false, "code": "profile_in_use", "deployments": ["q14"], "message": "in use"})).await;
    let error = execute(&Command::EngineRemove { name: "vllm".into(), drain: false }, Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "profile_in_use");
    assert!(error.message.contains("q14"), "{}", error.message);
}

// Owner decision 2026-09-25: without a role nothing is removed; a profile the
// operator declared in host.yaml is never removed by mllm.
#[tokio::test]
async fn remove_without_a_role_writes_nothing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let error = execute(&Command::EngineRemove { name: "vllm".into(), drain: true }, Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "invalid_config", "no such profile");
    let _ = execute(&add(&env), Some(&document), dir.path()).await;
    let before = std::fs::read(engines_beside(&document)).unwrap();
    let error = execute(&Command::EngineRemove { name: "vllm".into(), drain: true }, Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "agent_unreachable");
    assert_eq!(std::fs::read(engines_beside(&document)).unwrap(), before);
    let mut host: Value = serde_json::from_str(&std::fs::read_to_string(&document).unwrap()).unwrap();
    host["runtime_profiles"]["theirs"] = engines_of(&document).profiles["vllm"].clone();
    std::fs::write(&document, host.to_string()).unwrap();
    let error = execute(&Command::EngineRemove { name: "theirs".into(), drain: false }, Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "invalid_config");
    assert!(error.message.contains("edit that file"), "{}", error.message);
}

// ADR 0018 §1: list shows registered and declared profiles with what the
// role accepted.
#[tokio::test]
async fn list_merges_the_files_and_the_role() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_r, stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    execute(&add(&env), Some(&document), dir.path()).await.unwrap();
    stop.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let offline = execute(&Command::EngineList, Some(&document), dir.path()).await.unwrap();
    assert_eq!(offline["agent"], "unreachable");
    assert_eq!(offline["engines"][0]["published"], "unknown");
    assert_eq!(offline["engines"][0]["source"], "engines.yaml");
    let (_r, _stop) = role(&document, json!({"ok": true, "connected": true, "live_profile_update": true,
        "accepted": {}, "users": {}})).await;
    let listed = execute(&Command::EngineList, Some(&document), dir.path()).await.unwrap();
    assert_eq!(listed["engines"][0]["published"], "not published");
}

// T01: `list engines` is a server command.
#[test]
fn list_engines_goes_to_the_server() {
    assert!(mllm_cli::remote_roles::supports(&Command::List { resource: mllm_cli::grammar::ListResource::Engines }));
}
```

(`libc`, `async-trait`, `tokio`, `tempfile` and `mllm-agent` are dependencies of `mllm-cli` already; add them to `[dev-dependencies]` only if the test build asks. `ControlRequest` needs `PartialEq`, which Task 12 derives.)

- [ ] **Step 6: Run them to verify they fail.**

Run: `cargo test -p mllm-cli --test engine_cli --locked`
Expected: FAIL to compile (`mllm_cli::engine` not found).

- [ ] **Step 7: Implement the target.** Create `crates/mllm-cli/src/engine/target.rs`:

```rust
//! ADR 0018 §2 (owner decision 2026-09-25): which role document, engines
//! file and role socket an `mllm engine` command acts on. The engines file
//! sits beside the role document named with `--config` (or `$MLLM_CONFIG`);
//! otherwise it is `<config home>/mllm/engines.yaml`, for a host and for
//! standalone alike, which is where the role looks too.
use crate::output::StructuredError;
use mllm_agent::control_socket::SOCKET_NAME;
use mllm_config::registration::{config_home, engines_path};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleKind {
    Host,
    Standalone,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub role_document: PathBuf,
    pub kind: RoleKind,
    pub engines: PathBuf,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
}

pub(crate) fn invalid(message: impl Into<String>) -> StructuredError {
    StructuredError { code: "invalid_config", message: message.into() }
}

/// The role document: `--config`; else `$MLLM_CONFIG`; else
/// `<config home>/mllm/host.yaml` if it exists; else
/// `<state_dir>/config/standalone.yaml` if it exists. Both implicit documents
/// present is ambiguous and refused.
pub fn resolve_target(explicit: Option<&Path>, state_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Target, StructuredError> {
    let home = config_home(env).ok_or_else(|| invalid("neither XDG_CONFIG_HOME nor HOME is set; pass --config"))?;
    let named = explicit.map(Path::to_path_buf).or_else(|| env("MLLM_CONFIG").map(PathBuf::from));
    let chosen = match &named {
        Some(path) => path.clone(),
        None => {
            let host = Some(home.join("mllm/host.yaml")).filter(|p| p.exists());
            let standalone = Some(state_dir.join("config/standalone.yaml")).filter(|p| p.exists());
            match (host, standalone) {
                (Some(host), Some(standalone)) => {
                    return Err(invalid(format!(
                        "both {} and {} exist; pass --config to choose",
                        host.display(),
                        standalone.display()
                    )))
                }
                (Some(host), None) => host,
                (None, Some(standalone)) => standalone,
                (None, None) => return Err(invalid("no host or standalone document found; pass --config")),
            }
        }
    };
    let text = std::fs::read_to_string(&chosen).map_err(|e| invalid(format!("{}: {e}", chosen.display())))?;
    let (kind, state) = match mllm_config::remote_roles::HostConfig::parse(&text) {
        Ok(host) => (RoleKind::Host, host.state_dir),
        Err(host_error) => match mllm_config::parse_strict(mllm_config::ConfigKind::Standalone, &text) {
            Ok(_) => (RoleKind::Standalone, state_dir.to_path_buf()),
            Err(_) => return Err(invalid(format!("{}: {}", chosen.display(), host_error.detail))),
        },
    };
    Ok(Target {
        engines: engines_path(named.as_deref(), &home),
        role_document: chosen,
        kind,
        socket: state.join(SOCKET_NAME),
        state_dir: state,
    })
}
```

- [ ] **Step 8: Implement the commands.** Create `crates/mllm-cli/src/engine.rs`:

```rust
//! ADR 0018: `mllm engine detect|add|list|remove`, the same on a host and in
//! standalone. Detection reads metadata only; an installation runs only
//! after the operator named or picked it.
mod target;
pub use target::{resolve_target, RoleKind, Target};

use crate::grammar::{Command, DeepParkChoice, DriftChoice};
use crate::output::StructuredError;
use mllm_agent::control_socket::{request, ControlRequest};
use mllm_agent::engines::{check_version, detect, resolve, Resolved, ScanBounds, ScanRoots, VERSION_CHECK_TIMEOUT};
use mllm_config::effective::InstallationDrift;
use mllm_config::engine_policy::Engine;
use mllm_config::registration::{
    check_profile, lock_engines, profile_document, valid_profile_name, write_engines, EnginesFile, ProfileSpec,
    ENVIRONMENT_PROFILES,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ADD_REPLY: Duration = Duration::from_secs(60);
const REMOVE_REPLY: Duration = Duration::from_secs(990);
const LIST_REPLY: Duration = Duration::from_secs(5);

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError { code, message: message.into() }
}

/// A role's reply code as a closed CLI code.
fn closed(code: &str) -> &'static str {
    match code {
        "publish_rejected" => "publish_rejected",
        "profile_in_use" => "profile_in_use",
        "agent_unreachable" => "agent_unreachable",
        "profile_exists" => "profile_exists",
        "invalid_config" => "invalid_config",
        _ => "internal",
    }
}

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Vllm => "vllm",
        Engine::Sglang => "sglang",
    }
}

pub fn is_engine_command(command: &Command) -> bool {
    matches!(command, Command::EngineDetect { .. } | Command::EngineAdd { .. } | Command::EngineList | Command::EngineRemove { .. })
}

pub async fn execute(command: &Command, config: Option<&Path>, state_dir: &Path) -> Result<Value, StructuredError> {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    match command {
        Command::EngineDetect { paths } => Ok(detected(paths)),
        Command::EngineAdd { path, name, deep_park, drift, args } => {
            let target = resolve_target(config, state_dir, &env)?;
            add(&target, path.as_deref(), name.as_deref(), *deep_park, *drift, args).await
        }
        Command::EngineList => list(&resolve_target(config, state_dir, &env)?).await,
        Command::EngineRemove { name, drain } => remove(&resolve_target(config, state_dir, &env)?, name, *drain).await,
        _ => Err(error("invalid_config", "not an engine command")),
    }
}

fn candidates(paths: &[PathBuf]) -> Vec<mllm_agent::engines::Candidate> {
    detect(&ScanRoots::from_env(paths.to_vec()), &ScanBounds::default())
}

fn detected(paths: &[PathBuf]) -> Value {
    let rows: Vec<Value> = candidates(paths)
        .into_iter()
        .map(|c| json!({"engine": engine_name(c.engine), "version": c.version, "custom": c.custom,
            "env": c.env, "entry": c.entry, "source": c.source}))
        .collect();
    json!({"candidates": rows})
}

/// ADR 0018 §1: with no path, an operator at a terminal picks a candidate.
fn pick() -> Result<PathBuf, StructuredError> {
    use std::io::{BufRead, IsTerminal, Write};
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(error("not_interactive", "engine add without a path needs a terminal; name the installation"));
    }
    let found = candidates(&[]);
    if found.is_empty() {
        return Err(error("engine_not_found", "no installation found; name it, or use engine detect --path DIR"));
    }
    let mut stderr = std::io::stderr();
    for (i, c) in found.iter().enumerate() {
        let _ = writeln!(stderr, "{:>3}  {} {}{}  {}", i + 1, engine_name(c.engine), c.version, if c.custom { " (custom)" } else { "" }, c.env.display());
    }
    let _ = write!(stderr, "register which? ");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|_| error("not_interactive", "no answer"))?;
    let index: usize = line.trim().parse().map_err(|_| error("invalid_config", "not a number from the list"))?;
    found.get(index.wrapping_sub(1)).map(|c| c.entry.clone()).ok_or_else(|| error("invalid_config", "not a number from the list"))
}

struct Registration {
    version: String,
    fingerprint: Option<mllm_agent::installation::InstallationFingerprint>,
    deep_park_missing: Option<bool>,
}

/// ADR 0018 §1 step 3: executed only now, because the operator named it.
fn register(resolved: &Resolved, state_dir: &Path) -> Result<Registration, StructuredError> {
    let version = check_version(resolved, VERSION_CHECK_TIMEOUT)
        .map_err(|e| error("engine_version_failed", format!("{}: {e}; nothing was written", resolved.executable.display())))?;
    let fingerprint = mllm_agent::installation::InstallationMeasurer::new().measure(resolved.engine, &resolved.executable).ok();
    let scratch = tempfile::tempdir_in(state_dir).map_err(|e| error("internal", format!("probe directory: {e}")))?;
    let runtime = scratch.path().join("runtime");
    crate::managed_runtime::prepare_for_role(&runtime)?;
    let report = mllm_agent::installation::probe_capabilities(
        resolved.engine,
        &resolved.executable,
        &runtime,
        mllm_agent::installation::PROBE_TIMEOUT,
    );
    Ok(Registration { version, fingerprint, deep_park_missing: report.and_then(|r| r.available("deep_park")).map(|a| !a) })
}

/// The role document, strictly parsed (never written).
fn role_document(target: &Target) -> Result<Value, StructuredError> {
    let text = std::fs::read_to_string(&target.role_document)
        .map_err(|e| error("invalid_config", format!("{}: {e}", target.role_document.display())))?;
    let kind = match target.kind {
        RoleKind::Host => mllm_config::ConfigKind::Host,
        RoleKind::Standalone => mllm_config::ConfigKind::Standalone,
    };
    mllm_config::parse_strict(kind, &text).map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))
}

/// Profiles the operator declared in the role document itself.
fn declared_by_operator(target: &Target) -> Result<serde_json::Map<String, Value>, StructuredError> {
    let document = role_document(target)?;
    let profiles = match target.kind {
        RoleKind::Host => &document["runtime_profiles"],
        RoleKind::Standalone => &document["host"]["runtime_profiles"],
    };
    Ok(profiles.as_object().cloned().unwrap_or_default())
}

/// ADR 0018 §2: lock engines.yaml, re-check the name against both files,
/// validate, write. The role document is never written. The revision written.
fn write_profile(target: &Target, name: &str, spec: &ProfileSpec) -> Result<u64, StructuredError> {
    let lock = lock_engines(&target.engines).map_err(|e| error("internal", e.detail))?;
    let mut engines = EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    if engines.profiles.contains_key(name) || declared_by_operator(target)?.contains_key(name) {
        return Err(error("profile_exists", format!("profile {name} exists; use --name, or remove it first")));
    }
    let profile = profile_document(spec);
    check_profile(name, &profile).map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))?;
    engines.profiles.insert(name.to_owned(), profile);
    let host = match target.kind {
        RoleKind::Host => Some(role_document(target)?),
        RoleKind::Standalone => None,
    };
    write_engines(&engines, &lock, host.as_ref()).map_err(|e| error("invalid_config", e.detail))
}

async fn add(
    target: &Target,
    path: Option<&Path>,
    name: Option<&str>,
    deep_park: Option<DeepParkChoice>,
    drift: DriftChoice,
    args: &[String],
) -> Result<Value, StructuredError> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => pick()?,
    };
    let resolved = resolve(&path).map_err(|e| error(e.code(), e.to_string()))?;
    let name = name.unwrap_or(engine_name(resolved.engine)).to_owned();
    if !valid_profile_name(&name) {
        return Err(error("invalid_config", format!("profile name {name:?} must be lowercase letters, digits, '-' or '_'")));
    }
    if target.kind == RoleKind::Standalone && ENVIRONMENT_PROFILES.contains(&name.as_str()) {
        return Err(error("profile_exists", format!("{name} is reserved for the MLLM_VLLM_BIN / MLLM_SGLANG_BIN installation; use --name")));
    }
    // Checked before anything runs, and again under the lock when writing.
    let existing = EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    if existing.profiles.contains_key(&name) || declared_by_operator(target)?.contains_key(&name) {
        return Err(error("profile_exists", format!("profile {name} exists; use --name, or remove it first")));
    }
    let (r, state) = (resolved.clone(), target.state_dir.clone());
    let registration = tokio::task::spawn_blocking(move || register(&r, &state))
        .await
        .map_err(|_| error("internal", "registration failed"))??;
    let probe = match registration.deep_park_missing {
        Some(true) => "capability_missing",
        Some(false) => "available",
        None => "unknown",
    };
    // Owner decision 2026-09-25: missing deep park is recorded as disabled unless asked.
    let deep = match deep_park {
        Some(DeepParkChoice::Enabled) => true,
        Some(DeepParkChoice::Disabled) => false,
        None => registration.deep_park_missing != Some(true),
    };
    let spec = ProfileSpec {
        engine: resolved.engine,
        executable: resolved.executable.clone(),
        build_fingerprint: registration.version.clone(),
        deep_park: deep,
        installation_drift: match drift {
            DriftChoice::Warn => InstallationDrift::Warn,
            DriftChoice::Refuse => InstallationDrift::Refuse,
        },
        args: args.to_vec(),
    };
    let revision = write_profile(target, &name, &spec)?;
    let mut out = json!({
        "profile": name, "engine": engine_name(resolved.engine), "version": registration.version,
        "custom": resolved.custom(), "executable": resolved.executable,
        "fingerprint": registration.fingerprint.map(|f| json!({"version": f.version, "digest": f.digest})),
        "deep_park": if deep { "enabled" } else { "disabled" }, "deep_park_probe": probe,
        "engines_file": target.engines, "revision": revision,
    });
    match request(&target.socket, &ControlRequest::Add, ADD_REPLY).await {
        Ok(reply) if reply["ok"] == true => {
            out["published"] = reply["published"].clone();
            Ok(out)
        }
        Ok(reply) => Err(error(
            closed(reply["code"].as_str().unwrap_or("")),
            format!("{} (the profile is written at revision {revision} and shows as not published)", reply["message"].as_str().unwrap_or("refused")),
        )),
        Err(e) => Err(error(
            "agent_unreachable",
            format!("{e}; {} is written (revision {revision}); it takes effect when the role starts", target.engines.display()),
        )),
    }
}

async fn list(target: &Target) -> Result<Value, StructuredError> {
    let engines = EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    let role = request(&target.socket, &ControlRequest::List, LIST_REPLY).await.ok().filter(|r| r["ok"] == true);
    // ADR 0018 §2: registered profiles, then the ones the operator declared.
    let mut all: Vec<(String, Value, &'static str)> =
        engines.profiles.clone().into_iter().map(|(n, p)| (n, p, "engines.yaml")).collect();
    all.extend(declared_by_operator(target)?.into_iter().map(|(n, p)| (n, p, "role document")));
    let rows: Vec<Value> = all
        .into_iter()
        .map(|(name, profile, source)| {
            let engine = match profile["engine"].as_str() { Some("sglang") => Engine::Sglang, _ => Engine::Vllm };
            let accepted = role.as_ref().map(|r| r["accepted"].get(&name).cloned());
            let version = accepted
                .clone()
                .flatten()
                .and_then(|a| a["installation"]["version"].as_str().map(str::to_owned))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| profile["build_fingerprint"].as_str().unwrap_or("unknown").to_owned());
            json!({
                "profile": name, "source": source, "engine": profile["engine"], "version": version,
                "custom": !mllm_config::registration::is_verified(engine, &version),
                "executable": profile["executable"],
                "fingerprint": accepted.clone().flatten().map(|a| a["installation"].clone()),
                "deep_park": profile["security"]["deep_park"],
                "deep_park_probe": accepted.clone().flatten().map(|a| a["deep_park_probe"].clone()).unwrap_or(json!("unknown")),
                "published": match &accepted { None => "unknown", Some(Some(_)) => "published", Some(None) => "not published" },
                "deployments": role.as_ref().map(|r| r["users"][&name].clone()).unwrap_or(json!([])),
            })
        })
        .collect();
    Ok(json!({"engines_file": target.engines, "revision": engines.revision,
        "agent": if role.is_some() { "reachable" } else { "unreachable" }, "engines": rows}))
}

async fn remove(target: &Target, name: &str, drain: bool) -> Result<Value, StructuredError> {
    if target.kind == RoleKind::Standalone && ENVIRONMENT_PROFILES.contains(&name) {
        return Err(error("invalid_config", format!(
            "{name} comes from MLLM_VLLM_BIN / MLLM_SGLANG_BIN; unset the variable and restart the role instead"
        )));
    }
    let engines = EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    if !engines.profiles.contains_key(name) {
        // ADR 0018 §2: a profile declared in the role document is the operator's.
        return Err(error("invalid_config", if declared_by_operator(target)?.contains_key(name) {
            format!("{name} is declared in {}; edit that file and restart the role", target.role_document.display())
        } else {
            format!("{} has no profile named {name}", target.engines.display())
        }));
    }
    match request(&target.socket, &ControlRequest::Remove { profile: name.into(), drain }, REMOVE_REPLY).await {
        Ok(reply) if reply["ok"] == true => Ok(reply),
        Ok(reply) => {
            let mut message = reply["message"].as_str().unwrap_or("refused").to_owned();
            if let Some(names) = reply["deployments"].as_array().filter(|n| !n.is_empty()) {
                let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                message = format!("{message}: {}", names.join(", "));
            }
            Err(error(closed(reply["code"].as_str().unwrap_or("")), message))
        }
        // Owner decision 2026-09-25: a published profile is never removed unconfirmed.
        Err(e) => Err(error("agent_unreachable", format!("{e}; nothing was removed; start the role and retry"))),
    }
}
```

Add `pub mod engine;` to `lib.rs`. In `main.rs`, beside the `Drain` branch:

```rust
    // ADR 0018: engine registration, on this machine, through its role's socket.
    if mllm_cli::engine::is_engine_command(&invocation.command) {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(_) => return ExitCode::from(output::ExitCode::INTERNAL.0 as u8),
        };
        return match runtime.block_on(mllm_cli::engine::execute(&invocation.command, invocation.config.as_deref(), &default_state_dir())) {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                output::print_error(&err, format);
                ExitCode::from(err.exit_code().0 as u8)
            }
        };
    }
```

In `remote_roles.rs::supports` add `| Command::List { resource: ListResource::Engines }`, and beside the `ListResource::Hosts` arm of `execute`:

```rust
        Command::List { resource: ListResource::Engines } => {
            // ADR 0018: every host's published profiles, from the server.
            let config = server_context(invocation.config.as_deref(), root)?;
            management_request(&config, reqwest::Method::GET, "/engines", None).await
        }
```

- [ ] **Step 9: Run the tests.**

Run: `cargo test -p mllm-cli --test engine_cli --test grammar --test errors --locked`
Expected: all pass.

- [ ] **Step 10: Core suite, workspace, Clippy.** Run the three Global Constraints commands. Expected: pass.

- [ ] **Step 11: Commit**

```bash
git add crates/mllm-cli docs/operations/install.md
git commit -m "feat(cli): mllm engine detect, add, list, remove and list engines

ADR 0018: engine commands on a host or in standalone. Detect reads
metadata only; add resolves, checks the version, measures and probes the
named installation, writes the profile under the document lock and asks
the running role to publish it; remove relays to the role, which removes
a published profile only after the server confirms. Closed codes exit
16 to 23; 9 stays unused."
```

---

### Task 15: Standalone: environment compatibility, several engines, live add and remove

One deliverable: standalone runs the same registration steps in one process. Both engine variables give two profiles; profiles registered in its `engines.yaml` (beside `--config`, else `<config home>/mllm/engines.yaml`) coexist with them; the same socket adds and removes live. The standalone document itself is never written.

**Files:**
- Modify: `crates/mllm-controller/src/engine_provider.rs:26-80` (`NamedInstallation`, `ProviderError::ProfileExists`, `EngineProvider::installations`)
- Modify: `crates/mllm-controller/src/installation_gate.rs:15-260` (`EmbeddedInstallations`; `InstalledBindings` and `InstallationGate` pick by executable)
- Modify: `crates/mllm-cli/src/roles.rs` (`EnvEngineProvider::installation` `:425-497` split into `role_installation`; `installations`; `start_standalone_inner` `:729-1035`; `App` `:84-128` gains `host: Arc<crate::standalone_engines::EmbeddedHost>`; `StartError` maps `ProfileExists` to `profile_exists`)
- Modify: `crates/mllm-cli/src/standalone_config.rs` (`host_policy` `:54-137` takes `&[NamedInstallation]`; `deployment_document` `:168-227` takes `profile: &str`)
- Modify: `crates/mllm-management/src/configuration.rs:340-370` (`HostSource::Embedded { document: Arc<RwLock<Value>>, id }`, `SharedConfigurationSource::new_shared`)
- Modify: `crates/mllm-management/src/installation.rs:22` (view lists every installation)
- Create: `crates/mllm-cli/src/standalone_engines.rs`; modify `crates/mllm-cli/src/lib.rs`
- Modify: `crates/mllm-cli/src/standalone_config/tests.rs:42` and add env tests there; Create: `crates/mllm-cli/tests/standalone_engines.rs`

**Interfaces:**
- Consumes: Tasks 2–3 (`EnginesFile`, `engines_path`, `config_home`, `lock_engines`, `write_engines`, `ENVIRONMENT_PROFILES`), Task 7 store API, Task 11 `StoreRetirements`, Task 12 socket, Task 13 reply shapes, Task 14 CLI (unchanged: the socket is the contract).
- Produces:
  - `pub struct NamedInstallation { pub profile: String, pub installation: EngineInstallation }`
  - `EngineProvider::installations(&self, registered: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<NamedInstallation>, ProviderError>` with a default implementation
  - `ProviderError::ProfileExists(String)`
  - `pub struct EmbeddedInstallations` with `new() -> Arc<Self>`, `register(&self, profile: &str, engine: Engine, executable: &Path)`, `for_executable(&self, executable: &Path) -> Option<Arc<EmbeddedInstallation>>`, `views(&self) -> Vec<serde_json::Value>`
  - `standalone_config::host_policy(installations: &[NamedInstallation], environment_fingerprint: &str, capacity_bytes: i64, inventory: Option<&InventoryPublication>) -> Value`
  - `standalone_config::deployment_document(name, route, source, engine, capacity_bytes, request_deadline, deep_park, profile: &str) -> Value`
  - `SharedConfigurationSource::new_shared(state, host: Arc<RwLock<Value>>, principal: &str) -> Result<Self, ConfigurationFailure>`
  - `pub struct EmbeddedHost` and `pub struct StandaloneControl` (`impl ControlHandler`)
  - `App::profiles(&self) -> Vec<String>`

- [ ] **Step 1: Write the failing environment tests.** Append to `crates/mllm-cli/src/standalone_config/tests.rs` (they use its `ENVIRONMENT` mutex, `fake_engine_bin` and `private_runtime`):

```rust
fn engine_env(dir: &std::path::Path, vllm: bool, sglang: bool) {
    let models = dir.join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::env::set_var("MLLM_MODELS_ROOT", &models);
    std::env::set_var("MLLM_RUNTIME_DIR", private_runtime(dir));
    std::env::set_var("MLLM_ENGINE_FINGERPRINT", "0.29.0");
    let bin = fake_engine_bin(dir);
    if vllm { std::env::set_var("MLLM_VLLM_BIN", &bin) } else { std::env::remove_var("MLLM_VLLM_BIN") }
    if sglang { std::env::set_var("MLLM_SGLANG_BIN", bin.with_file_name("python3")) } else { std::env::remove_var("MLLM_SGLANG_BIN") }
    for name in ["MLLM_KV_CACHE_BYTES", "MLLM_ENGINE_ARGS", "MLLM_DEEP_PARK", "MLLM_TRUST_REMOTE_CODE"] {
        std::env::remove_var(name);
    }
}

fn names(found: &[mllm_controller::engine_provider::NamedInstallation]) -> Vec<&str> {
    found.iter().map(|n| n.profile.as_str()).collect()
}

fn registered(executable: &std::path::Path) -> serde_json::Map<String, serde_json::Value> {
    let profile = mllm_config::registration::profile_document(&mllm_config::registration::ProfileSpec {
        engine: Engine::Vllm,
        executable: executable.into(),
        build_fingerprint: "0.29.0".into(),
        deep_park: true,
        installation_drift: mllm_config::effective::InstallationDrift::Warn,
        args: vec![],
    });
    [("vllm-patched".to_string(), profile)].into_iter().collect()
}

// ADR 0018 §5: one variable gives `local`, exactly as before.
#[test]
fn one_variable_gives_local() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    engine_env(dir.path(), true, false);
    let found = crate::roles::EnvEngineProvider::new().installations(&Default::default()).unwrap();
    assert_eq!(names(&found), vec!["local"]);
}

// ADR 0018 §5: both variables (refused before) give two profiles.
#[test]
fn both_variables_give_local_vllm_and_local_sglang() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    engine_env(dir.path(), true, true);
    let found = crate::roles::EnvEngineProvider::new().installations(&Default::default()).unwrap();
    assert_eq!(names(&found), vec!["local-vllm", "local-sglang"]);
    assert_eq!(found[1].installation.engine, Engine::Sglang);
}

// ADR 0018 §5: registered profiles coexist; a collision is profile_exists;
// a registered profile alone is enough.
#[test]
fn registered_profiles_coexist_and_collide_by_name() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    engine_env(dir.path(), true, false);
    let bin = dir.path().join("venv/bin/vllm");
    let found = crate::roles::EnvEngineProvider::new().installations(&registered(&bin)).unwrap();
    assert_eq!(names(&found), vec!["local", "vllm-patched"]);
    let mut clash = registered(&bin);
    clash.insert("local".into(), clash["vllm-patched"].clone());
    assert!(matches!(
        crate::roles::EnvEngineProvider::new().installations(&clash),
        Err(mllm_controller::engine_provider::ProviderError::ProfileExists(name)) if name == "local"
    ));
    engine_env(dir.path(), false, false);
    let alone = crate::roles::EnvEngineProvider::new().installations(&registered(&bin)).unwrap();
    assert_eq!(names(&alone), vec!["vllm-patched"]);
    assert!(crate::roles::EnvEngineProvider::new().installations(&Default::default()).is_err());
}
```

Change `the_host_declares_exactly_one_engine_installation` (`:42`) to build `host_policy(&[NamedInstallation { profile: "local".into(), installation }], ..)` and keep its assertion of one profile, named `local`.

- [ ] **Step 2: Run them to verify they fail.**

Run: `cargo test -p mllm-cli --lib standalone_config --locked`
Expected: FAIL to compile (`installations`, `NamedInstallation` not found).

- [ ] **Step 3: Provider contract.** In `engine_provider.rs`:

```rust
/// ADR 0018 §5: one engine installation under the profile name it publishes.
#[derive(Debug, Clone)]
pub struct NamedInstallation {
    pub profile: String,
    pub installation: EngineInstallation,
}
```

`ProviderError` gains `#[error("profile {0} is declared twice (an environment variable and engines.yaml); rename the registered one")] ProfileExists(String)`. `EngineProvider` gains:

```rust
    /// ADR 0018 §5: every installation this host publishes: the environment's
    /// (`local`) and the profiles registered in engines.yaml, which reuse its role
    /// settings. A name declared twice is refused.
    fn installations(&self, registered: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<NamedInstallation>, ProviderError> {
        let base = self.installation()?;
        let mut all = vec![NamedInstallation { profile: "local".into(), installation: base.clone() }];
        for (name, profile) in registered {
            if all.iter().any(|n| &n.profile == name) {
                return Err(ProviderError::ProfileExists(name.clone()));
            }
            all.push(NamedInstallation { profile: name.clone(), installation: from_profile(&base, profile) });
        }
        Ok(all)
    }
```

with the shared helper in the same file:

```rust
/// A registered profile over the role's settings (models root, ports, KV
/// default, runtime directory).
pub fn from_profile(base: &EngineInstallation, profile: &serde_json::Value) -> EngineInstallation {
    let engine = match profile["engine"].as_str() {
        Some("sglang") => mllm_config::engine_policy::Engine::Sglang,
        _ => mllm_config::engine_policy::Engine::Vllm,
    };
    EngineInstallation {
        engine,
        executable: profile["executable"].as_str().unwrap_or_default().into(),
        build_fingerprint: profile["build_fingerprint"].as_str().unwrap_or("unknown").into(),
        deep_park: profile["security"]["deep_park"].as_str() != Some("disabled"),
        installation_drift: if profile["security"]["installation_drift"].as_str() == Some("refuse") {
            mllm_config::effective::InstallationDrift::Refuse
        } else {
            mllm_config::effective::InstallationDrift::Warn
        },
        args: profile["args"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        ..base.clone()
    }
}
```

- [ ] **Step 4: `EnvEngineProvider`.** In `roles.rs`, move everything in `installation()` after the engine match into `fn role_installation(&self, engine: Engine, executable: PathBuf) -> Result<EngineInstallation, ProviderError>`; `installation()` keeps the old one-variable behaviour for callers that still use it, but no longer refuses both (it returns the vLLM one). Override `installations`:

```rust
    fn installations(&self, registered: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<NamedInstallation>, ProviderError> {
        // ADR 0018 §5: one variable is `local`, as always; both (refused
        // before, so nothing depends on it) are `local-vllm` and `local-sglang`.
        let mut all = match (env_value(ENGINE_BIN), env_value(SGLANG_BIN)) {
            (Some(v), None) => vec![NamedInstallation { profile: "local".into(), installation: self.role_installation(Engine::Vllm, v.into())? }],
            (None, Some(s)) => vec![NamedInstallation { profile: "local".into(), installation: self.role_installation(Engine::Sglang, s.into())? }],
            (Some(v), Some(s)) => vec![
                NamedInstallation { profile: "local-vllm".into(), installation: self.role_installation(Engine::Vllm, v.into())? },
                NamedInstallation { profile: "local-sglang".into(), installation: self.role_installation(Engine::Sglang, s.into())? },
            ],
            (None, None) => Vec::new(),
        };
        for (name, profile) in registered {
            if all.iter().any(|n| &n.profile == name) {
                return Err(ProviderError::ProfileExists(name.clone()));
            }
            let engine = if profile["engine"] == "sglang" { Engine::Sglang } else { Engine::Vllm };
            let executable = PathBuf::from(profile["executable"].as_str().unwrap_or_default());
            let base = self.role_installation_with_fingerprint(engine, executable, profile["build_fingerprint"].as_str().unwrap_or("unknown"))?;
            all.push(NamedInstallation { profile: name.clone(), installation: mllm_controller::engine_provider::from_profile(&base, profile) });
        }
        if all.is_empty() {
            return Err(no_installation(format!(
                "this host declares no engine: set {ENGINE_BIN} or {SGLANG_BIN}, or register one with `mllm engine add`"
            )));
        }
        Ok(all)
    }
```

`role_installation_with_fingerprint` is `role_installation` with the fingerprint given instead of probed (a registered profile's version was checked by `engine add`; nothing is executed at start).

- [ ] **Step 5: Several installations in the host policy and gate.** In `standalone_config.rs`, `host_policy` iterates `installations` to build `runtime_profiles` (one entry per `NamedInstallation`, the same per-profile JSON as today, from each installation); role-level fields (`hardware_fingerprint: "standalone-<first engine>"`, `model_store`, `resource_policy`) come from the first. `deployment_document` gains `profile: &str` and writes `"runtime_profile": profile`. In `installation_gate.rs`:

```rust
/// ADR 0018 §5: the embedded host's installations, keyed by executable (two
/// profiles on one executable are one installation).
pub struct EmbeddedInstallations {
    by_executable: std::sync::RwLock<std::collections::BTreeMap<std::path::PathBuf, Arc<EmbeddedInstallation>>>,
}
impl EmbeddedInstallations {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { by_executable: Default::default() })
    }
    pub fn register(&self, profile: &str, engine: Engine, executable: &Path) {
        let registered = Arc::new(EmbeddedInstallation::register(profile, engine, executable));
        if let Ok(mut map) = self.by_executable.write() {
            map.entry(executable.to_path_buf()).or_insert(registered);
        }
    }
    pub fn for_executable(&self, executable: &Path) -> Option<Arc<EmbeddedInstallation>> {
        self.by_executable.read().ok()?.get(executable).cloned()
    }
    pub fn views(&self) -> Vec<serde_json::Value> {
        self.by_executable.read().map(|m| m.values().map(|i| i.view()).collect()).unwrap_or_default()
    }
}
```

`InstalledBindings::new(inner, installations: Arc<EmbeddedInstallations>)`; `InstallationGate::new` looks up `installations.for_executable(Path::new(&work.effective().profile.executable))` and, when absent (not registered), admits without a drift check (unmeasured is never a refusal, ADR 0008). `installation_router` returns `{"api_version":"1","installation": views[0], "installations": views}`.

- [ ] **Step 6: Run the environment tests.**

Run: `cargo test -p mllm-cli --lib standalone_config --locked && cargo test -p mllm-controller --lib installation_gate --locked`
Expected: pass.

- [ ] **Step 7: Write the failing live standalone tests.** Create `crates/mllm-cli/tests/standalone_engines.rs`:

```rust
//! ADR 0018 §5: standalone runs engine registration in one process: the
//! same engines.yaml write, the same socket, the same retirement. Fake-engine
//! tests; not qualification.
mod support;
use mllm_agent::control_socket::{request, ControlRequest, SOCKET_NAME};
use mllm_config::registration::{engines_beside, lock_engines, write_engines, EnginesFile, ProfileSpec};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn standalone_doc(state: &std::path::Path) -> std::path::PathBuf {
    let config = state.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = config.join("standalone.yaml");
    std::fs::write(&path, "schema_version: 1\nkind: standalone\nname: local\nhost:\n  name: local\n  runtime_profiles: {}\n").unwrap();
    path
}

/// `engine add`'s write: `name` into engines.yaml beside the standalone document.
fn register(document: &std::path::Path, name: &str) {
    let path = engines_beside(document);
    let lock = lock_engines(&path).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert(name.into(), mllm_config::registration::profile_document(&ProfileSpec {
        engine: mllm_config::engine_policy::Engine::Vllm,
        executable: "/bin/true".into(),
        build_fingerprint: "0.29.0".into(),
        deep_park: false,
        installation_drift: mllm_config::effective::InstallationDrift::Warn,
        args: vec![],
    }));
    write_engines(&engines, &lock, None).unwrap();
}

// ADR 0018 §3, §5: a profile added while standalone runs is published
// without a restart; the socket is owner-only.
#[tokio::test]
async fn standalone_add_is_usable_without_restart() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    let app = support::boot_configured(state.path(), &document).await;
    assert_eq!(app.profiles(), vec!["local".to_string()]);
    let socket = state.path().join(SOCKET_NAME);
    assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
    register(&document, "vllm-patched");
    let reply = request(&socket, &ControlRequest::Add, Duration::from_secs(10)).await.unwrap();
    assert_eq!(reply["published"], "published", "{reply}");
    assert_eq!(app.profiles(), vec!["local".to_string(), "vllm-patched".to_string()]);
}

// ADR 0018 §4, §5: an unused registered profile is removed and unpublished;
// an environment profile cannot be removed.
#[tokio::test]
async fn standalone_remove_rewrites_and_unpublishes() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "vllm-patched");
    let app = support::boot_configured(state.path(), &document).await;
    let socket = state.path().join(SOCKET_NAME);
    let reply = request(&socket, &ControlRequest::Remove { profile: "vllm-patched".into(), drain: false }, Duration::from_secs(10)).await.unwrap();
    assert_eq!(reply["removed"], "vllm-patched", "{reply}");
    assert!(!EnginesFile::load(&engines_beside(&document)).unwrap().profiles.contains_key("vllm-patched"));
    assert_eq!(app.profiles(), vec!["local".to_string()]);
    let refused = request(&socket, &ControlRequest::Remove { profile: "local".into(), drain: false }, Duration::from_secs(10)).await.unwrap();
    assert_eq!(refused["code"], "invalid_config");
}
```

(`support::boot_configured(state_dir, config)` exists in `tests/support/mod.rs:44` and boots with the fake `local` installation; its `App` must stay alive for the socket to be served.)

- [ ] **Step 8: Run them to verify they fail.**

Run: `cargo test -p mllm-cli --test standalone_engines --locked`
Expected: FAIL to compile (`App::profiles` not found).

- [ ] **Step 9: The swappable embedded host.** In `configuration.rs`, `HostSource::Embedded { document: Arc<std::sync::RwLock<Value>>, id: String }`; `new` wraps its `trusted_host` in a fresh `Arc<RwLock<_>>` and calls:

```rust
    /// ADR 0018 §5: the embedded host document shared with the standalone
    /// control handler, which replaces it when an engine is added or removed.
    pub fn new_shared(state: Arc<Mutex<OwnedCoordinatorState>>, host: Arc<std::sync::RwLock<Value>>, principal: &str) -> Result<Self, ConfigurationFailure> {
        let id = host.read().map_err(|_| ConfigurationFailure::Internal)?["name"]
            .as_str()
            .filter(|id| identifier(id))
            .ok_or(ConfigurationFailure::Internal)?
            .to_owned();
        if !identifier(principal) {
            return Err(ConfigurationFailure::Internal);
        }
        Ok(Self { state, host: HostSource::Embedded { document: host, id }, principal: principal.into() })
    }
```

Every read of the embedded document clones it under the read lock (`document.read().map_err(|_| ConfigurationFailure::Internal)?.clone()`).

Create `crates/mllm-cli/src/standalone_engines.rs`:

```rust
//! ADR 0018 §5: standalone's answers to `mllm engine add`, `remove` and
//! `list`, in one process. Add re-reads engines.yaml, rebuilds the embedded
//! host document and swaps it; remove retires through the store (the
//! ordinary stop path when drained) before rewriting engines.yaml. The
//! standalone document is never written.
use mllm_agent::control_socket::{ControlHandler, ControlRequest};
use mllm_config::registration::{lock_engines, write_engines, EnginesFile, ENVIRONMENT_PROFILES};
use mllm_controller::engine_provider::{EngineProvider, NamedInstallation};
use mllm_controller::installation_gate::EmbeddedInstallations;
use mllm_controller::profile_retirement::{ProfileRetirements, RetirementStep, RETIREMENT_POLL};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// What the embedded host publishes, and how to rebuild it.
pub struct EmbeddedHost {
    pub document: Arc<RwLock<Value>>,
    pub installations: Arc<EmbeddedInstallations>,
    pub named: RwLock<Vec<NamedInstallation>>,
    pub environment_fingerprint: String,
    pub capacity_bytes: i64,
    pub inventory: Option<crate::device_inventory::InventoryPublication>,
}

impl EmbeddedHost {
    pub fn profiles(&self) -> Vec<String> {
        self.named.read().map(|n| n.iter().map(|i| i.profile.clone()).collect()).unwrap_or_default()
    }
    /// Swap in `named`: the host document, then the installation registry.
    pub fn replace(&self, named: Vec<NamedInstallation>) -> Result<(), String> {
        let document = crate::standalone_config::host_policy(&named, &self.environment_fingerprint, self.capacity_bytes, self.inventory.as_ref());
        for n in &named {
            self.installations.register(&n.profile, n.installation.engine, &n.installation.executable);
        }
        *self.document.write().map_err(|_| "host document lock poisoned")? = document;
        *self.named.write().map_err(|_| "installation list lock poisoned")? = named;
        Ok(())
    }
}

pub struct StandaloneControl {
    /// The role's engines.yaml (Task 2's `engines_path`).
    pub engines: PathBuf,
    pub provider: Arc<dyn EngineProvider>,
    pub host: Arc<EmbeddedHost>,
    pub retirements: Arc<dyn ProfileRetirements>,
    pub host_id: String,
}

fn refused(code: &str, message: impl Into<String>) -> Value {
    json!({"ok": false, "code": code, "message": message.into()})
}

impl StandaloneControl {
    fn reload(&self) -> Value {
        let registered = match EnginesFile::load(&self.engines) {
            Ok(engines) => engines.profiles,
            Err(e) => return refused("invalid_config", e.detail),
        };
        for (name, profile) in &registered {
            if let Err(e) = mllm_config::registration::check_profile(name, profile) {
                return refused("publish_rejected", format!("profile {name}: {}", e.detail));
            }
        }
        let named = match self.provider.installations(&registered) {
            Ok(named) => named,
            Err(mllm_controller::engine_provider::ProviderError::ProfileExists(name)) => {
                return refused("profile_exists", format!("{name} is already an environment profile"))
            }
            Err(e) => return refused("publish_rejected", e.to_string()),
        };
        if named.iter().map(|n| &n.profile).eq(self.host.profiles().iter()) {
            return json!({"ok": true, "published": "unchanged"});
        }
        match self.host.replace(named) {
            Ok(()) => json!({"ok": true, "published": "published"}),
            Err(e) => refused("internal", e),
        }
    }

    async fn remove(&self, profile: &str, drain: bool) -> Value {
        if ENVIRONMENT_PROFILES.contains(&profile) {
            return refused("invalid_config", format!("{profile} comes from MLLM_VLLM_BIN / MLLM_SGLANG_BIN; unset it and restart"));
        }
        let key = format!("{}:{}", self.host_id, ulid::Ulid::new());
        let (service, host, name, k) = (self.retirements.clone(), self.host_id.clone(), profile.to_owned(), key.clone());
        let mut step = tokio::task::spawn_blocking(move || service.begin(&host, &name, &k, drain)).await.unwrap_or(RetirementStep::Refused("retirement failed".into()));
        while let RetirementStep::Draining(_) = step {
            tokio::time::sleep(RETIREMENT_POLL).await;
            let (service, host, name, k) = (self.retirements.clone(), self.host_id.clone(), profile.to_owned(), key.clone());
            if let Ok(Some(next)) = tokio::task::spawn_blocking(move || service.poll(&host, &name, &k)).await {
                step = next;
            }
        }
        match step {
            RetirementStep::Confirmed => {}
            RetirementStep::InUse(d) | RetirementStep::Holding(d) => {
                return json!({"ok": false, "code": "profile_in_use", "deployments": d, "message": "deployments use the profile; stop them, or use --drain"})
            }
            RetirementStep::Refused(reason) => return refused("publish_rejected", reason),
            RetirementStep::Draining(_) => unreachable!("loop above"),
        }
        let written = lock_engines(&self.engines).and_then(|lock| {
            let mut engines = EnginesFile::load(&self.engines)?;
            engines.profiles.remove(profile);
            write_engines(&engines, &lock, None)
        });
        if let Err(e) = written {
            return refused("internal", e.detail);
        }
        let mut reply = self.reload();
        reply["removed"] = profile.into();
        reply
    }
}

#[async_trait::async_trait]
impl ControlHandler for StandaloneControl {
    async fn handle(&self, request: ControlRequest) -> Value {
        match request {
            ControlRequest::Add => self.reload(),
            ControlRequest::Remove { profile, drain } => self.remove(&profile, drain).await,
            ControlRequest::List => json!({"ok": true, "connected": true, "live_profile_update": true,
                "accepted": self.host.profiles().into_iter().map(|p| (p, json!({}))).collect::<serde_json::Map<_, _>>(),
                "users": {}}),
        }
    }
}
```

(Confirmed retirements on the embedded host are deleted after the rewrite: call `store.cancel_profile_retirement(host_id, profile, key)` through the same `commands` the service uses, so the retirement does not keep the profile out of placement if it is added again.)

- [ ] **Step 10: Wire it in `start_standalone_inner`.** Replace `let installation = provider.installation()?;` with `let engines = EnginesFile::load(&engines_path(config, &config_home(&env)?))?;` (the `config` path `start_standalone_configured` received, else the user's config home; `check_honoured` still refuses `host.runtime_profiles` in the standalone document itself) and `let named = provider.installations(&engines.profiles)?;` (`ProviderError::ProfileExists` maps to a `StartError` whose structured code is `profile_exists`); `installation` becomes `named[0].installation.clone()`. Build `host_policy(&named, ..)`; register each installation in `EmbeddedInstallations::new()`; pass the shared document `Arc<RwLock<Value>>` to `SharedConfigurationSource::new_shared`; build `EmbeddedHost` and keep it in `App.host`; add `pub fn profiles(&self) -> Vec<String> { self.host.profiles() }`. After the management routers exist, bind `ControlServer::bind(&state_dir.join(SOCKET_NAME))` (on error, keep the reason in `config_notices`) and spawn `serve(Arc::new(StandaloneControl { engines, provider, host, retirements: Arc::new(StoreRetirements::new(actions.clone())), host_id }), geteuid(), shutdown)`, where `engines` is the engines.yaml path above and `host_id` is the embedded host's name. The socket task stops with the role's supervision.

- [ ] **Step 11: Run the tests.**

Run: `cargo test -p mllm-cli --test standalone_engines --test standalone_start --test standalone_installation --locked && cargo test -p mllm-cli --lib --locked`
Expected: all pass.

- [ ] **Step 12: Core suite, workspace, Clippy.** Run the three Global Constraints commands. Expected: pass.

- [ ] **Step 13: Commit**

```bash
git add crates/mllm-controller crates/mllm-management crates/mllm-cli crates/mllm-testkit
git commit -m "feat(standalone): several engines and live engine add and remove

ADR 0018 section 5: MLLM_VLLM_BIN alone still gives local; both
variables give local-vllm and local-sglang; engines registered in
engines.yaml coexist, and a name declared twice is refused
profile_exists at start. The standalone role serves the same control
socket, swaps its embedded host document on add, and removes a profile
only after the store-backed retirement confirms it."
```

---

### Task 16: Live harness, rows ENG1–ENG4, operator guide and status (rows written, not run by the plan's author)

One deliverable: everything the live qualification needs, plus the operator documentation of the feature. CPU tests above are not qualification; these rows are. Only the existing environments are used: vLLM `$HOME/mllm-vllm-venv2` (host-a) and `$HOME/mllm-vllm-0.29-venv` (host-b), SGLang `$HOME/mllm-sglang-0.5.20-venv` (both). No venv is created or changed.

**Files:**
- Modify: `scripts/live/matrix/lib.sh:126-130` (per-host binary), `scripts/live/matrix/roles.sh` (systemd host bring-up, profile-less host document), `scripts/live/matrix/gen_host_doc.py:47-63` (`--no-profiles`), `scripts/live/matrix/README.md`
- Create: `scripts/live/matrix/rows/ENG1.sh`, `ENG2.sh`, `ENG3.sh`, `ENG4.sh`
- Modify: `docs/operations/install.md` (new section "Registering engines"), `docs/runbooks/f2-current-status.md` (new top section)

**Interfaces:**
- Consumes: Tasks 13–15 and 17 behaviour through the real binaries; `rowlib.sh` (`step`, `deploy`, `wait_state`, `infer`, `keep_owned`, `cleanup_check`, `stop_dep`, `delete_dep`, `refused_with`, `timed`, `host_idle`), `lib.sh` (`rsh`, `rsh_out`, `cli`, `vllm_venv`, `SGLANG_VENV`, `RRD`, `RBIN`).
- Produces: `rbin <host>`; `roles.sh host-doc-bare <host> <policy>`, `roles.sh host-up-systemd <host>`, `roles.sh host-down-systemd <host>`; rows ENG1–ENG4.

- [ ] **Step 1: Harness hooks.**

`lib.sh`, after `RBIN=...` in `load_run`:

```bash
}

# ADR 0018 (row ENG4): one host may run another binary than the rest, e.g. an
# rc.3 agent beside new ones. MLLM_REMOTE_BIN_a / MLLM_REMOTE_BIN_b override
# RBIN for that host only.
rbin() { # rbin <host>
  local var
  var="MLLM_REMOTE_BIN_$(host_short "$1")"
  printf '%s\n' "${!var:-$RBIN}"
```

(Place the function after `load_run`'s closing brace; the snippet above shows the brace it follows.) In `roles.sh`, `host_up` and `host_down` use `$(rbin "$host")` in place of `$RBIN`, and add:

```bash
host_doc_bare() { # ADR 0018: a host document with no runtime profiles
  local host=$1 policy=$2
  NO_PROFILES=1 host_doc "$host" "$policy"
}

host_up_systemd() { # ADR 0018 (ENG1): the host role under a transient user unit
  local host=$1
  load_run
  rsh "$host" "systemd-run --user --unit=mx-host-$RUN --collect --property=KillMode=process --property=UMask=0077 \
$(rbin "$host") start host --config $RRD/host.yaml"
}

host_down_systemd() {
  local host=$1
  load_run
  rsh "$host" "systemctl --user stop mx-host-$RUN"
}
```

In `host_doc`, pass `${NO_PROFILES:+--no-profiles}` to `gen_host_doc.py`, and add the dispatch lines `host-doc-bare) [ $# -eq 2 ] || usage; host_doc_bare "$@" ;;`, `host-up-systemd) host_up_systemd "$@" ;;`, `host-down-systemd) host_down_systemd "$@" ;;`. In `gen_host_doc.py` add `parser.add_argument("--no-profiles", action="store_true")` and, before writing, `if args.no_profiles: doc["runtime_profiles"] = {}`.

- [ ] **Step 2: Row ENG1.** Create `scripts/live/matrix/rows/ENG1.sh`:

```bash
# shellcheck shell=bash
# ENG1 (ADR 0018 §1, §3): engine add of the existing vLLM and SGLang
# environments on a host running under systemd, published live, then a
# deployment on each new profile serves:
#   run_row.sh ENG1 --tag a -- host-a va-4 sa-4
#   run_row.sh ENG1 --tag 17 -- host-b vb-4 sb-4
#
# Expected:
#   a  engine detect (no --path) lists both home-level environments
#      (metadata only; owner decision 2026-09-25).
#   a2 host.yaml is byte-identical before and after the adds; the profiles
#      live in $RRD/engines.yaml beside it.
#   b  engine add of each exits 0 with published=published within 10 s.
#   c  list engines on the server shows vllm and sglang on the host, custom=false.
#   d  the standard fixtures (profiles vllm and sglang, revision 1) reach
#      Ready and answer; stop with verified cleanup; deleted.
#   e  the host is returned to its tmux role and full document.

eng_remote() { # eng_remote <host> <engine args...>: run `mllm engine` on the host
  local host=$1
  shift
  rsh "$host" "$(rbin "$host") engine $* --config $RRD/host.yaml"
}

eng_published() { # eng_published <json file>: add answered published
  dry && return 0
  python3 - "$1" <<'PY'
import json, sys
line = [l for l in open(sys.argv[1]).read().splitlines() if l.startswith("{")][-1]
reply = json.loads(line)
print(reply.get("published"), reply.get("version"), reply.get("custom"))
sys.exit(0 if reply.get("published") == "published" and reply.get("custom") is False else 1)
PY
}

eng_bare_systemd() { # eng_bare_systemd <host>: profile-less document, systemd role
  local host=$1
  "$MATRIX_DIR/roles.sh" host-down "$host" &&
    rsh "$host" "rm -f $RRD/engines.yaml $RRD/engines.yaml.lock" &&
    "$MATRIX_DIR/roles.sh" host-doc-bare "$host" "${POLICY:-normal}" &&
    "$MATRIX_DIR/roles.sh" host-up-systemd "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180
}

eng_restore() { # eng_restore <host>: back to the tmux role and full document
  local host=$1
  "$MATRIX_DIR/roles.sh" host-down-systemd "$host" || true
  # The full matrix document declares vllm and sglang itself; the same names
  # in engines.yaml would be refused at start.
  rsh "$host" "rm -f $RRD/engines.yaml $RRD/engines.yaml.lock" || return 1
  "$MATRIX_DIR/roles.sh" host-doc "$host" "${POLICY:-normal}" &&
    "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180
}

eng_add() { # eng_add <host> <path>
  local host=$1 path=$2 out
  out="$EVID/add-$(basename "$(dirname "$(dirname "$path")")")-$host.json"
  dry && { eng_remote "$host" add "$path"; return 0; }
  eng_remote "$host" add "$path" | tee "$out" && eng_published "$out"
}

eng_listed() { # eng_listed <host> <profile...>: list engines shows them
  local host=$1
  shift
  cli list engines --output json >"$EVID/engines.json" || return 1
  dry && return 0
  python3 - "$EVID/engines.json" "$host" "$@" <<'PY'
import json, sys
rows = json.load(open(sys.argv[1]))["engines"]
host, wanted = sys.argv[2], sys.argv[3:]
have = {r["profile"]: r for r in rows if r["host"] == host}
missing = [p for p in wanted if p not in have or have[p]["custom"]]
print({p: (have[p]["version"], have[p]["custom"]) for p in have})
sys.exit(1 if missing else 0)
PY
}

eng_serves() { # eng_serves <fixture>
  local fix=$1 host dep rc=0
  host=$(fixture_host "$fix"); dep=$fix
  step "deploy-$fix" deploy "$fix" --activate --wait || return 1
  step "owned-$fix" keep_owned "$dep" ready
  step "infer-$fix" infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step "stop-$fix" stop_dep "$dep" || rc=1
  step "stopped-$fix" wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step "cleanup-$fix" cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step "delete-$fix" delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() {
  local host=$1 vfix=$2 sfix=$3 rc=0
  step before host_idle "$host" || return 1
  step bare-systemd eng_bare_systemd "$host" || return 1
  step host-yaml-before rsh "$host" "sha256sum $RRD/host.yaml > $RRD/host-yaml.sum" || rc=1
  step detect eng_remote "$host" detect || rc=1
  step add-vllm timed add-vllm eng_add "$host" "$(vllm_venv "$host")/bin/vllm" || rc=1
  step add-sglang timed add-sglang eng_add "$host" "$SGLANG_VENV/bin/python3" || rc=1
  step listed eng_listed "$host" vllm sglang || rc=1
  step host-yaml-unchanged rsh "$host" "sha256sum -c $RRD/host-yaml.sum" || rc=1
  step engines-file rsh "$host" "head -1 $RRD/engines.yaml" || rc=1
  eng_serves "$vfix" || rc=1
  eng_serves "$sfix" || rc=1
  step restore eng_restore "$host" || rc=1
  return "$rc"
}
```

- [ ] **Step 3: Row ENG3** (removal; reuses ENG1's helpers). Create `scripts/live/matrix/rows/ENG3.sh`:

```bash
# shellcheck shell=bash
# ENG3 (ADR 0018 §4): removing a published profile is refused while a
# deployment uses it, then --drain stops it through the ordinary path and
# removes the profile only on stop evidence:
#   run_row.sh ENG3 --tag a -- host-a va-4
#
# Expected:
#   a  engine remove vllm while the deployment is Ready exits 20 naming it;
#      the deployment stays Ready and answers.
#   b  engine remove vllm --drain exits 0; the deployment is stopped with
#      verified cleanup (accounting released only on gone evidence).
#   c  list engines no longer shows vllm on the host; a start of the
#      deployment is refused (no eligible host carries the profile).
#   d  engine add of the vLLM environment again publishes it.
. "$MATRIX_DIR/rows/ENG1.sh"

row_main() {
  local host vfix=$2 dep rc=0
  host=$1; dep=$vfix
  step before host_idle "$host" || return 1
  step bare-systemd eng_bare_systemd "$host" || return 1
  step add-vllm eng_add "$host" "$(vllm_venv "$host")/bin/vllm" || return 1
  step deploy deploy "$vfix" --activate --wait || return 1
  step owned keep_owned "$dep" ready
  step remove-in-use refused_with profile_in_use eng_remote "$host" remove vllm || rc=1
  step still-ready wait_state "$dep" ready 30 || rc=1
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step remove-drain timed remove-drain eng_remote "$host" remove vllm --drain || rc=1
  step stopped wait_state "$dep" stopped 900 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step gone refused eng_listed "$host" vllm || rc=1
  step start-refused refused start_dep "$dep" --wait || rc=1
  step readd eng_add "$host" "$(vllm_venv "$host")/bin/vllm" || rc=1
  step delete delete_dep "$dep" || rc=1
  step restore eng_restore "$host" || rc=1
  return "$rc"
}
```

- [ ] **Step 4: Row ENG2** (standalone, both engines). Create `scripts/live/matrix/rows/ENG2.sh`:

```bash
# shellcheck shell=bash
# ENG2 (ADR 0018 §5): standalone with MLLM_VLLM_BIN and MLLM_SGLANG_BIN both
# set (refused before) publishes local-vllm and local-sglang, and serves a
# deployment on each in turn:
#   run_row.sh ENG2 --no-e0 -- host-a
#
# Expected:
#   a  the role starts; engine list shows local-vllm and local-sglang,
#      published.
#   b  engine add of nothing new is not needed; engine remove local-vllm is
#      refused (environment profile).
#   c  the role stops cleanly; its private state directory is removed.
# The host's matrix role is stopped for the row and restarted after it.

ENG2_DIR=
eng2_start() { # eng2_start <host>
  local host=$1
  ENG2_DIR=$RRD/eng2-standalone
  rsh "$host" "rm -rf $ENG2_DIR && mkdir -m 700 $ENG2_DIR && tmux new-session -d -s mx-eng2-$RUN \
env MLLM_STATE_DIR=$ENG2_DIR MLLM_VLLM_BIN=$(vllm_venv "$host")/bin/vllm MLLM_SGLANG_BIN=$SGLANG_VENV/bin/python3 \
MLLM_MODELS_ROOT=$MODELS_ROOT $REMOTE_TREE/scripts/live/matrix/role_exec.sh $ENG2_DIR/role.pid $ENG2_DIR/role.log \
$(rbin "$host") start standalone"
  rsh "$host" "for i in \$(seq 1 120); do [ -S $ENG2_DIR/control.sock ] && exit 0; sleep 1; done; exit 1"
}

eng2_list() { # eng2_list <host>
  local host=$1
  rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") engine list" | tee "$EVID/eng2-list.json"
  dry && return 0
  python3 - "$EVID/eng2-list.json" <<'PY'
import json, sys
line = [l for l in open(sys.argv[1]).read().splitlines() if l.startswith("{")][-1]
rows = {r["profile"]: r for r in json.loads(line)["engines"]}
ok = all(rows.get(p, {}).get("published") == "published" for p in ("local-vllm", "local-sglang"))
print(sorted(rows))
sys.exit(0 if ok else 1)
PY
}

eng2_stop() { # eng2_stop <host>
  local host=$1
  rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile $ENG2_DIR/role.pid --expect 'start standalone' --signal TERM; \
sleep 10; rm -rf $ENG2_DIR"
}

row_main() {
  local host=$1 rc=0
  step before host_idle "$host" || return 1
  step matrix-host-down "$MATRIX_DIR/roles.sh" host-down "$host" || return 1
  step start eng2_start "$host" || { rc=1; }
  step list eng2_list "$host" || rc=1
  step env-remove-refused refused_with invalid_config rsh "$host" "MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") engine remove local-vllm" || rc=1
  step stop eng2_stop "$host" || rc=1
  step idle host_idle "$host" || rc=1
  step matrix-host-up "$MATRIX_DIR/roles.sh" host-up "$host" || rc=1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  return "$rc"
}
```

(Serving a deployment on each standalone profile uses the standalone client's `deploy model --file` with a document naming `runtime_profile: local-vllm` or `local-sglang`; add those two steps once Task 15's CPU tests have fixed the standalone deployment shape, generating the files with `gen_deployment.py --document-json '{"runtime_profile":"local-vllm","placement":null}'` and deploying with `MLLM_STATE_DIR=$ENG2_DIR $(rbin "$host") deploy model --file <file> --activate --wait`.)

- [ ] **Step 5: Row ENG4** (version-skew fallback, owner decision 2026-09-25). Create `scripts/live/matrix/rows/ENG4.sh`:

```bash
# shellcheck shell=bash
# ENG4 (ADR 0018 §3, owner decision 2026-09-25): engine add beside an rc.3 agent, and a
# new agent against an rc.3 server:
#   MLLM_RC3_LOCAL=~/rc3/mllm MLLM_RC3_REMOTE=~/rc3/mllm run_row.sh ENG4 --no-e0 -- host-b
#
# Preconditions (checked, never installed or downloaded): MLLM_RC3_LOCAL
# (control-host) and MLLM_RC3_REMOTE (on the host) are existing rc.3 binaries whose
# `--version` prints 0.1.0-rc.3.
#
# Expected:
#   a  rc.3 host role, new CLI binary: engine add exits 22 agent_unreachable
#      and engines.yaml is written (revision 1). An rc.3 agent never reads
#      engines.yaml, so after an rc.3 restart the profile is still not
#      published; after the host role is upgraded to the new binary it is
#      published at start (list hosts).
#   b  rc.3 server, new host role: engine add exits 0 with
#      published=restart_required; after a host restart it is published.
#   c  both sides are returned to the new binaries.
. "$MATRIX_DIR/rows/ENG1.sh"

eng4_preconditions() {
  local host=$1
  [ -n "${MLLM_RC3_LOCAL:-}" ] && [ -n "${MLLM_RC3_REMOTE:-}" ] || { echo "set MLLM_RC3_LOCAL and MLLM_RC3_REMOTE"; return 1; }
  dry && return 0
  "$MLLM_RC3_LOCAL" --version | grep -q '0.1.0-rc.3' || { echo "MLLM_RC3_LOCAL is not rc.3"; return 1; }
  rsh "$host" "$MLLM_RC3_REMOTE --version | grep -q '0.1.0-rc.3'" || { echo "MLLM_RC3_REMOTE is not rc.3"; return 1; }
}

eng4_rc3_agent() { # (a)
  local host=$1 short
  short=$(host_short "$host")
  "$MATRIX_DIR/roles.sh" host-down "$host" && rsh "$host" "rm -f $RRD/engines.yaml" &&
    "$MATRIX_DIR/roles.sh" host-doc-bare "$host" "${POLICY:-normal}" || return 1
  env "MLLM_REMOTE_BIN_$short=$MLLM_RC3_REMOTE" "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  refused_with agent_unreachable rsh "$host" "$RBIN engine add $(vllm_venv "$host")/bin/vllm --config $RRD/host.yaml" || return 1
  "$MATRIX_DIR/roles.sh" host-down "$host" &&
    env "MLLM_REMOTE_BIN_$short=$MLLM_RC3_REMOTE" "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  # rc.3 ignores engines.yaml: not published.
  if ! dry && cli list hosts --output json | tee "$EVID/rc3-agent-hosts.json" | grep -q '"vllm"'; then
    echo "UNEXPECTED: an rc.3 agent published engines.yaml"; return 1
  fi
  "$MATRIX_DIR/roles.sh" host-down "$host" && "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  cli list hosts --output json | tee "$EVID/upgraded-agent-hosts.json" | grep -q '"vllm"'
}

eng4_rc3_server() { # (b)
  local host=$1 out
  "$MATRIX_DIR/roles.sh" down || true
  MLLM_LOCAL_BIN=$MLLM_RC3_LOCAL "$MATRIX_DIR/roles.sh" up "${POLICY:-normal}" || return 1
  "$MATRIX_DIR/roles.sh" host-down "$host" && rsh "$host" "rm -f $RRD/engines.yaml" &&
    "$MATRIX_DIR/roles.sh" host-doc-bare "$host" "${POLICY:-normal}" &&
    "$MATRIX_DIR/roles.sh" host-up "$host" && "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  out=$(eng_remote "$host" add "$(vllm_venv "$host")/bin/vllm") || return 1
  printf '%s\n' "$out" | tee "$EVID/rc3-server-add.json"
  dry || printf '%s\n' "$out" | grep -q '"published":"restart_required"' || return 1
  "$MATRIX_DIR/roles.sh" host-down "$host" && "$MATRIX_DIR/roles.sh" host-up "$host" &&
    "$MATRIX_DIR/roles.sh" wait-online 180 || return 1
  MLLM_LOCAL_BIN=$MLLM_RC3_LOCAL cli list hosts --output json | tee "$EVID/rc3-server-hosts.json" | grep -q '"vllm"'
}

row_main() {
  local host=$1 rc=0
  step preconditions eng4_preconditions "$host" || return 1
  step rc3-agent eng4_rc3_agent "$host" || rc=1
  step rc3-server eng4_rc3_server "$host" || rc=1
  step restore-engines rsh "$host" "rm -f $RRD/engines.yaml" || true
  step restore-server "$MATRIX_DIR/roles.sh" down || true
  step restore-up "$MATRIX_DIR/roles.sh" up "${POLICY:-normal}" || rc=1
  return "$rc"
}
```

- [ ] **Step 6: Validate the harness (not qualification).**

Run: `for r in ENG1 ENG2 ENG3 ENG4; do bash -n scripts/live/matrix/rows/$r.sh || exit 1; done && shellcheck scripts/live/matrix/rows/ENG*.sh scripts/live/matrix/lib.sh scripts/live/matrix/roles.sh && python3 -m py_compile scripts/live/matrix/gen_host_doc.py`
Expected: no errors (install `shellcheck` locally if missing; skip it only with a note in the commit message).

Run: `scripts/live/matrix/run_row.sh ENG1 --dry-run --tag a -- host-a va-4 sa-4` (and ENG2–ENG4 likewise, ENG4 with `MLLM_RC3_LOCAL=/bin/true MLLM_RC3_REMOTE=/bin/true`)
Expected: each writes `target/live/matrix/dry-run/ENG*/commands.log` and contacts no host.

- [ ] **Step 7: Operator guide.** Add to `docs/operations/install.md` a section:

```markdown
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
The running role listens on `<state_dir>/control.sock` (mode 0600, your user
only) for these commands.

`engine remove` removes only profiles `engine add` registered; one you wrote
into `host.yaml` stays yours to edit. It is refused while a deployment on this
machine uses the profile (`profile_in_use`); `--drain` stops those deployments
through the ordinary stop path first, and the profile is removed only after the
server confirms their stop evidence. A role that is not running cannot remove a
published profile (`agent_unreachable`); start it and retry.

A deployment naming a runtime profile that no allowed host publishes is refused
at `deploy` (`profile_not_published`), naming the profile and each host; run
`mllm engine add <path> --name <profile>` on a host, then deploy again.

In standalone, `MLLM_VLLM_BIN` or `MLLM_SGLANG_BIN` alone gives the profile
`local`; both give `local-vllm` and `local-sglang`. Profiles you add coexist
with them.
```

- [ ] **Step 8: Status runbook.** Add at the top of `docs/runbooks/f2-current-status.md`:

```markdown
## Engine registration — <date> (branch `feat/engine-registration`)

ADR 0018 (amends SPEC §4.2, §15.1): `mllm engine detect|add|list|remove` and
`mllm list engines`, live publication (`live_profile_update`), two-phase removal,
standalone `local-vllm`/`local-sglang`. Commits: `<first>..<last>`.

Evidence: CPU and Fake-engine tests only (`crates/*/tests/{registration,engines,
control_socket,live_profiles,engine_cli,standalone_engines}.rs`). These are not
qualification. Live rows ENG1–ENG4 are written and not yet run.

Owner decisions of 2026-09-25 are recorded in the plan and ADR 0018; the answer on the exit code for `profile_not_published` (plan item 20): <record it>.
```

(Fill `<date>`, the commit range and the owner's answers when the section is written; they are facts of the execution, not of the plan.)

- [ ] **Step 9: Commit**

```bash
git add scripts/live/matrix docs/operations/install.md docs/runbooks/f2-current-status.md
git commit -m "test(live): engine registration rows ENG1 to ENG4 and operator guide

ADR 0018: rows for engine add on a systemd host, standalone with both
engines, removal refused then drained, and the rc.3 fallbacks, using
only the existing engine environments on host-a and host-b. The
harness gains a per-host binary override, a systemd host bring-up and a
profile-less host document. Written and dry-run only; not run live."
```

---

### Task 17: Deploy fails fast on a runtime profile no allowed host publishes

Owner decision 2026-09-25: a deployment is never re-resolved after `engine add`. Instead, `deploy` refuses at once, storing nothing, when no allowed host publishes the named `runtime_profile`, and the refusal names the profile, each host with what it does publish, and the fix. This task can run any time after Task 14 (it adds one CLI code beside 16–23).

**Files:**
- Modify: `crates/mllm-management/src/configuration.rs` (`ConfigurationFailure` `:33-80` gains `ProfileNotPublished`; `response` `:84-125`; the embedded path in `accept` `:449-468`; `registry_targets` `:539-640`)
- Modify: every exhaustive `match` over `ConfigurationFailure` the compiler names (for example `crates/mllm-management/src/actions.rs:820` area)
- Modify: `crates/mllm-cli/src/client.rs:595-622` (`refusal`), `crates/mllm-cli/src/output.rs` (`ExitCode::PROFILE_NOT_PUBLISHED = 24` and its `exit_code` arm), `docs/operations/install.md` (exit-code table row 24)
- Test: `crates/mllm-management/tests/configuration.rs` (append), `crates/mllm-cli/tests/errors.rs` (append)

**Interfaces:**
- Consumes: `Store::host_publication`, `Store::enrolled_hosts`; the deployment document's `runtime_profile` field.
- Produces: `ConfigurationFailure::ProfileNotPublished { profile: String, hosts: Vec<(String, Vec<String>)> }` → HTTP 409, `{"error":{"code":"profile_not_published","message":..,"retryable":false,"details":{"profile":..,"hosts":{HOST:[PROFILES]}}}}`; CLI code `profile_not_published`, exit 24; per-host refusal diagnostic `profile_not_published` for an allowed host that lacks the profile while another has it.

- [ ] **Step 1: Write the failing tests.** Append to `crates/mllm-management/tests/configuration.rs` (it has `fixture`, `app`, `request`, `json_response`, `publish_host`):

```rust
/// ADR 0018 §7 (owner decision 2026-09-25): a deploy naming a runtime profile
/// no allowed host publishes is refused at once and nothing is stored; the
/// refusal names the profile, each host with what it publishes, and the fix.
// T03 T07
#[tokio::test]
async fn a_deploy_naming_an_unpublished_profile_fails_fast() {
    let (_directory, state, mut config, _) = fixture();
    publish_host(&state, "host-a", 'e', json!({}));
    publish_host(&state, "host-b", 'f', json!({}));
    let router = configuration_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(SharedConfigurationSource::from_registry(state.clone(), "owner").unwrap()),
    );
    config["instances"] = json!(1);
    config["placement"] = json!({"hosts": ["host-a", "host-b"]});
    config["devices"] = json!([{"sharing": "shared"}]);
    for phase in ["cold", "ready", "parking", "wake"] {
        config["resources"][phase]["devices"] = json!([{"sharing": "shared"}]);
    }
    let before = state.lock().unwrap().store().snapshot().unwrap().deployments.len();
    let mut missing = config.clone();
    missing["runtime_profile"] = json!("vllm-patched");
    let response = router
        .clone()
        .oneshot(request("POST", "/management/v1/deployments", "missing", json!({"config":missing,"activate":false})))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let body = json_response(response).await;
    assert_eq!(body["error"]["code"], "profile_not_published", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    for needle in ["vllm-patched", "host-a", "host-b", "local", "mllm engine add", "--name vllm-patched"] {
        assert!(message.contains(needle), "{needle}: {message}");
    }
    assert_eq!(body["error"]["details"]["profile"], "vllm-patched");
    assert_eq!(state.lock().unwrap().store().snapshot().unwrap().deployments.len(), before, "nothing stored");
    // The same deployment naming a published profile is accepted.
    let response = router
        .oneshot(request("POST", "/management/v1/deployments", "present", json!({"config":config,"activate":false})))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
}

/// ADR 0018 §7: the embedded (standalone) host fails fast the same way.
// T03 T07
#[tokio::test]
async fn an_embedded_deploy_naming_an_unpublished_profile_fails_fast() {
    let (_directory, state, mut config, host) = fixture();
    let router = app(state.clone(), host);
    config["runtime_profile"] = json!("sglang");
    let response = router
        .oneshot(request("POST", "/management/v1/deployments", "embedded", json!({"config":config,"activate":false})))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let body = json_response(response).await;
    assert_eq!(body["error"]["code"], "profile_not_published", "{body}");
    assert!(state.lock().unwrap().store().snapshot().unwrap().deployments.is_empty());
}
```

Append to `crates/mllm-cli/tests/errors.rs`:

```rust
// T01 (ADR 0018 §7): a deploy refused for an unpublished profile exits 24.
#[test]
fn profile_not_published_exits_24() {
    let error = StructuredError { code: "profile_not_published", message: String::new() };
    assert_eq!(error.exit_code(), ExitCode(24));
    assert_eq!(ExitCode::PROFILE_NOT_PUBLISHED, ExitCode(24));
}
```

- [ ] **Step 2: Run them to verify they fail.**

Run: `cargo test -p mllm-management --test configuration --locked unpublished_profile && cargo test -p mllm-cli --test errors --locked`
Expected: the management tests get 202 (or 403) instead of 409; the CLI test fails to compile (`PROFILE_NOT_PUBLISHED`).

- [ ] **Step 3: The failure and its answer.** In `configuration.rs`, add to `ConfigurationFailure`:

```rust
    /// ADR 0018 §7 (owner decision 2026-09-25): no allowed host publishes the
    /// deployment's runtime profile. Refused before anything is stored.
    ProfileNotPublished { profile: String, hosts: Vec<(String, Vec<String>)> },
```

and in `response`, beside the `HostIneligible` arm (and add `ProfileNotPublished { .. }` to the `unreachable!` arm below it):

```rust
            ProfileNotPublished { profile, hosts } => {
                let each: Vec<String> = hosts
                    .iter()
                    .map(|(host, names)| {
                        format!("{host}: {}", if names.is_empty() { "none".to_owned() } else { names.join(", ") })
                    })
                    .collect();
                let message = format!(
                    "runtime profile `{profile}` is not published by any allowed host ({}); register it on a host with `mllm engine add <path> --name {profile}`, then deploy again",
                    each.join("; ")
                );
                let details: serde_json::Map<String, Value> =
                    hosts.iter().map(|(h, n)| (h.clone(), serde_json::json!(n))).collect();
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"api_version":"1","error":{
                        "code":"profile_not_published","message":message,"retryable":false,
                        "operation_id":null,"details":{"profile":profile,"hosts":details}}})),
                )
                    .into_response();
            }
```

- [ ] **Step 4: Check before anything is stored.** Add a helper beside `registry_targets`:

```rust
/// ADR 0018 §7: the profile the deployment names, and whether `published`
/// (host, profiles) carries it anywhere. Refused as a whole when nowhere.
fn require_published(config_json: &str, published: Vec<(String, Vec<String>)>) -> Result<String, ConfigurationFailure> {
    let config = mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, config_json)?;
    let profile = config["runtime_profile"].as_str().unwrap_or_default().to_owned();
    if published.iter().any(|(_, names)| names.iter().any(|n| *n == profile)) {
        Ok(profile)
    } else {
        Err(ConfigurationFailure::ProfileNotPublished { profile, hosts: published })
    }
}

fn profile_names(document: &Value) -> Vec<String> {
    document["runtime_profiles"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}
```

In `registry_targets`, right after `allowed` is validated:

```rust
    // ADR 0018 §7 (owner decision 2026-09-25): fail fast, storing nothing.
    let mut published = Vec::new();
    for selector in &allowed {
        let names = store
            .host_publication(selector)
            .map_err(|_| ConfigurationFailure::Internal)?
            .and_then(|p| serde_json::from_str::<Value>(&p.config_json).ok())
            .map(|d| profile_names(&d))
            .unwrap_or_default();
        published.push((selector.clone(), names));
    }
    let profile = require_published(config_json, published)?;
```

and in the loop, after the `no_runtime_profiles` check:

```rust
        if original["runtime_profiles"].get(&profile).is_none() {
            single = Some(refuse(ConfigurationFailure::HostPolicyDenied, "profile_not_published", &mut refusals));
            continue;
        }
```

In the `HostSource::Embedded { document, id }` arm, before composing controls:

```rust
                let (ConfigurationCommand::Create { config_json }
                | ConfigurationCommand::Replace { config_json, .. }) = &command;
                require_published(config_json, vec![(id.clone(), profile_names(document))])?;
```

(After Task 15, `document` is behind the `RwLock`; take the read guard first. If Task 17 runs before Task 15, use it as it is.)

- [ ] **Step 5: CLI code and exit code.** In `client.rs::refusal` add `"profile_not_published" => "profile_not_published",` beside `"host_ineligible"`. In `output.rs` add `pub const PROFILE_NOT_PUBLISHED: Self = Self(24);` after `NOT_INTERACTIVE` and the arm `"profile_not_published" => ExitCode::PROFILE_NOT_PUBLISHED,`. In `errors.rs`'s `a_start_with_no_eligible_host_exits_15` add `24` to the `assert_ne!` list. Add row `24 profile_not_published` (remedy: register the profile on a host with `mllm engine add … --name <profile>`, then deploy again) to the exit table in `docs/operations/install.md`.

- [ ] **Step 6: Run the tests.**

Run: `cargo test -p mllm-management --all-targets --locked && cargo test -p mllm-cli --test errors --locked`
Expected: all pass, including the three new tests; `registry_resolves_every_allowed_host_the_selector_matches` still answers 403 for its selector miss (its profile `local` is published).

- [ ] **Step 7: Core suite, workspace, Clippy.** Run the three Global Constraints commands. Expected: pass.

- [ ] **Step 8: Commit**

```bash
git add crates/mllm-management crates/mllm-cli docs/operations/install.md
git commit -m "feat(management): refuse a deploy whose runtime profile no host publishes

ADR 0018 section 7 (owner decision 2026-09-25): a deploy naming a
runtime profile that no allowed host publishes is refused at once with
profile_not_published (CLI exit 24), storing nothing. The refusal names
the profile, each host with the profiles it publishes, and the fix:
mllm engine add <path> --name <profile>, then deploy again. Deployments
are never re-resolved after engine add."
```

---

## Self-review

**Spec coverage** (spec as revised in this PR).

| Spec item | Task |
|---|---|
| `engine detect`: locations (including home-level venvs), metadata only, bounds, no symlink escape | 5; CLI 14 |
| `engine add`: resolve, detect, register (version check, fingerprint, probe), name, custom mark, write `engines.yaml` (lock, atomic, revision), reload | 4, 3, 2, 14, 13, 15 |
| Role document never rewritten; `engines.yaml` location rule; name in both files refused | 2, 13, 14, 15 |
| Options `--deep-park`, `--drift`, `--arg` | 3, 14 |
| Probe reports missing deep park → restart_only | 14 |
| Interactive pick; `not_interactive` | 14 |
| `engine list` columns | 13 (role side), 14 |
| `engine remove [--drain]`: two phases, no placement slips in, stop evidence only; never-published removed locally; operator-declared profiles not removable | 7, 10, 11, 13, 14, 15 |
| `list engines` on the server | 11, 14 |
| Environment variables: one → `local`; both → `local-vllm`/`local-sglang`; collision → `profile_exists` at start | 15 |
| Control socket: 0600, `SO_PEERCRED`, add/remove/list only | 12, 13, 15 |
| Re-publish validated like a startup publish; accepted swaps; rejected keeps the snapshot; `publish_rejected`; `not published` in list | 8, 9, 13, 14 |
| `agent_unreachable` (add writes `engines.yaml`; remove writes nothing) | 13, 14 |
| `live_profile_update` and the restart fallback | 6, 9, 13, 14 |
| Deploy fails fast on an unpublished profile; no re-resolution | 17 |
| Closed codes; exit codes 16–24, 9 unused | 14, 17 |
| Security rules | 4, 5, 12; Global Constraints |
| ADR 0018 amending SPEC §4.2 and §15.1 | 1 |
| Spec's CPU test list | 2, 3, 4, 5, 7, 9, 10, 11, 12, 13, 14, 15, 17 |
| Spec's live checks | 16 (ENG1–ENG4) |

**Open item:** decision 20 (exit 24 for `profile_not_published`). **Known limit:** ENG2 checks that standalone publishes both environment profiles and refuses to remove one. Its "switch between them" step is written as a note, to be added once Task 15 has fixed the standalone deployment shape.

**Type consistency** (names checked across tasks): `ENGINES_FILE`, `engines_beside`, `engines_path`, `config_home`, `EnginesFile`, `lock_engines`, `write_engines`, `merge_into_host`, `revision_of`, `HostConfig::load`, `ConfigKind::Engines` (2 → 13, 14, 15); `ProfileSpec`, `profile_document`, `check_profile`, `is_verified`, `valid_profile_name`, `ENVIRONMENT_PROFILES`, `only_profiles_differ`, `added_profiles`, `removed_profiles` (3 → 8, 9, 11, 13, 14, 15); `Resolved`, `resolve`, `check_version`, `VERSION_CHECK_TIMEOUT`, `Candidate`, `ScanRoots`, `ScanBounds`, `detect` (4, 5 → 14); `LIVE_PROFILE_UPDATE`, `server_capabilities`, `MAX_REQUEST_ID`, `MAX_REASON`, `MAX_RETIREMENT_DEPLOYMENTS` (6 → 9, 10, 13); `RetirementStart`, `RetirementProgress`, `RetirementCandidate`, `begin_profile_retirement`, `profile_candidates`, `record_profile_retirement_stops`, `profile_retirement_progress`, `cancel_profile_retirement`, `profile_retirement`, `expire_profile_retirements` (7 → 8, 11, 13, 15); `RepublishRefusal`, `republish_host_configuration`, `host_publication::republish`, `republish_inventory` (8 → 9); `RetirementStep`, `ProfileRetirements`, `RETIREMENT_POLL`, `with_profile_retirements` (10 → 11, 13, 15); `StoreRetirements` (11 → 13, 15); `ControlRequest`, `ControlHandler`, `ControlServer`, `request`, `SOCKET_NAME` (12 → 13, 14, 15); `ProfileSet`, `HostProfiles`, `ProfileUpdates`, `PublishOutcome`, `RetireOutcome`, `run_session_with_updates`, `HostControl` (13 → 15 reply shapes); `RoleKind`, `Target`, `resolve_target` (14); `NamedInstallation`, `EmbeddedInstallations`, `EmbeddedHost`, `StandaloneControl` (15); `ConfigurationFailure::ProfileNotPublished`, `ExitCode::PROFILE_NOT_PUBLISHED` (17).
