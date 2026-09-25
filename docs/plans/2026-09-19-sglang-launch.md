# S3 — SGLang ordinary launch Implementation Plan

**Goal:** A single-host SGLang deployment the coordinator starts, proves ready, serves through, and stops with the group proven gone — the SGLang twin of vLLM run 6.

**Architecture:** Mirror the vLLM ordinary path (`ProfileBindings::spec` → `AdapterSpec` → adapter `initialize` → `OwnedProcessLaunch`). Part A (Tasks 1–5) is the Rust launch path. Part B (Tasks 6–9) composes the audited native startup contract in `runtime/sglang_entry.py` and replaces the `pinned_source_contract_unavailable` denial — the owner authorized this on 2026-09-19, with the constraint that the denial opens only after the gates hold.

**Tech Stack:** Rust (tonic-free; tokio, axum already present), Python 3.12 stdlib for runtime composition, SQLite store, pytest for runtime tests.

> Retired 2026-09-23: `crates/mllm-cli/tests/live_sglang.rs` and the
> `scripts/live/run-on-spark.sh` runner are deleted by owner decision (2026-09-22). The
> SGL1–SGL3 gate is matrix row M73 on an SGLang fixture, driven through the shipped CLI
> and roles; see `docs/plans/2026-09-22-two-host-engine-matrix.md`, Tier 8.
> This plan is a historical record.

**Spec:** `docs/specs/2026-09-19-sglang-ordinary-launch-design.md`

## Global Constraints

- SPEC §6.1: an HTTP server being up is not model readiness; readiness is the served model answering.
- SPEC §3/§13.3: credentials ride protected descriptors or the child environment, never argv; redacted from journals (`mllm_adapters::vllm::args::redact_text` is the reference).
- SPEC §18: CPU and Fake-engine tests are a pre-check, never native qualification evidence. Say so in any status claim.
- The entrypoint denial is replaced **last** (Task 9), after Tasks 6–8 hold. No task before that edits `_verified_native_contract` or `_import_and_launch`.
- Part A (Tasks 1–5) edits no Python under `runtime/`.
- Schema changes follow the migration pattern in `crates/mllm-store/src/migrations.rs` (`MIGRATIONS` array; one entry per version).
- Prose in documents and commit messages is normal English. Test IDs cited as `// Txx` where an acceptance-matrix entry exists.
- Core suite command: `cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`. Clippy: `cargo clippy --all-targets -- -D warnings` over those crates.
- Excluded files (do not read/edit/stage): `crates/mllm-cli/tests/live_interactive.rs`, `.superpowers/sdd/2026-09-12-f2a2d-coordinator-integration/task-2-report.md`.

## File Structure

- `crates/mllm-store/src/schema.rs`, `migrations.rs`, `secrets.rs` — credential roles.
- `crates/mllm-controller/src/native_launch.rs` (new) — production `NativeLaunch` builder from the frozen effective configuration, shared descriptor builder.
- `crates/mllm-controller/src/engine_bindings.rs` — SGLang branch of `spec`.
- `crates/mllm-controller/src/coordinator/worker.rs` — seal both SGLang roles; protected tools.
- `crates/mllm-launchers/src/owned_launch.rs` — descriptor-capable spawn.
- `crates/mllm-adapters/src/traits.rs` — `spawn_durable_protected` on `OwnedProcessLaunch`.
- `crates/mllm-adapters/src/resolve.rs`, `sglang/adapter.rs`, `sglang/initialize.rs` (new) — adapter launch capability.
- `runtime/sglang_entry.py` + new `runtime/sglang_native_composition.py` — the contract and the opened gate.
- `crates/mllm-cli/tests/live_sglang.rs` (new) — the live gate.

---

### Task 1: Credential roles in `engine_secrets`

**Files:**
- Modify: `crates/mllm-store/src/schema.rs` (add `SCHEMA_V15`), `crates/mllm-store/src/migrations.rs`
- Modify: `crates/mllm-store/src/secrets.rs`
- Test: `crates/mllm-store/src/secrets.rs` (tests module), `crates/mllm-store/tests/` if schema tests live there

**Interfaces:**
- Produces: `Store::store_engine_key(&self, binding_id: &str, incarnation: &str, key: &[u8; 32], role: SecretRole)`, `Store::engine_key(&self, binding_id: &str, incarnation: &str, role: SecretRole) -> Result<Option<[u8;32]>, StoreError>`, `Store::delete_engine_keys(&self, binding_id: &str)`, `pub enum SecretRole { Inference, Admin }` with `as_str()` returning `"inference"`/`"admin"`. Existing vLLM call sites pass `SecretRole::Inference`.
- Consumes: existing `new_engine_key()`, XChaCha sealing with `binding_id`+`incarnation` as AAD (role joins the AAD so a row copied between roles does not authenticate).

- [ ] **Step 1:** Failing tests in `secrets.rs` tests module: round-trip both roles independently; a row copied between roles fails to authenticate (AAD check); `delete_engine_keys` removes both; existing single-key tests updated to the new signatures.
- [ ] **Step 2:** Run `cargo test -p mllm-store secrets` — expect compile errors (signature changes).
- [ ] **Step 3:** Implement: `SCHEMA_V15` migrates `engine_secrets` (rebuild with `role TEXT NOT NULL CHECK(role IN ('inference','admin'))`, PK `(binding_id, role)`, existing rows → `'inference'`); append to `MIGRATIONS`; update `secrets.rs` with role-aware statements and role in AAD.
- [ ] **Step 4:** Full store tests green.
- [ ] **Step 5:** Commit `feat: engine secrets carry a role so SGLang seals inference and admin keys`.

### Task 2: Production `NativeLaunch` builder + shared descriptor builder

**Files:**
- Create: `crates/mllm-controller/src/native_launch.rs`
- Modify: `crates/mllm-controller/src/lib.rs` (module), `crates/mllm-controller/src/runtime.rs` (arm uses the extracted builder)
- Test: `crates/mllm-controller/tests/native_launch_builder.rs` (new)

**Interfaces:**
- Produces:
  - `pub fn frozen_from_work(work: &InitializeWork, inference_ref: String, admin_ref: String) -> Result<NativeLaunch, CoordinatorError>` — metadata from `work.effective()` (profile engine/`NATIVE_SGLANG_RECIPE`/`NATIVE_SGLANG_SOURCE_REVISION`/`NATIVE_CHECKPOINT_REVISION` constants, `work.binding_id()/incarnation()/endpoint()`, served name `format!("candidate-{}", binding_id)`, digest = SHA-256 hex over the serialized `SglangLaunchSettings`, device from profile's selected device + host hardware fingerprint).
  - `pub fn private_descriptor(session_id: &str, execution: &Execution..., checkpoint_root: &str, public_settings: &serde_json::Value) -> Result<Vec<u8>, RuntimeError>` — the JSON `NativeLaunchHandoff::arm` builds today (`schema_version: 2`, kind `sglang_candidate_private_launch`, `launch_scope`).
- Consumes: `InitializeWork`, `mllm_config::effective::sglang` constants, `NativeLaunch::from_frozen_store`.

- [ ] **Step 1:** Failing tests: builder produces the exact metadata a frozen descriptor carries for a golden effective config; descriptor JSON byte-matches what `arm` produces for the same inputs (extract, don't fork).
- [ ] **Step 2:** Run — expect "module not found".
- [ ] **Step 3:** Implement builder + extraction; `runtime.rs::arm` calls `private_descriptor` instead of inline JSON.
- [ ] **Step 4:** `cargo test -p mllm-controller` green (runtime_binding tests must be unchanged in behavior).
- [ ] **Step 5:** Commit `feat: production NativeLaunch builder and one shared SGLang descriptor`.

### Task 3: Descriptor-capable spawn through the ordinary tools

**Files:**
- Modify: `crates/mllm-adapters/src/traits.rs` (`OwnedProcessLaunch::spawn_durable_protected`), `crates/mllm-launchers/src/owned_launch.rs`
- Test: `crates/mllm-launchers/tests/` (extend owned-launch tests)

**Interfaces:**
- Produces: `fn spawn_durable_protected(&self, incarnation: &str, cmd: &RenderedCommand, descriptors: &ProtectedLaunchDescriptors) -> Result<ProcessIdentity, RuntimeError>`; default implementation returns `RuntimeError::Unsupported` (vLLM unaffected). `OwnedLaunch` implements it by calling `self.spawn.spawn_persisted_with_descriptors(incarnation, cmd, Some(descriptors), self.association.as_ref())` (new `DurableSpawn` method wrapping `durable.rs:188`'s existing optional-descriptors path).
- Consumes: `ProtectedLaunchDescriptors::numbers()` (`durable.rs:101`), existing `spawn_persisted` internals.

- [ ] **Step 1:** Failing test: a protected spawn inherits the three fds and the child can read them (fixture reads `/proc/self/fd/<n>`); vLLM-path tests unchanged.
- [ ] **Step 2:** Run — expect missing method.
- [ ] **Step 3:** Implement trait method + `spawn_persisted_with_descriptors`.
- [ ] **Step 4:** Launcher + adapter tests green.
- [ ] **Step 5:** Commit `feat: owned process tools spawn with protected descriptors`.

### Task 4: SGLang adapter launch capability

**Files:**
- Modify: `crates/mllm-adapters/src/sglang/adapter.rs` (observer → `Option<Arc<dyn SglangRuntimeObserver>>`; add `with_launch`/`with_tools`/`with_credentials`; `check_readiness` real), `crates/mllm-adapters/src/sglang/mod.rs`
- Create: `crates/mllm-adapters/src/sglang/initialize.rs`
- Modify: `crates/mllm-adapters/src/resolve.rs` (`AdapterSpec::Sglang.observer` → `Option<...>`; pass tools)
- Test: `crates/mllm-adapters/tests/sglang_initialize.rs` (new)

**Interfaces:**
- Produces: `SglangAdapter::launch_parts() -> Result<(SglangLaunchHandle, Arc<dyn OwnedProcessLaunch>, String, String), RuntimeError>` where `SglangLaunchHandle` bundles the frozen `NativeLaunch` + rendered public settings; `initialize(adapter, command)` mirroring `vllm/initialize.rs` (same constants: `READINESS_POLL` 500ms, `BUILDER_MARGIN_MS` 2000, `LOG_TAIL_LINES` 20).
- Readiness: `GET {endpoint}/v1/models` with `Authorization: Bearer <inference>`; served name present → Ready; unreachable → `Initializing`; other errors → `Err(AdapterError::Uncertain)`.
- Initialize: render public settings JSON, build `ProtectedLaunchDescriptors` (private descriptor from Task 2 builder, inference bytes, admin bytes), `spawn_durable_protected`, readiness loop with `tools.present`, one authenticated chat probe (`forward_chat`, model = served name), `tools.observe_group`, milestones `[AllocationsRestored, WeightsUsable, CacheValid, ModelUsable]`, receipt `format!("sglang {} ready on {}; probe answered", fingerprint, endpoint)`.
- Control actions without an observer: `execute_persisted` → `RuntimeError::Unsupported` (unchanged); `park`/`restore`/`reload_weights` → `AdapterError::UnsupportedCapability` (unchanged).
- Consumes: Tasks 2 and 3.

- [ ] **Step 1:** Failing tests in `sglang_initialize.rs`: (a) missing launch/tools/credentials → `Unsupported`; (b) full launch against a stub engine HTTP surface + stub process tools → exact identities, milestones, receipt; (c) readiness by served name (wrong key refused — `// T10`); (d) engine exits before readiness → error with redacted log tail; (e) observer-less adapter refuses control actions (`// T16` posture); (f) served name is `candidate-{binding_id}` and kinds are `sglang_candidate_launch`/`sglang_candidate_private_launch` (contract pin).
- [ ] **Step 2:** Run — expect compile failures.
- [ ] **Step 3:** Implement adapter changes + `initialize.rs` (transcribe the vLLM loop, swapping renderer/readiness; keep redaction via `redact_text`).
- [ ] **Step 4:** `cargo test -p mllm-adapters` green; existing `sglang_control.rs`/`sglang_args.rs` tests updated only for the optional-observer constructor.
- [ ] **Step 5:** Commit `feat: SGLang adapter launches the ordinary way`.

### Task 5: `ProfileBindings` SGLang branch + dual sealing

**Files:**
- Modify: `crates/mllm-controller/src/engine_bindings.rs`, `crates/mllm-controller/src/coordinator/worker.rs` (`spawn_resolved`)
- Test: `crates/mllm-controller/src/engine_bindings/tests.rs`, `crates/mllm-controller/src/coordinator/tests.rs`

**Interfaces:**
- Produces: `EngineBindings::spec` for `Engine::Sglang` returns `AdapterSpec::Sglang { frozen: Box::new(native_launch::frozen_from_work(work, inference_ref, admin_ref)), inference, admin, observer: None }` where `inference`/`admin` are hex of two fresh `new_engine_key()`s and the refs are `format!("sglang-inference-{binding}")`/`format!("sglang-admin-{binding}")`. `spawn_resolved` matches `AdapterSpec::Sglang { inference, admin, .. }` and seals **both** roles (Task 1 API) before the builder runs; the release path already deletes by binding (`delete_engine_keys`).
- `engine_bindings/tests.rs::sglang_is_refused_by_name_rather_than_stubbed` is **deleted** — replaced by a test asserting the spec builds and names both credential refs.
- Consumes: Tasks 1, 2, 4.

- [ ] **Step 1:** Failing coordinator test: an SGLang deployment's start arms, seals both roles, spawns through the stub tools, reaches Ready (`// T16`); and `delete_engine_keys` removed both after release.
- [ ] **Step 2:** Run — expect refusal text gone/compile error.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Core suite green: `cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`; clippy `-D warnings` clean.
- [ ] **Step 5:** Commit `feat: ProfileBindings builds the SGLang runtime and seals both launch keys`.

### Task 6: Native composition module (Python)

**Files:**
- Create: `runtime/sglang_native_composition.py`
- Test: `runtime/tests/test_sglang_native_composition.py` (new)

**Interfaces:**
- Produces: `compose(spec, checkpoint) -> NativeContract` that runs, in order: pinned-source revalidation (`sglang_source_preflight.revalidate_sglang_sources`), plugin closure (`sglang_startup_guards.enforce_closed_plugins`), placement attestation (`sglang_device.observe_placement` with the service-authorized digest from the private descriptor), checkpoint revalidation (`checkpoint_preflight.revalidate_checkpoint`). Returns an immutable named tuple carrying the verified facts. Any failure raises a closed-category error; no native import has happened at this point.
- `enroll_and_observe(spec, contract, server_factory) -> ObservationHandle`: attaches the scheduler observation path (`sglang_scheduler_observer` + `sglang_observation_transport` + `sglang_observation_server.SchedulerObservationServer`) for the enrolled worker, returning the socket path the Rust `NativeObservationClient` reads.
- Consumes: existing runtime modules unchanged; the private descriptor's `launch_scope` and a new optional `placement_digest` field (added in Task 8 with the rename — until then the digest comes from the descriptor's public settings digest check).

- [ ] **Step 1:** Failing pytest: composition order enforced (a failing placement check prevents import-time work); socket created under 0700 service-owned dir; closed categories on every failure path; synthetic fixtures only (no GPU, no engine import).
- [ ] **Step 2:** Run pytest — expect ImportError.
- [ ] **Step 3:** Implement composition as pure orchestration of the existing modules.
- [ ] **Step 4:** `python3 -m pytest runtime/tests/test_sglang_native_composition.py` green; full `runtime/tests` green.
- [ ] **Step 5:** Commit `feat: SGLang native startup composition composes the audited gates`.

### Task 7: Worker enrollment across spawned interpreters

**Files:**
- Modify: `runtime/sglang_entry.py` (child-main preparation path only), `runtime/sglang_startup_guards.py` (export the preimport sequence as one callable)
- Test: `runtime/tests/test_sglang_entry.py` (extend)

**Interfaces:**
- Produces: `sglang_startup_guards.preimport_guard()` — one callable combining `contain_startup_output()` + `enforce_closed_plugins()` + trusted-path assertion, safe to call at the top of any spawned interpreter's main. The entry's `__mp_main__` preparation calls it before `Process` arguments unpickle. **Does not touch `_verified_native_contract`/`_import_and_launch` (still denied).**
- [ ] Steps: failing test that a simulated spawned interpreter runs the guard before unpickling; implement; pytest green; commit `feat: SGLang spawned interpreters run the preimport guard`.

### Task 8: Ordinary naming rename (Rust + Python together)

**Files:**
- Modify: `crates/mllm-controller/src/native_launch.rs` (descriptor kinds), `crates/mllm-adapters/src/sglang/args.rs` (served-name validation), `crates/mllm-adapters/tests/sglang_args.rs`, `crates/mllm-controller/tests/runtime_binding.rs`
- Modify: `runtime/sglang_entry.py` validators (`:155`, `:169`, `:275`), `runtime/sglang_server_args.py` if it pins the served name
- Test: both sides assert the new literals

**Interfaces:**
- Produces: private kind `sglang_private_launch`, public kind `sglang_launch`, served name = the deployment's route name (validated as a non-empty printable token, no `candidate-` requirement). `NativeLaunchMetadata.served_name` callers updated.

- [ ] **Step 1:** Failing tests both sides for new literals; old literals rejected.
- [ ] **Step 2:** Implement rename atomically (single commit touches both).
- [ ] **Step 3:** Rust core suite + pytest runtime tests green.
- [ ] **Step 4:** Commit `feat: SGLang descriptor contract takes its ordinary names`.

### Task 9: Open the gate

**Files:**
- Modify: `runtime/sglang_entry.py` (`_verified_native_contract` → calls Task 6's `compose`; `_import_and_launch` → guarded import of `sglang.launch_server` under the verified contract)
- Test: `runtime/tests/test_sglang_entry.py` — the denial tests invert to contract tests; a test asserts composition failure still refuses startup with closed categories.

- [ ] **Step 1:** Update tests: contract present → startup proceeds to the guarded import boundary; any gate failure → `pinned_source_contract_unavailable` behavior replaced by the gate's own closed category.
- [ ] **Step 2:** Implement; keep `main`'s closed-category error rendering exactly.
- [ ] **Step 3:** pytest green; `runtime/tests` full green.
- [ ] **Step 4:** Commit `feat: SGLang native entrypoint composes the audited startup contract`.
- [ ] **Step 5:** **This task runs only after Tasks 6–8 are reviewed.** The denial is replaced here, not before.

### Task 10: Live gate on host-a

**Files:**
- Create: `crates/mllm-cli/tests/live_sglang.rs` — scenarios: SGL1 launch (Ready), SGL2 one routed inference, SGL3 stop with group proven gone + memory returns. Pattern: `crates/mllm-cli/tests/live_vllm.rs`, env `MLLM_SGLANG_BIN` (venv `~/mllm-sglang-f2-venv/bin/sglang`), `MLLM_MODELS_ROOT=~/models`, engine `qwen3-4b-instruct`, SGLang 0.5.19.
- Modify: `scripts/live/run-on-spark.sh` — build `live_sglang` too and run it after `live_vllm` (same pre-flight, one thread).
- Modify: `docs/runbooks/spark-live-f2.md` — append the SGLang evidence entry from the template.

- [ ] **Step 1:** Implement the test file mirroring `live_vllm.rs` shapes (guarded by `MLLM_SGLANG_BIN` so CPU runs skip).
- [ ] **Step 2:** `cargo test -p mllm-cli --test live_sglang` compiles and skips off-host.
- [ ] **Step 3:** Commit `test: SGLang live gate scenarios`.
- [ ] **Step 4:** Owner-visible live run via `scripts/live/run-on-spark.sh`; record evidence. **CPU results are not the claim.**

## Self-review notes

- Spec coverage: §4.1 → Tasks 2,4; §4.2 → Tasks 1,5; §4.3 → Task 4; §4.4 → Task 4; §4.5 → Task 8; §5 → Tasks 6–9; §7 → Tasks 4,10.
- The plan's SGLang control actions stay refused; park is S2, not here.
- Route order and the 2-process identity model are untouched (program §5).
