# S3 — Ordinary SGLang launch: design

Status: approved. Owner authorized the full slice on 2026-09-19. This document
records a blocking discovery made while surveying the code, and designs the
ordinary SGLang launch path around it.

## 1. The blocking discovery

The previous status runbook (`docs/runbooks/f2-current-status.md`) recorded that
SGLang "has a complete `execute_persisted`" and that `ProfileBindings` refuses it
only because "nothing resolves its admin credential and no trusted observation
socket is supplied" — two inputs to wire. That is incomplete, and acting on it
would have produced a launch that cannot start.

SGLang native startup is deliberately and comprehensively closed. The entrypoint
`runtime/sglang_entry.py` raises `pinned_source_contract_unavailable` from both
`_verified_native_contract` and `_import_and_launch`:

```python
def _verified_native_contract(spec, checkpoint):
    """A pinned source map and real memory-saver observation are mandatory.
    ... Replacing this denial requires the complete audited startup and
    observation contract."""
    raise LaunchError("pinned_source_contract_unavailable")
```

`runtime/README.md` states the same posture in prose: "Native startup remains
closed; production composition must select the protected installed package root
and consume/revalidate this observation alongside the remaining gates," and "No
native entrypoint is enabled by their presence." AGENTS.md makes it a hard
constraint: "Native entrypoint denials stay closed until their prerequisites are
met."

So there are three blockers, and the third is security-gated:

1. `crates/mllm-controller/src/engine_bindings.rs:191` refuses `Engine::Sglang`,
   because there is no ordinary launch path to build.
2. The trusted `SglangRuntimeObserver` has no production construction; the
   adapter is built before the arm that would give it a token.
3. The native entrypoint refuses to start until the audited startup and
   observation contract is composed. This is the gate, and it needs owner
   authorization to open.

This design therefore has two parts. Part A is the ordinary SGLang launch path on
the mllm side; it is ordinary product code and can be built and tested without
touching the gate. Part B is the audited native startup contract that replaces
the denial; it is security-sensitive, it is the prerequisite for any live SGLang
launch, and it is not authorized by this document.

## 2. Goal and scope

Goal: a single-host SGLang deployment that the coordinator can start, prove
ready, serve inference through, and stop with the group proven gone — the SGLang
twin of the vLLM run 6.

In scope:

- The ordinary launch path for `Engine::Sglang`, mirroring the vLLM shape that
  S1 landed (`ProfileBindings::spec` → `AdapterSpec` → adapter `initialize`).
- Fresh per-launch inference and admin credentials, sealed under the binding.
- Readiness by the served name in SGLang's model list, plus one authenticated
  inference probe.
- Cleanup through the existing identity-based gone-proof.
- The audited native startup contract as a separately authorized prerequisite.

Out of scope, and deliberately not swallowed:

- Park, restore, wake, switch (S2). SGLang's adapter `park`/`restore` stay
  `UnsupportedCapability`; the observer is not wired here.
- Remote control and host agents (F3).
- Two-node groups and distributed park (F4).
- Any change to the deep-park security gate (SPEC §9.1, T21).

## 3. What exists

The SGLang code is not greenfield. The following are already written and tested
at the contract level:

- `crates/mllm-adapters/src/sglang/` — `SglangAdapter`, `SglangLaunch` (frozen
  plan renderer using protected descriptor fds), `args.rs`, `http.rs`
  (admin-keyed control endpoints), `forward.rs`. 943 lines.
- `crates/mllm-controller/src/sglang_observer.rs` — `NativeResidencyObserver`
  and `NativeSglangObserver`, a residency fusion over committed milestones,
  request leases, physical saver facts, and group liveness.
- `crates/mllm-launchers/src/native_observation.rs` — `NativeObservationClient`,
  the protected Unix-socket transport to the observation server.
- `runtime/sglang_entry.py`, `runtime/sglang_observation_server.py`,
  `runtime/sglang_observation_transport.py`, `runtime/sglang_scheduler_observer.py`,
  `runtime/sglang_saver_binding.py`, `runtime/sglang_source_preflight.py`,
  `runtime/sglang_startup_guards.py`, `runtime/sglang_device.py`,
  `runtime/checkpoint_preflight.py`.
- The `NativeLaunch` / `NativeLaunchSource` / `NativeLaunchHandoff` path in
  `crates/mllm-controller/src/runtime.rs`. This is complete and heavily tested
  (`tests/runtime_binding.rs`), but it is **test-only**: `rg` finds no production
  caller. It is built around the deleted candidate path — wire kind
  `sglang_candidate_private_launch`, served name `candidate-{binding_id}` — which
  the status runbook already lists as leftovers.

The ordinary path is the one production runs. `crates/mllm-cli/src/roles.rs:540`
calls `OwnedCoordinator::spawn_resolved`, which builds a driver from
`EngineBindings::spec` and `EngineBindings::adapter`
(`crates/mllm-controller/src/coordinator/worker.rs:620`). vLLM is served by
`ProfileBindings` returning `AdapterSpec::Vllm { launch: PlanInputVllm,
engine_key, .. }` and `VllmAdapter` launching through `OwnedProcessLaunch`.

SGLang has no equivalent. `SglangAdapter::execute_persisted` first asks
`action_timeout(command.action)`; for `Initialize` that returns
`RuntimeError::Unsupported` (`crates/mllm-adapters/src/sglang/http.rs:105-115`)
before any control arm is reached, so the adapter has no launch path at all. The
control arms it does have (Drain, Park, Restore, ReloadWeights, InvalidateCache,
Probe) act only on an engine somebody else started.

## 4. Part A — the ordinary SGLang launch path

### 4.1 Shape

Mirror vLLM's S1 shape exactly, so the two engines share one coordinator path and
one set of lifecycle invariants:

1. `ProfileBindings::spec` builds an `AdapterSpec::Sglang` from the frozen
   effective configuration and the binding's own leased endpoint — never from
   ambient configuration, for the same reason `engine_bindings.rs` gives for
   vLLM.
2. The adapter gains a launch capability (`with_launch` / `with_tools` /
   `with_credentials`), and `execute_persisted(Initialize)` delegates to a new
   `crates/mllm-adapters/src/sglang/initialize.rs`.
3. `initialize` renders the command, spawns through `OwnedProcessLaunch`, waits
   for readiness while watching the process, probes once, enumerates the group,
   and reports identities and milestones — the same sequence as
   `crates/mllm-adapters/src/vllm/initialize.rs`, with SGLang's renderer and
   readiness rule.

The renderer is the existing `SglangLaunch`. The frozen `NativeLaunch` it needs
is built in `ProfileBindings::spec` from `work.effective()`. Every existing
`NativeLaunch` is built only in tests; there is no production builder, so Part A
names each metadata field's derivation:

- `engine`, `recipe`: from the profile's engine and the pinned recipe constant
  (`mllm-config::effective::sglang::NATIVE_SGLANG_RECIPE`).
- `source_revision`, `checkpoint_revision`: the pinned constants
  `NATIVE_SGLANG_SOURCE_REVISION` and `NATIVE_CHECKPOINT_REVISION`.
- `binding_id`, `incarnation`, `endpoint`: from `work`.
- `served_name`: `candidate-{binding_id}` in this slice (see §4.5).
- `rendered_settings_digest`: a digest over the frozen launch settings, computed
  the same way the effective-configuration fingerprint is.
- `device`: mapped from the profile's selected device and the host policy's
  hardware fingerprint into `NativeDeviceSelection`.
- `checkpoint_root`: the resolved model path; `executable`: the profile's engine
  binary; both credential refs: the minted keys' references (see §4.2).

The renderer itself must also change: `SglangLaunch::from_frozen` validates the
served name as `candidate-{binding_id}` (`crates/mllm-adapters/src/sglang/args.rs:82`),
so the "reuse" is the reviewed shape, not an untouched file.

`SglangAdapter::check_readiness` is a permanent `Initializing` stub
(`crates/mllm-adapters/src/sglang/adapter.rs:303-307`). Part A implements it: the
served name in SGLang's model list means Ready, unreachable means Initializing,
any other error is an error. Without this, an initialize loop built on the vLLM
sequence would never become ready.

### 4.2 Credentials

SGLang needs two credentials, not one: an inference key for `/v1` and an admin
key for the control endpoints. Both are minted fresh per launch, mirroring
vLLM's `new_engine_key` (`crates/mllm-store/src/secrets.rs:101`), and both are
sealed under the binding before the child exists.

The store today holds one secret per binding: `engine_secrets` is keyed by
`binding_id` with `incarnation` carried as a column and used as associated data
(`crates/mllm-store/src/schema.rs:341-347`, `secrets.rs:143`). This design adds a
role to that key — `inference` and `admin` — so a restart can recover both and a
failing launch can delete both. The primary key becomes `(binding_id, role)`. The
alternative, one combined secret, is rejected: the two credentials have different
audiences and the adapter must be able to present them separately. This is a
schema change and takes a schema version bump with the migration the store
already patterns for such changes; the existing vLLM secret becomes the
`inference` role, so no existing row is orphaned.

Sealing is currently wired for vLLM alone: `spawn_resolved` seals
`AdapterSpec::Vllm { engine_key }` (`crates/mllm-controller/src/coordinator/worker.rs:647-670`),
and the binding carries a single `credential_ref`. Part A must add the SGLang
case: both minted keys sealed before the child exists, both deleted when the
launch is released. There is no production service object to reuse for this —
the arm path's "resolver" is a `&dyn Fn(&str) -> Result<Vec<u8>>` parameter
(`crates/mllm-controller/src/runtime.rs:212`) and `NativeLaunchHandoff` has no
production caller — so Part A names the resolver it adds and where it lives.

### 4.3 Readiness and the inference probe

SPEC §6.1 governs: an HTTP server that is up is not a model that is ready. The
readiness rule is the served name appearing in SGLang's model list, exactly as
vLLM's is; unreachable means `Initializing` (the launcher owns crash detection);
any other error is an error, never a quiet `Initializing`. Once the model lists,
one authenticated chat completion is sent through the same path inference will
use, and empty content fails the step.

### 4.4 The observer is not in this slice

The trusted `SglangRuntimeObserver` is needed by control actions — drain, park,
restore, reload, invalidate, probe — none of which are S3. The adapter's
`Initialize` path does not consult it. Part A therefore makes the observer
optional on the adapter. This is a type change, not a paragraph: the field
becomes `Option<Arc<dyn SglangRuntimeObserver>>` on `SglangAdapter`,
`AdapterSpec::Sglang`'s `observer` becomes optional
(`crates/mllm-adapters/src/resolve.rs:50-55`), and `resolve` builds the adapter
without it when absent. An adapter built for launch answers its control actions
with the honest refusal: `execute_persisted` returns `RuntimeError::Unsupported`,
and `park`/`restore`/`reload_weights` keep returning
`AdapterError::UnsupportedCapability`. `prepare_park` currently returns
`Ok(Quiescence { quiescent: false })` (`adapter.rs:308-310`) and is not a refusal;
Part A leaves it as-is rather than claiming a posture it does not have. The
observer's production construction, and the arm-timing problem it carries (the
driver is built at `worker.rs:1603`, before `arm_initialize_with_context` at
`:1626`), are S2's to solve, when park first needs a token.

### 4.5 Wire kinds, served name, and the gated file

The ordinary path launches through the protected wrapper
(`SglangLaunch::render_for_launcher` with the service wrapper path) because the
protected descriptor fds and the memory-saver binding live there.

The SGLang descriptor contract is candidate-shaped, and the gated entrypoint
validates all of it. Two wire kinds exist and both are checked by
`runtime/sglang_entry.py`: the private descriptor kind
`sglang_candidate_private_launch` (`runtime.rs:246`, checked at
`sglang_entry.py:275`) and the public-settings kind `sglang_candidate_launch`
(`args.rs:121`, checked at `sglang_entry.py:155`). The served name is
`candidate-{binding_id}`, validated on both sides — `sglang_entry.py:169` and
`sglang/args.rs:82`.

Part A therefore reuses the candidate kinds and the candidate served name
unchanged. It renames nothing in the descriptor contract, so it edits no
security-gated Python and no validated literal. The ordinary naming —
`sglang_private_launch`, the deployment's route name — is Part B's, done with
the entrypoint composition and under the same authorization, and it is the point
at which both the Rust literals and the Python validators change together.

The descriptor JSON that `NativeLaunchHandoff::arm` builds inline is extracted
into one builder both paths call, so Part A reuses the reviewed shape rather than
copying it.

## 5. Part B — the audited native startup contract

Authorized by the owner on 2026-09-19 ("go ahead, you implement everything").
The authorization covers building the contract and, once its gates are composed
and audited, replacing the denial. The ordering constraint from AGENTS.md holds:
the entrypoint denial stays closed until the prerequisites are met, so the
denial is the **last** thing this part changes, never the first.

The contract the denial names, from `runtime/README.md` and the runtime module
docstrings, has these parts:

1. **Protected installed package root.** Select and pin the immutable
   package/metadata/search roots, without executing `.pth` hooks or
   `site.main()`, and revalidate the pinned source map
   (`sglang_source_preflight`, commit
   `fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`) immediately before imports.
2. **Worker enrollment.** The protected `sglang_entry` main path must survive
   CPython spawn's `__mp_main__` preparation and repeat the preimport plugin
   guards in every spawned interpreter before native `Process` arguments are
   unpickled.
3. **Plugin closure.** `enforce_closed_plugins()` must reject both installed
   entry-point groups and nonempty selectors in every spawned interpreter.
4. **Physical placement.** `sglang_device.observe_placement` must recollect the
   NVIDIA inventory and require the service-authorized UUID digest and that exact
   UUID in `CUDA_VISIBLE_DEVICES`.
5. **Service observation composition.** The enrolled scheduler's saver, the
   scheduler safe-point bridge, the transport, and the protected Unix listener
   must be composed and attached at startup.
6. **Checkpoint revalidation** immediately before loading, before any protected
   wrapper effect.

Until all six are composed and audited, `sglang_entry.py` keeps raising
`pinned_source_contract_unavailable`, and the SGLang live gate cannot be met.
This design does not enumerate the full contract's implementation; that is its
own spec and plan, and its own review, because it is security-relevant.

## 6. Decisions

- **Ordinary path, not the legacy handoff.** The candidate-path
  `NativeLaunchHandoff` plumbing is not wired into production and is built around
  retired naming. S3 builds the ordinary path and leaves the handoff for
  retirement with the rest of the candidate leftovers.
- **Fresh per-launch credentials, not a host credential file.** Consistent with
  vLLM and with least privilege: every launch is isolated, and a leaked key
  cannot be replayed against the next launch.
- **Two credential roles in `engine_secrets`.** See §4.2.
- **Observer optional in S3.** See §4.4.
- **The gate opens last.** Owner-authorized 2026-09-19. The six gates of §5 are
  composed, tested and audited first; the denial in `runtime/sglang_entry.py` is
  replaced only after they hold, and the rename to ordinary naming rides with
  that composition. No task opens the gate before its prerequisites exist.

## 7. Verification

CPU:

- A `sglang_initialize` test drives the full launch path against a stub process
  and a stub readiness surface: exact rendered argv, both credentials sealed and
  recoverable, readiness by served name, one probe, group enumeration, milestones
  `[AllocationsRestored, WeightsUsable, CacheValid, ModelUsable]`.
- A contract test asserts the served name is `candidate-{binding_id}` and both
  descriptor kinds are the ones `runtime/sglang_entry.py` already accepts, so
  Part A provably touches no security-gated Python and no validated literal.
- A test asserts that an adapter built without an observer refuses its control
  actions (`RuntimeError::Unsupported` from `execute_persisted`,
  `AdapterError::UnsupportedCapability` from `park`/`restore`/`reload_weights`)
  rather than appearing to grant park.

Live (blocked on Part B):

- On `host-a`, launch SGLang 0.5.19 with `qwen3-4b-instruct` through the
  coordinator, confirm Ready, serve one inference through the router, stop, and
  confirm the group is proven gone and memory returns. This run is the SGLang
  twin of the vLLM run 6 evidence, and it cannot be attempted until the owner
  authorizes and the audited contract lands.

CPU and Fake-engine tests are a pre-check only, never the claim that a native
engine recipe works (SPEC §18).

## 8. Owner decisions

1. **Part B authorized** on 2026-09-19. Resolved.
2. **Route order.** Carried over from S1: frozen revisions sort routes
   alphabetically, so `--served-model-name` is the alphabetically first route,
   not document order. Unchanged here, but SGLang inherits it.
