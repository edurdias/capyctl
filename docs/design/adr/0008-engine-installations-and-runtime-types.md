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
