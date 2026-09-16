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
