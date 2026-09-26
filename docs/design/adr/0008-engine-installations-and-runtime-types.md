# ADR 0008 — Engine families, installations, and runtime types

**Status:** Accepted (2026-09-16)
**Amends:** `SPEC.md` sections 1.2, 2, 3.2, 8, 14. The amended architecture supersedes
those sections where they differ, except the two carve-outs named under Decision.

## Context

mllm natively understands a small set of engines, but any number of stock or custom
builds of those engines must coexist on one host — `vllm-stock`, `vllm-exl3`,
`vllm-dev` and `sglang` side by side. A custom build must not become a new engine
type, and the way an engine is started (a command today, a container later) must not
be confused with which engine it is.

Most of this already exists under different names. `RuntimeProfile` in
`crates/mllm-config/src/effective.rs` carries engine, executable, build fingerprint,
args, env, security and revision, and profiles are already a name-keyed map, so
multiple builds of one engine already coexist. `Engine` in
`crates/mllm-config/src/engine_policy.rs` is already the family discriminator and
already holds per-family knowledge such as reserved flags. The gaps are a runtime-type
discriminator, a family-to-adapter resolution layer, and model sourcing.

## Decision

**Terminology.** Adopt these names on every user-facing surface — CLI, management API,
configuration and documentation:

| Name | Meaning | Today |
|---|---|---|
| Engine family | What mllm knows about an engine | `Engine` enum, `engine_policy.rs`, `EngineAdapter` |
| Engine installation | One build of one family on one host | `RuntimeProfile`, keyed by name |
| Runtime type | How that installation is started | new; `executable` implies command today |
| Model source | Where the weights come from | new |

"Engine installation" replaces "runtime profile" as the term of record. Internal Rust
type names may lag, but no new surface introduces a second name for one concept.

**Runtime type.** An installation declares `runtime.type`. `command` carries an
executable path; `docker` carries an image reference. A custom build is never a runtime
type: `vllm-exl3` is engine family vLLM, runtime type command. Launcher and adapter
stay separate traits, as they are now.

**Model source.** A deployment declares its source: `local` with a path, `huggingface`
with repo and revision, or `http`. The host configures a model store; the agent resolves
a source to a local path, reusing a cached copy when present. Docker is never a model
source. The model store is a charged filesystem resource owner under `SPEC.md` §7, not
untracked disk.

**Engine configuration.** A deployment owns its `engine_config`, whose schema comes from
its engine family, rather than referencing a separately authored reusable recipe. The
frozen, fingerprinted recipe remains an internal artifact derived from the deployment at
admission: qualification identity depends on `recipe_fingerprint`, and that is unchanged.

**Discovery.** The agent may discover installed engines and present them for explicit
selection. Discovery is bounded and engine-aware, never a filesystem scan. Manual
registration produces the same resource as discovery.

**Two carve-outs where `SPEC.md` still governs.**

1. *Delivery order.* §18 places container and service launchers in slice F5. The runtime
   type abstraction lands now; the docker launcher is still implemented in F5 order. A
   scope decision is not a schedule decision.
2. *Discovery safety.* §4.2 forbids silently installing engines or executing discovered
   scripts. Discovery may read and present; it may not run a discovered binary to
   fingerprint it without host-policy authorization.

**Scope change recorded explicitly.** §1.2 lists implicit checkpoint download as a
non-goal. A declared model source is explicit user intent, so materializing it is
permitted. Implicit or silent download, and any quantization, remain out of scope.

## Consequences

- Add a `RuntimeType` discriminator; `executable` becomes the command variant's field.
- Add a family-to-adapter resolution layer. Adapters are currently selected at hardcoded
  construction sites, which is the real blocker for a third engine.
- Add model source resolution and a host model store with filesystem accounting.
- CLI gains engine installation management alongside discovery.
- `SPEC.md` §2 gains engine family, engine installation, runtime type and model source;
  "model recipe" narrows to the internal frozen artifact.

## Amendment 2026-09-23: installation identity without pinned hashes

**Owner decision** (after reviewing which files the permission checks guard):
mllm's private state stays strict; mllm's own runtime helper scripts use the
owner-only rule everywhere, including the SGLang entry path check; engine
installation files get no hard-coded hashes and no permission rule. This
replaces the pinned SGLang 0.5.20 source audit (`runtime/sglang_source_preflight.py`,
with the pinned saver source inventory `runtime/saver_source_preflight.py`),
which refused any custom or patched SGLang build and any group-writable
installation. It also reconciles ADR 0014 §9, which had kept that audit with a
per-installation source manifest generated at registration.

**Fingerprint at registration.** When a host registers its installations
(agent start), it measures each one: the engine package's version from its
`*.dist-info` metadata and a `sha256:` digest over a canonical manifest of the
package's files (relative path, size and SHA-256, sorted; bytecode caches left
out), bounded, reading bytes and metadata only and never running the
installation (carve-out 2). The host publishes version, digest and state
(`measured` or `unmeasured`) with each installation, and the host view shows
them. An installation that cannot be measured is `unmeasured`, never refused.

**Drift.** Every launch measures again. A different digest is drift: the host's
status marks the installation `drifted` with the observed digest, and the
controller journals an `installation_drift_flagged` event. Drift refuses the
launch (closed reason `installation_drift`) only when the installation's host
policy says `security.installation_drift: refuse`; the default is `warn`.

**Capability probes.** Each internal API mllm hooks is probed at launch by
shape (`runtime/engine_capabilities.py`): the module imports, the attribute or
method exists and is callable, the record declares the field, the router serves
the route, the metrics module names the gauge. Capabilities are closed per
family: `core` (what every launch needs), `deep_park` (SGLang's memory saver
adapter and saver package, release, resume, reload-from-disk and flush routes;
vLLM's sleep and middleware destinations and sleep, wake, collective RPC and
prefix-cache routes), `metrics` (the scraped load gauges) and, for SGLang,
`observation` (the scheduler and saver shapes the allocation observer binds).
A missing capability refuses only the dependent feature with a typed closed
reason: `capability_missing:deep_park` refuses a `deep` launch (declare
`restart_only`) and any Park, which leaves the launch `unchanged`;
`capability_missing:core` refuses every launch. Core serving on a build without
the saver hooks stays available. The host probes before admitting a launch
whose tier depends on a gated feature, under the installation's own interpreter
with a bounded time; the protected entries probe again at startup. A probe that
cannot run is unknown and refuses nothing by itself.

**Permissions.** The owner-only rule (owned by root or the service user, never
other-writable, group-writable only through the owning user's private group)
has one Rust statement, `mllm_adapters::owner_only`, mirrored for Python by
`runtime/owner_only.py`. It covers the runtime directory and its modules, the
protected SGLang entry and its ancestors, the reviewed saver library binding and
the observation listener's ancestor directories. Private state that mllm
creates 0600/0700 (identity storage, management credentials, remote role
files, launcher lock files, observation sockets and their directory) keeps the
strict rule with no group write.

**Consequences.** `RuntimeProfileStatus` gains installation version, digest,
state, observed digest and missing capabilities (additive). A reconciled host
may change only the drift and capability fields. The SGLang descriptor's
`source_revision` literal remains as a wire token and no longer identifies or
constrains the installed build. Standalone mode probes capabilities in the
protected entries but does not yet record an installation fingerprint.
Passing fingerprints and probes are not qualification evidence.

## Amendment 2026-09-24: materializing declared model sources

**Declaration.** A deployment's `model.source` is `local` (`path`), `huggingface`
(`repo`, `revision`, optional `files` allow patterns, optional `token_ref`) or
`http` (`url`, `sha256`, optional `archive: none | tar`). Both the tagged spelling
(`{type: huggingface, ...}`) and the keyed spelling (`{huggingface: {...}}`) are
accepted; the frozen revision keeps the tagged form, so existing fingerprints do
not change. Only pinned sources are accepted: `revision` is a full 40-character
commit SHA (a branch or tag is refused, and `locked_commit` is retired), `sha256`
is 64 lowercase hex, URLs are `https://` without credentials, and `token_ref` is a
`secret://<name>` reference, never a value.

**Host policy.** A host opts in per kind: `model_sources: {huggingface:
allowed|denied, http: allowed|denied, max_bytes, allowed_hosts,
huggingface_endpoint}`. Both kinds default to `denied`, and a deployment with a
denied source does not resolve on that host (`model_source_denied`). A host that
allows either kind must state `max_bytes`, the model store's ceiling for
materialized sources. `allowed_hosts`, when stated, lists the origins a source may
name (the hub's host for Hugging Face).

**Store layout and accounting.** A remote source resolves to a fixed directory in
the host's model store: `sources/huggingface/<owner>--<name>@<sha>` (with a short
digest of the allow patterns when they narrow the files) or `sources/http/<sha256>`
(`-tar` for an archive). The store is the charged filesystem resource owner of
SPEC §7: before any byte is written, the download's full size (from the hub's
listing, or the response length; an origin that states none is refused
`size_unknown`) is reserved against `max_bytes` (verified copies plus reservations
in flight) and the filesystem's free space, and the reservation is persisted. It
is released only when the temporary directory is verifiably gone, or converted
into the verified copy's charge on commit. A transient failure keeps its partial
files and reservation for a resume; a terminal one (`hash_mismatch`, `too_large`,
`not_found`, `unauthorized`, `invalid_listing`, `unsafe_archive`, ...) removes
them first.

**Verification.** Hugging Face LFS files are checked against the SHA-256 in their
pointer, other repository files against their git blob id, and an `http` payload
against its declared SHA-256. A tar archive is verified whole, then extracted by a
reader that accepts regular files and directories under relative paths only. The
verified tree is renamed into place atomically; the ADR 0014 §7 checkpoint digest
is then measured over it like any local checkpoint. Engines are unchanged: they
read the local directory, with the offline environment they always had.

**Protocol and control.** `MaterializeSource` is an additive member action (field
14 in `ExecuteMember`, result evidence field 14 in `MemberExecutionResult`), sent only to hosts whose `Connect` declares
`model_sources`. It carries the deployment document, never a path or a secret; the
host checks its own policy and answers at once with `pending`, `downloading`
(bytes done and total), `verified` or `failed` with a closed reason. A download it
starts keeps running; concurrent requests share it, and a request after an agent
restart resumes it with range requests. The server records each answer per host;
status shows them under `model_sources`. Activation waits
(`model_source_pending`) until one host holds a verified copy and is refused
(`model_source_failed`) once every attempt failed terminally; the checkpoint digest
is measured only where the copy is verified. A placement on another host
materializes there first, before anything is launched.

**Secrets.** A `token_ref` resolves from `<state_dir>/secrets/<name>` on the host,
an owner-only file (no group or other access). The value is sent only as a
sensitive `Authorization` header (dropped on a cross-host redirect) and never
written to a file, journal, status, error or log line.

**Reclaim.** Deleting a deployment never deletes its copy (SPEC §6.3).
`mllm prune sources --host-config <host.yaml> [--apply]` is the explicit,
host-side reclaim: it removes only verified copies under `sources/` that no
existing deployment references (the server's `GET /management/v1/model-sources`,
or `--referenced-file`), lists them unless `--apply` is given, and skips a copy
whose download lock is held.

**Not yet.** The standalone role's generated host document states no
`model_sources`, so a standalone deployment with a remote source is refused.
The server's placement planner does not plan disk; the host enforces the store
ceiling locally. A live Hugging Face download on a Spark has not been run, and
the CPU tests (a local fake hub and origin) are not qualification.

## Amendment 2026-09-25: sources allowed by default, a store of their own

Owner decisions of 2026-09-25, applied to every host alike (standalone is a
server plus one host):

- **Default on.** Hugging Face and HTTP sources are `allowed` unless a host
  states `denied` (also spelled `disabled`); an explicit value in an existing
  host document wins over the default. `max_bytes` defaults to 500 GiB when a
  host states none, so an allowed download is always bounded.
- **Three ways, one rule.** The switch is `--model-sources allowed|disabled`
  on `start host` and `start standalone`, `MLLM_MODEL_SOURCES`, or the
  document's `model_sources.huggingface` / `model_sources.http`; the ceiling is
  `--model-sources-max`, `MLLM_MODEL_SOURCES_MAX` or `model_sources.max_bytes`.
  Precedence is flag > environment > document > default
  (`mllm_config::model_settings`).
- **Sources store.** Downloads live in `<state_dir>/models/sources` unless
  `model_sources.path` names another directory; the model store keeps the
  operator's own checkpoints. A downloaded checkpoint is contained by, and its
  digest measured against, the sources store. The role writes the resolved
  `model_store.path` and `model_sources` (including `path`) into the host
  document it publishes, so the server resolves a revision against exactly what
  the host enforces. A document that states neither keeps the earlier layout
  (`<model_store>/sources`).
- **Disk check.** A reservation also leaves 1 GiB free on the filesystem
  (`insufficient_space` otherwise), besides the `max_bytes` ceiling
  (`too_large`).
- **Standalone.** The embedded host materializes declared sources in process
  exactly as an enrolled host answers `MaterializeSource`, so a standalone
  deployment with a remote source is accepted (provisionally where its memory is
  sized from the weights, ADR 0014 §7) and activates once the copy is verified.
  The "Not yet" note above about standalone no longer applies.

