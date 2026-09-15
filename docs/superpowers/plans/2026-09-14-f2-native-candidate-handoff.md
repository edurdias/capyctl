# F2 Native Candidate Handoff Implementation Plan

**Goal:** Admit the pinned Qwen3 SGLang recipe to the existing candidate-only lifecycle through a checked native handoff, while keeping ordinary routing, qualification promotion, and real engine effects closed until later evidence gates.

**Architecture:** Candidate arming selects a closed, validated native launch descriptor rather than treating engine strings as authority. A protected Python entrypoint receives only frozen public settings and private credential descriptors; it validates the pinned local checkpoint immediately before launch and has no management or public control surface. The adapter and coordinator retain the existing `ArmResult::New` one-send rule; this plan does not make an engine call from deterministic tests.

**Tech Stack:** Rust 2021, existing candidate/store/controller contracts, standard-library Python 3.12, the committed `runtime.checkpoint_preflight` verifier, unittest, Rust integration tests. No SGLang import, model loading, network access, GPU work, downloads, installer changes, existing engine environment changes, or live qualification in deterministic tasks.

**Spec:** [F2 design](../../design/milestones/f2-sglang-design.md), [A2d coordinator](2026-09-12-f2a2d-coordinator-integration.md) Task 7, [F2B adapter](2026-09-12-f2b-sglang-adapter.md) Tasks 1–2, [native execution brief](../../.superpowers/sdd/2026-09-12-f2a2d-coordinator-integration/task-7-native-execution-module-brief.md), and [protected IPC contract](../../.superpowers/sdd/2026-09-12-f2a2d-coordinator-integration/task-7-native-ipc-contract.md).

## Global Constraints

- Only a persisted candidate run with the exact reviewed manifest, current session/fences, policy authorization, and `ArmResult::New` can produce a launch handoff. No `bool`, profile name, or caller string is launch authority.
- The compiled-in checkpoint preflight accepts only `Qwen/Qwen3-4B-Instruct-2507` revision `cdbee75f17c01a7cc42f958dc650907174af0554`; revalidate immediately before any eventual engine load.
- SGLang v0.5.16 source pin `fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`, TP=1, DP=1, BF16, context/token cap 4096, eight requests, memory saver enabled, CPU weight backup disabled, and no graphs/speculation/LoRA/remote code/disaggregation/external cache/CPU-KV offload/native gRPC are the sole candidate recipe.
- Native candidate acceptance remains `Unknown`; it never opens ordinary routes, promotes qualification, reduces reservations, or creates cleanup authority.
- Credential values never enter argv, environment, persisted/debug DTOs, test messages, logs, or errors. Only references cross Rust persistence boundaries.
- Never edit, stage, format, test, or run `crates/mllm-cli/tests/live_interactive.rs`; never access host-b. Deterministic work does not access host-a.

---

### Task 1: Freeze a closed native candidate descriptor

**Files:** Modify `crates/mllm-config/src/effective/candidate.rs`, `crates/mllm-config/tests/candidate.rs`, `crates/mllm-store/src/candidate_creation/initialize.rs`, and its private tests.

**Interfaces:** Replace the Fake-only `StoredLaunch` branch with a private versioned enum that can encode `Fake` or `SglangPinned { checkpoint_root, checkpoint_revision, executable, binding_id, incarnation, inference_credential_ref, admin_credential_ref, rendered_settings_digest }`. It is created only from the strict effective candidate snapshot; public APIs return redacted descriptor metadata, never paths or references.

- [ ] Add a store regression that constructs the exact SGLang candidate fixture, attempts `arm_step`, and asserts its current `Unsupported` result without adding an engine call.
- [ ] Run `cargo test -p mllm-store --lib candidate_creation::initialize::tests::initialize_arm_rolls_back_after_grant_and_denies_native`; record its passing old behavior as characterization, not a product RED.
- [ ] Add a failing candidate-config test for an SGLang descriptor with a mismatched recipe, missing distinct admin reference, non-absolute checkpoint root, or any non-pinned normalized setting; each must be rejected before persistence.
- [ ] Implement a closed descriptor constructor that accepts only the exact normalized SGLang launch settings already represented by `CandidateLaunch::Sglang`, a local absolute checkpoint root, and nonempty distinct credential references. Reject all other native engines and every unrecognized descriptor field. Hash only public behavioral inputs; do not hash references or secret values.
- [ ] Change `arm_step` so it validates the selected descriptor before resource reservation, stores the frozen redacted native descriptor only after all current policy/session/fence checks pass, and returns `ArmResult::New` exactly once. Preserve Fake behavior byte-for-byte and retain the existing rollback/`AlreadyRecorded` guarantees.
- [ ] Add tests that stale policy/session/fence, descriptor mutation, or reservation failure leave no native descriptor, grant, armed step, endpoint release, or send authority; add a positive SGLang candidate arm test that asserts ordinary dispatch remains closed.
- [ ] Run `cargo test -p mllm-config --test candidate` and `cargo test -p mllm-store --lib candidate_creation::initialize::tests` to GREEN. Commit only the listed Task 1 paths with `feat(store): freeze native candidate launch descriptors`.

### Task 2: Render a secret-free SGLang handoff

**Files:** Create `crates/mllm-adapters/src/sglang/mod.rs`, `crates/mllm-adapters/src/sglang/args.rs`, `crates/mllm-adapters/tests/sglang_args.rs`; modify `crates/mllm-adapters/src/lib.rs`.

**Interfaces:** `SglangLaunch::from_frozen(&NativeCandidateLaunch) -> Result<SglangLaunch, RuntimeError>` validates a frozen Task 1 descriptor and returns a `RenderedCommand` for `runtime/sglang_entry.py` containing only public flags and protected credential-reference file descriptors. `Debug` and `Display` omit checkpoint roots and every credential reference.

- [ ] Write Rust tests first: two valid frozen bindings render distinct endpoints and served names; all reserved aliases, mutable profile args, missing memory saver, CPU backup, unsupported topology, and ordinary/no-candidate authorization are rejected.
- [ ] Run `cargo test -p mllm-adapters --test sglang_args`; capture the missing-module/signature failure.
- [ ] Implement the closed argument renderer with explicit TP/DP/tokenizer, dtype, context, request/token caps, memory-saver and disabled-feature flags. Render no shell string and accept no arbitrary engine-argument passthrough. Validate every byte/count conversion exactly and reject overflow or a value not representable by the pinned native argument form.
- [ ] Ensure public command data has no secret value or credential reference. Require a final launcher-only resolver to substitute private descriptor file descriptors; deterministic tests assert the values are absent from Debug, Display, argv, error text, and serialized command metadata.
- [ ] Run the argument suite to GREEN and `cargo clippy -p mllm-adapters --test sglang_args -- -D warnings`. Commit only Task 2 paths with `feat(adapters): render pinned SGLang candidate handoffs`.

### Task 3: Enforce protected preflight and health boundaries

**Files:** Create `runtime/sglang_entry.py`, `runtime/tests/test_sglang_entry.py`.

**Interfaces:** `build_launch(argv, descriptor_reader) -> LaunchSpec` is import-safe and uses no SGLang import. `main()` performs descriptor resolution, calls `verify_checkpoint`, resolves private credentials only in process-local state, calls `revalidate_checkpoint` immediately before the deferred guarded engine import, and exits with sanitized status on failure. `private_health_allowed(path, authorization, inference_key) -> bool` gates normalized inference-producing `/health` paths with constant-time comparison.

- [ ] Add Python tests first for import safety (no `sglang`, `torch`, subprocess, socket, or network import), duplicate/unknown descriptor fields, unsafe checkpoint root, missing credentials, preflight error sanitization, and health path authorization including encoded/slashed variants and OPTIONS.
- [ ] Run `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s runtime/tests -p test_sglang_entry.py -v`; capture the missing-module failure and nonzero discovered count.
- [ ] Implement strict descriptor decoding with duplicate-key rejection, bounded UTF-8 fields, closed recipe/version checks, and a no-op-free failure path when memory saver evidence is absent. Do not import SGLang until `main()` after both preflight checks; deterministic tests must replace the final import/launch seam with a sentinel.
- [ ] Implement private health middleware as a narrow gate only; it cannot proxy controls, accept an arbitrary path/method, or weaken the pinned server's independent inference/admin authentication.
- [ ] Add a mutation hook test proving a checkpoint inode replacement between first verification and the final launch revalidation prevents the launch seam. Assert no test starts a server or loads a model.
- [ ] Run the Python suite to GREEN and `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s runtime/tests -p 'test_checkpoint_preflight.py' -v`. Commit only Task 3 paths with `feat(runtime): gate SGLang candidate startup on pinned preflight`.

### Task 4: Carry the frozen handoff to supervised launch without promotion

**Files:** Modify `crates/mllm-controller/src/runtime.rs`, `crates/mllm-controller/tests/runtime_binding.rs`, `crates/mllm-launchers/src/durable.rs`, `crates/mllm-launchers/tests/durable_spawn.rs`, and narrowly required adapter/store integration tests.

**Interfaces:** A controller-only `NativeCandidateHandoff` consumes exactly one frozen `ArmResult::New` descriptor and passes the Task 2 rendered command plus launcher-only credential-descriptor handles to `DurableSpawn`. It returns `Uncertain` until the existing complete-identity enrollment path is satisfied; it cannot return `Ready`, create a public endpoint, or dispatch a candidate command on reconnect/`AlreadyRecorded`.

- [ ] Add focused tests first: `AlreadyRecorded` produces no handoff; a stale binding or preflight failure produces no spawn; a secret never enters the durable record; an ambiguous child association retains binding, endpoint, grant, and closed dispatch.
- [ ] Run the relevant controller/launcher tests and capture the absent handoff failure.
- [ ] Implement the handoff after existing session/fence validation, never while holding a SQLite transaction or from a router/management request. Preserve the gate-before-initialization ordering. No test executes the rendered Python process; assert only its bounded captured launch specification.
- [ ] Add a test that a candidate SGLang handoff cannot be resolved by ordinary `RuntimeBindings::binding`, cannot make `admission_enabled`/`dispatch_enabled` true, and needs later F2B controls plus trusted evidence before completion/promotion.
- [ ] Run `cargo test -p mllm-controller --test runtime_binding`, `cargo test -p mllm-launchers --test durable_spawn`, and the affected store/adapters suites to GREEN. Commit only Task 4 paths with `feat(controller): hand off frozen native candidate launches`.

## Verification and completion boundary

- Run the four task-focused test commands, `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s runtime/tests -p 'test_*.py' -v`, relevant Rust workspace tests, `cargo clippy -p mllm-store -p mllm-controller -p mllm-launchers -p mllm-adapters --all-targets -- -D warnings`, and `git diff --check`.
- Obtain one consolidated independent review after all four tasks, fix its Critical/Important findings, then rerun the impacted verification.
- This plan only unblocks a safe candidate SGLang wrapper/arm handoff. Typed control calls, complete worker enrollment, allocation observation, model probes, evidence promotion, ordinary coordinator/router cutover, and F2C live qualification remain required and explicitly open.
