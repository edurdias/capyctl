# F1 vLLM Path Implementation Plan

**Goal:** Deliver the first working vLLM path — a real engine adapter behind the F0 contracts, a streaming router with admission/fairness, durable operations on real processes with generation machinery, attachment, restart-only switching, and the policy-gated park/reload path — validated live on the owner's DGX Spark.

**Architecture:** The F0 fake engine stays the conformance base; the vLLM adapter implements the same `EngineAdapter` trait over vLLM's OpenAI-compatible HTTP API, with parked-state observability added to the contract (`Phase::Parked`, `EngineState.build_fingerprint`). The router (`capyctl-router`) owns `/v1/models` + `/v1/chat/completions` with bounded admission and the switching engine. A real `exec` launcher (process groups, signals) replaces fake spawning for managed operations. All engine-specific knowledge stays inside `capyctl-adapters::vllm`.

**Tech Stack:** Existing 11-crate Rust workspace (tokio, tonic, rusqlite, clap). New: `reqwest` (adapter HTTP client), `axum` (router surface, hyper 1.x-compatible with tonic 0.13), `tokio-tungstenite`-free SSE via `axum` response bodies, `nix` or `libc` for process groups.

**Spec:** `docs/design/milestones/f1-vllm-path-design.md` (approved + reviewed) — the plan implements it; upstream authority is `docs/SPEC.md` rev 0.2. Executors read both. F0 landed contracts are in `crates/` on `master` (103 tests green).

## Global Constraints

- Every claim is tiered: simulator evidence vs live-Spark evidence never conflated (AGENT_HANDOFF).
- Engine-specific endpoints/launch parameters live only inside `capyctl-adapters::vllm`; the router never imports engine crates.
- `check_readiness`: `/v1/models` returning the served model is readiness; liveness (`/health`) is not (SPEC §6.1). `/v1/models` alone never establishes Ready after a park.
- Park/reload is core F1 functionality via engine capabilities (sleep/awake where available, restart otherwise); if the pinned build's sleep path misbehaves on the Spark, the recipe is revised until it works — no downgrade, no deferral.
- The `vllm-sleep` profile requires `security.allow_development_engine_controls: true` — the opt-in gates the profile itself (launching in any mode), not just park/reload. Default denial enforced at fake/adapter/host-policy/controller layers (T21). The upstream vLLM dev-mode warning is carried into docs, never erased.
- Exit codes (frozen): 0 ok, 2 invalid config, 3 unauthorized, 4 insufficient resources, 5 unsupported, 6 unreconciled, 7 device conflict, 8 category limit, 10 activation timeout, 11 topology unknown, 12 no safe estimate, 13 internal (new in F1 for store/I-O/runtime-boot failures).
- Idempotency key = `SHA-256(server context id, deployment name, canonical manifest bytes)`; key/content mismatch → conflict.
- Permissions: state dirs 0700, files 0600. Store/journal never hold inference bodies; logs never hold prompts/secrets. Launch fingerprints record launch args **with secrets redacted** (SPEC §8.2/§13.3).
- Drain liveness: bounded grace after `Uncertain` cancel → best-effort abort → residual uncertainty recorded in release evidence; reservations retained until verified release (uncertainty never becomes free capacity).
- Switch failure: A reopens with window state; B receives structured switch-failed error with bounded re-queue honoring remaining deadlines; failed-switch event recorded.
- preinitialize: start → readiness validation → park, never displacing live work; fails clearly (unsupported-capability class) when parking is unqualified or policy-denied; simulator-tier until G6 qualification.
- The inference listener binds loopback/private per host profile with API-key auth; non-loopback binds require TLS; no public bind is a supported F1 configuration (SPEC §15.2).
- capyctl never installs engines or downloads checkpoints (T07); the operator executes `docs/runbooks/vllm-env.md`.
- Every task ends with `cargo test --workspace` green, `cargo clippy --workspace --all-targets -- -D warnings` clean, and a git commit on `master`.

## File Structure

```text
crates/
  capyctl-adapters/src/{traits.rs (extend), vllm/{mod.rs, http.rs, args.rs, sleep.rs}}
  capyctl-launchers/src/{lib.rs, process.rs}        # real OS process launcher
  capyctl-router/src/{lib.rs, admission.rs, chat.rs, stream.rs, switch.rs}
  capyctl-controller/src/{operations.rs (extend), generations.rs, attach.rs}
  capyctl-agent/src/{lib.rs (extend), supervision.rs}
  capyctl-domain/src/lifecycle.rs (extend: LifecycleAction here per F0)
  capyctl-store/src/{deployments.rs (extend: reservations/generations), migrations.rs (v3)}
  capyctl-cli/src/{main.rs, roles.rs, output.rs (extend)}
docs/runbooks/vllm-env.md                  # operator-executed environment contract
tests/harness/src/lib.rs (extend)                # conformance suite grows
tests/mapping/README.md (extend)                 # T07..T21 mapping
```

---

### Task 1: Adapter contract extension — `Phase::Parked` + fingerprint channel

**Files:**
- Modify: `crates/capyctl-adapters/src/traits.rs` (add `Phase::Parked`, `EngineState.build_fingerprint`)
- Modify: `crates/capyctl-adapters/src/fake/engine.rs` (implement Parked reporting)
- Modify: `tests/harness/src/lib.rs` (readiness check must not accept Parked-as-Ready)
- Test: `crates/capyctl-adapters/tests/parked_state.rs`

**Interfaces:**
- Consumes: F0 `EngineAdapter`, `FakeEngine`, conformance suite.
- Produces: `Phase::{Startup, Ready, Parking, Parked, Restore}`; `EngineState { phase, retained_bytes, build_fingerprint: Option<String> }`. All later tasks consume these exact shapes.

- [ ] **Step 1: Write the failing test**

```rust
// crates/capyctl-adapters/tests/parked_state.rs
use capyctl_adapters::{EngineAdapter, FakeEngine, MemberRef, ParkLevel, ParkPolicy};

fn member() -> MemberRef { MemberRef { deployment_id: "d".into(), member_id: "m".into() } }

#[tokio::test]
async fn parked_engine_reports_parked_phase_not_ready() {
    let e = FakeEngine::new().with_policy(ParkPolicy::ExperimentalAllowed);
    e.park(&member(), ParkLevel::Two).await.unwrap();
    let st = e.inspect(&member()).await.unwrap();
    assert!(matches!(st.phase, capyctl_adapters::Phase::Parked));
    assert!(st.build_fingerprint.is_some(), "fingerprint channel exists");
    // readiness must not claim Ready for a parked engine:
    let rd = e.check_readiness(&member()).await.unwrap();
    assert!(!matches!(rd, capyctl_adapters::Readiness::Ready));
}

#[tokio::test]
async fn restore_returns_to_ready_with_fingerprint() {
    let e = FakeEngine::new().with_policy(ParkPolicy::ExperimentalAllowed);
    e.park(&member(), ParkLevel::Two).await.unwrap();
    e.restore(&member()).await.unwrap();
    let st = e.inspect(&member()).await.unwrap();
    assert!(matches!(st.phase, capyctl_adapters::Phase::Ready));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p capyctl-adapters --test parked_state`
Expected: FAIL — `Phase::Parked` undefined; `build_fingerprint` missing.

- [ ] **Step 3: Implement**

`traits.rs`: add `Parked` to `Phase`; add `pub build_fingerprint: Option<String>` to `EngineState` (update all constructors). `fake/engine.rs`: on level-2 park success set phase `Parked`; `check_readiness` returns `Readiness::Initializing` (not Ready) while Parked; fingerprint returns the fake's constant build id. Update the F0 conformance `fabricated_ready_adapter_fails_readiness_gating` if it constructs `EngineState` (additive field).

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace` → PASS (all existing tests stay green).

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-adapters tests/harness && git commit -m "feat(adapters): Parked phase variant and build fingerprint channel"
```

---

### Task 2: HTTP engine client (`capyctl-adapters::vllm::http`)

**Files:**
- Create: `crates/capyctl-adapters/src/vllm/{mod.rs, http.rs}`
- Modify: `crates/capyctl-adapters/Cargo.toml` (+ `reqwest = { version = "0.12", features = ["json"] }`, `futures = "0.3"` in dev)
- Test: `crates/capyctl-adapters/tests/vllm_http.rs`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:

```rust
pub struct EngineHttp { base: reqwest::Url, api_key: Option<String>, client: reqwest::Client }
impl EngineHttp {
    pub fn new(base: reqwest::Url, api_key: Option<String>) -> Self;
    pub async fn health(&self) -> Result<bool, HttpError>;                       // /health
    pub async fn list_models(&self) -> Result<Vec<String>, HttpError>;           // /v1/models ids
    pub async fn chat_completion_stream(&self, req: &serde_json::Value, on_chunk: impl FnMut(StreamEvent))
        -> Result<StreamEnd, HttpError>;                                          // SSE consume
    pub async fn sleep(&self, level: u8) -> Result<SleepOutcome, HttpError>;      // POST /sleep?level=N
    pub async fn wake(&self) -> Result<WakeOutcome, HttpError>;                   // POST /wake_up
}
pub enum HttpError { Unreachable, UnexpectedStatus(u16), AuthRejected, Body(String) }
pub enum SleepOutcome { Applied, Uncertain }        // ack lost ≠ Applied
pub enum WakeOutcome { Applied, Uncertain }
pub enum StreamEnd { Completed, ClientClosed, BackendClosed }
```

- [ ] **Step 1: Failing tests** — spin a local `axum` test server (add `axum = "0.8"` + `tokio` to dev-deps of capyctl-adapters): `/health` 200; `/v1/models` returns JSON with ids; `/sleep?level=2` 200 with body; a variant dropping the connection before responding → `Uncertain`; an SSE endpoint emitting `data: {...}\n\n` lines asserting chunks arrive in order and `StreamEnd::Completed` on `data: [DONE]`.
- [ ] **Step 2: Run** `cargo test -p capyctl-adapters --test vllm_http` → FAIL.
- [ ] **Step 3: Implement** — reqwest client; SSE consumption loop parsing `data:` lines until `[DONE]`; timeout on sleep/wake (default 30s from a const `ENGINE_CONTROL_TIMEOUT_SECS`) → `Uncertain` on timeout-after-effect ambiguity is NOT assumed — a timeout maps to `HttpError::Unreachable`-class `Uncertain` only when the connection failed mid-response; document the distinction in a comment.
- [ ] **Step 4: Run** → PASS. Commit: `feat(adapters): vLLM HTTP engine client with SSE streaming and sleep/wake outcomes`.

---

### Task 3: vLLM adapter (`capyctl-adapters::vllm`)

**Files:**
- Create: `crates/capyctl-adapters/src/vllm/{adapter.rs, args.rs, sleep.rs}`
- Test: `crates/capyctl-adapters/tests/vllm_adapter.rs`

**Interfaces:**
- Consumes: Task 1 (`Phase::Parked`, `EngineState.build_fingerprint`), Task 2 (`EngineHttp`).
- Produces:

```rust
pub struct VllmAdapter { http: EngineHttp, fingerprint: String, policy: ParkPolicy, base_args: Vec<String> }
impl VllmAdapter { pub fn new(base: reqwest::Url, api_key: Option<String>, fingerprint: String, policy: ParkPolicy) -> Self }
#[async_trait] impl EngineAdapter for VllmAdapter { /* all 9 methods */ }
```

Semantics: `check_readiness` = `list_models` contains the deployment's model id (never `/health` alone); `inspect` = phase from `/v1/models` presence + last-known park state (the adapter tracks it; the controller corroborates via provenance per the design) + fingerprint; `prepare_park` = `observe_work` reports Idle; `park(level)` = policy check FIRST (`PolicyDenied` when `ParkPolicy::Denied`) then `http.sleep(level)`; `restore` = `wake()` then `reload_weights()`; `reload_weights` = policy check then `wake`-collective path (documented mapping to vLLM's RPC; simulator-tier only until Task 16); `cancel_work(require_ack=false)` → `Uncertain` always (vLLM has no ack API); `render_plan` → Task 4.

- [ ] **Step 1: Failing tests** — against a mock axum server implementing `/v1/models`, `/health`, `/sleep`, `/wake_up`: readiness requires the model id (server listing a different id → not Ready); park with `ParkPolicy::Denied` → `PolicyDenied` and NO HTTP call asserted (record server hit count); park with allowed policy + server 200 → `ParkOutcome::Parked{retained_bytes}` where the adapter maps level 2 to a documented residue constant; restore = wake+reload called once each (hit counts); cancel without ack → `Uncertain` without HTTP call.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Conformance gate** — run `harness::run_conformance(&adapter, &fake_launcher)` in the test; all checks pass. Commit: `feat(adapters): vLLM adapter behind the F0 EngineAdapter contract`.

---

### Task 4: Argument rendering + reserved-flag conflicts (T14, T12 groundwork)

**Files:**
- Create: `crates/capyctl-adapters/src/vllm/args.rs`
- Test: `crates/capyctl-adapters/tests/vllm_args.rs`

**Interfaces:**
- Produces:

```rust
pub struct PlanInputVllm { pub model_path: String, pub port: u16,
    pub granted: GrantedBudget, pub engine_args: Vec<String>,   // user pass-through
    pub sleep_flags: Vec<String> }                              // only when profile gated-in
pub struct GrantedBudget { pub kv_cache_bytes: Option<i64>, pub gpu_utilization_pct: Option<u8>,
    pub swap_space_bytes: Option<i64> }
pub const RESERVED_FLAGS: &[&str] = &["--port", "--device", "--gpu-memory-utilization",
    "--swap-space", "--kv-cache-bytes", "--enable-sleep-mode", "--api-key"];
pub fn render_command(input: &PlanInputVllm) -> Result<RenderedCommand, ArgsError>;
pub enum ArgsError { ReservedConflict(String), InvalidBudget(String) }
```

Rules: granted budgets map to vLLM flags with explicit units (`--gpu-memory-utilization 0.75` style derived from pct; `--kv-cache-bytes` from bytes); user `engine_args` are appended AFTER capyctl-controlled flags; any user arg matching `RESERVED_FLAGS` → `ReservedConflict` (T14); `sleep_flags` (e.g. `--enable-sleep-mode`) render only when the profile is policy-gated-in (empty otherwise); the rendered command carries the launch fingerprint and **redacts any `--api-key <value>` occurrence** (fingerprint stores `--api-key <redacted>`).

- [ ] **Step 1: Failing tests**: budget → exact flag strings; user arg `--port 9999` → `ReservedConflict("--port")`; `--enable-sleep-mode` from user args → ReservedConflict; sleep_flags empty when profile not gated; fingerprint redaction of `--api-key secret123`. **Step 2: Run FAIL. Step 3: Implement. Step 4: PASS.**
- [ ] **Step 5: Commit** `feat(adapters): vLLM argument rendering with reserved-flag conflicts and secret redaction`.

---

### Task 5: Real process launcher (`capyctl-launchers`)

**Files:**
- Create: `crates/capyctl-launchers/src/{lib.rs, exec.rs}`
- Modify: `crates/capyctl-agent/src/lib.rs` (embed the real launcher for managed ops)
- Test: `crates/capyctl-launchers/tests/exec.rs`

**Interfaces:**
- Consumes: `capyctl_adapters::{Launcher, OwnedHandle, ExitReport, HandleStatus, RenderedCommand}` (trait impl target).
- Produces: `ExecLauncher` implementing `Launcher` with **real OS processes**: spawn via `std::process::Command` + `process_group(0)` (libc/nix — new dep `nix = { version = "0.29", features = ["process", "signal"] }`), `OwnedHandle { pid, start_identity: boot-unique counter }`; `terminate` sends SIGTERM to the process **group**, waits `grace`, then SIGKILL; `ExitReport { pid, exit_code, signal, killed }` from wait; `verify_handle` re-stat `/proc/<pid>` + compare stored `/proc/<pid>/stat` starttime field (Linux start identity) → `HandleStatus::{Valid, StaleReused, Gone}`.

- [ ] **Step 1: Failing tests** (real processes, fast): spawn `sleep 30` → handle Valid, `/proc` exists; terminate with 1s grace → process group gone (assert no children via `/proc/<pid>/task` scan), ExitReport signal-reported; kill a process, spawn another until PID reuse is impractical — instead test start-identity: spawn, terminate, spawn again → new handle's `start_identity` differs; stale-handle detection: fabricate a handle whose starttime mismatches → `StaleReused` (verify by comparing against a real /proc read of the current pid). Signals: spawn a `sh -c 'trap "" TERM; sleep 30'` → grace expiry leads to SIGKILL and `killed=true`.
- [ ] **Step 2: Run FAIL. Step 3: Implement (procfs parsing via std::fs; no new heavy deps beyond nix). Step 4: PASS.**
- [ ] **Step 5: Commit** `feat(launchers): real exec launcher with process groups, signal escalation, start-identity verification`.

---

### Task 6: doctor host — real fingerprinting + environment contract + exit code 13

**Files:**
- Create: `docs/runbooks/vllm-env.md`
- Modify: `crates/capyctl-agent/src/{lib.rs, supervision.rs}` (doctor implementation), `crates/capyctl-cli/src/{main.rs, output.rs}` (exit code 13 internal), `crates/capyctl-cli/src/grammar.rs` (doctor output shape)
- Test: `crates/capyctl-agent/tests/doctor.rs`

**Interfaces:**
- Produces: `doctor_host(state_dir) -> DoctorReport` where `DoctorReport { profiles: Vec<ProfileReport>, domains: Vec<DomainObservation>, warnings: Vec<String> }`, `ProfileReport { name, command_exists: bool, build_fingerprint: Option<String>, notes }`; fingerprint capture = run `<command> --version` (e.g. `vllm --version`) via the real launcher, hash output + path (sha2, already a workspace dep). Domain observations: read `/proc/meminfo` MemTotal → `Domain { kind: System, observed_bytes, observed_at_unix: now }` (Linux; document platform caveat).
- Runbook content (operator-executed, exact commands): create venv `/opt/vllm`, `pip install vllm==<PINNED_VERSION>` — **the pin is set from `capyctl doctor`'s first live capture** (Task 16 step 1), placeholder `<PINNED_VERSION>` resolved at execution, not a fictional number; checkpoint download target `/srv/models/<approved-model>`; the vLLM development-mode security warning quoted verbatim with the note that it is required for the sleep path.

- [ ] **Step 1: Failing tests**: doctor on a temp state_dir with a profile whose command is `/bin/false --version` → `command_exists: true, fingerprint: Some(...)` (exit status captured); profile command missing → `command_exists: false`; doctor never launches an engine server (assert no long-running process); exit-code test: store failure during `start` → process exit 13 not 2. **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Write the runbook** (exact content with `<PINNED_VERSION>` marked as captured-at-qualification). Commit: `feat(agent): doctor host fingerprinting, environment contract runbook, internal exit code 13`.

---

### Task 7: Router core — models list + non-streaming chat (`capyctl-router`)

**Files:**
- Create: `crates/capyctl-router/src/{lib.rs, admission.rs, chat.rs}`
- Modify: `crates/capyctl-router/Cargo.toml` (+ `axum = "0.8"`, `reqwest`, workspace deps), `crates/capyctl-cli/src/roles.rs` (start standalone serves the router)
- Test: `crates/capyctl-router/tests/router_core.rs`

**Interfaces:**
- Consumes: store (`get_deployment`), scheduler (`admit`), controller (`submit_deploy`, `request_transition`), adapters (`EngineAdapter` for dispatch).
- Produces:

```rust
pub struct Router { /* deps injected */ }
pub struct RouterDeps { pub store: Arc<Mutex<capyctl_store::Store>>,
    pub controller: Arc<capyctl_controller::Controller>,
    pub adapters: HashMap<String, Arc<dyn EngineAdapter>>,   // profile name -> adapter
    pub limits: QueueLimits }
pub struct QueueLimits { pub max_requests_per_deployment: usize, pub max_buffered_bytes_total: usize }
pub fn serve_router(deps: RouterDeps, bind: SocketAddr) -> axum::Router;   // builds the axum app
// Routes: GET /v1/models; POST /v1/chat/completions  (auth: api_key header `Authorization: Bearer`)
```

Semantics: `/v1/models` lists enabled deployments' route ids — never wakes (call must not trigger `request_transition`); `/v1/chat/completions`: authenticate → resolve alias → admission (ledger `admit` with a request byte estimate; queue bounds enforced → structured `queue_full` error) → if deployment READY dispatch to adapter path (F1: via the assigned member's ingress; the in-process path dispatches through the EngineAdapter's chat stream), else enqueue + join the single activation operation (T15 groundwork; full switching in Task 11).

- [ ] **Step 1: Failing tests** (axum test client against `serve_router` with fake deps): models lists only enabled; disabled id → 404 structured error; unauthenticated → 401; chat to READY fake deployment returns completion; queue bound exceeded → structured `queue_full`; models call does not change observed_state (assert store). **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Commit** `feat(router): /v1/models and non-streaming chat completions with bounded admission`.

---

### Task 8: Streaming chat + cancellation accounting (T17 groundwork)

**Files:**
- Create: `crates/capyctl-router/src/stream.rs`
- Test: `crates/capyctl-router/tests/router_stream.rs`

**Interfaces:**
- Consumes: Task 7 router core, Task 2 `chat_completion_stream`/`StreamEnd`.
- Produces: SSE response piping adapter chunks to the client; **in-flight accounting**: a `StreamGuard` registered in the router's per-deployment in-flight map; client disconnect (axum body drop) → `StreamGuard` marks the request `abandoned` but does NOT decrement in-flight until the adapter reports `StreamEnd::Completed`/`BackendClosed` or the guard is cancelled — abandoned-until-confirmed is the conservative rule.

- [ ] **Step 1: Failing tests**: SSE chunks pass through in order; client disconnect mid-stream → guard stays registered (in-flight count unchanged) until the backend stream ends; backend close mid-stream → accounting releases; cancel request → accounting release requires completion observation. **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Commit** `feat(router): SSE streaming with conservative in-flight accounting`.

---

### Task 9: Generation machinery + reservation persistence (T18)

**Files:**
- Modify: `crates/capyctl-store/src/{deployments.rs, migrations.rs}` (migration v3: `generation_history` write path, `reservations` insert path), `crates/capyctl-controller/src/{operations.rs, generations.rs}`
- Test: `crates/capyctl-controller/tests/generations.rs`

**Interfaces:**
- Produces: `GenerationService` — on every successful transition: `current_generation += 1`, insert `generation_history(deployment_id, generation, started_at, outcome)`, and pass `generation` into dispatch (the in-process dispatch path carries it; the proto `Envelope.generation` is populated in Task 13's wiring); reservation rows inserted at acceptance (`accept_deployment` gains an optional `initial_reservation: Vec<ReservationRow>` — owner/deployment-linked, charged through the same owner ids the scheduler uses).
- T18: dispatch path rejects requests carrying a generation older than current (`StaleGenerationError` from capyctl-domain).

- [ ] **Step 1: Failing tests**: two transitions → two history rows, current_generation increments monotonically; acceptance inserts reservation rows readable back; dispatch with stale generation → rejected (structured stale error); after a controller restart, current_generation continues (never resets — read from store). **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Commit** `feat(controller): generation machinery and reservation persistence (F0 deferrals closed)`.

---

### Task 10: Durable operations on the real launcher + admin vs idle stop (T10, T12)

**Files:**
- Modify: `crates/capyctl-controller/src/operations.rs` (managed ops run `ExecLauncher`-launched real processes; fake retained for tests via trait injection), `crates/capyctl-agent/src/supervision.rs`
- Test: `crates/capyctl-controller/tests/managed_ops.rs`

**Interfaces:**
- Produces: controller operations that, given a profile with a real launch command, spawn the engine process (Task 5 launcher), wait readiness via the adapter, and record exit evidence. **Admin stop**: `stop deployment` sets `suspended=true` — subsequent inference requests do NOT auto-reactivate (explicit stop semantics; T10). **Idle stop** (policy timer): stops but leaves `admission_enabled=true` — on-demand eligible. Kill semantics: terminate covers the process group; a crashed child is reconciled (agent observes exit → RECONCILING → Failed per F0).

- [ ] **Step 1: Failing tests**: deploy with `command: /bin/sleep`-style profile (test binary that stays alive, prints readiness line) → controller starts it, handle verifies, terminate stops the group; admin stop blocks auto-activation (second start request via router path → error, not wake); idle-stop path leaves on-demand eligible (router request re-activates); crash mid-READY → observed Failed + journal evidence. **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Commit** `feat(controller): managed operations on the real launcher with admin/idle stop semantics`.

---

### Task 11: Switching engine — A→B with drain policy and failure branch (T16 sim, T15, T19)

**Files:**
- Create: `crates/capyctl-router/src/switch.rs`
- Modify: `crates/capyctl-router/src/admission.rs` (bounded non-resetting window), `tests/harness/src/lib.rs` (switching conformance check)
- Test: `crates/capyctl-router/tests/switching.rs`

**Interfaces:**
- Produces:

```rust
pub struct SwitchEngine { /* controller + router deps */ }
pub const DRAIN_GRACE_DEFAULT_SECS: u64 = 10;   // versioned default; config-overridable later
pub enum SwitchOutcome { Completed { generation: u64 }, Failed { cause: SwitchFailure } }
pub enum SwitchFailure { DrainTimeout, AdmissionBlocked(BlockReason) }
```

Semantics per design §5: single wake join (concurrent requests to B join one activation — T15); bounded non-resetting window for A (busy A cannot extend; timer opens when B first waits); close A admission → drain (cancel abandoned streams; after `Uncertain` cancel: `DRAIN_GRACE_DEFAULT_SECS` grace → best-effort abort → residual uncertainty recorded in release evidence, reservations retained until verified release) → quiescence → park-or-stop A → reserve B → launch/restore → verify → open gate → dispatch queue. **Failure branch**: A's admission reopens with window state preserved; B's queued requests get structured `switch_failed` error with bounded re-queue honoring remaining deadlines; failed-switch event journaled.

- [ ] **Step 1: Failing tests** (fake engines, real router): A→B→A completes with correct generations and release evidence; simultaneous B requests join ONE activation (one `request_transition(Start)` recorded — T15); busy-A window does not reset under continuous A load (T19); drain with abandoned stream → grace → best-effort abort → switch completes with uncertainty recorded; drain timeout → switch fails, A reopens, B requests get `switch_failed`, re-queue honors deadline; queue byte bounds enforced during switch (T19). **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Conformance** — extend `tests/harness` with a switch check; run against fake. Commit: `feat(router): switching engine with drain liveness policy and failure branch`.

---

### Task 12: Attachment (T11)

**Files:**
- Create: `crates/capyctl-controller/src/attach.rs`
- Modify: `crates/capyctl-router/src/chat.rs` (attached deployments routable), `crates/capyctl-cli/src/main.rs` (attach command wiring)
- Test: `crates/capyctl-controller/tests/attach.rs`

**Interfaces:**
- Produces: `Controller::attach(req) -> Accepted` — creates a deployment row with `kind: attached`, registers route + observed endpoint; lifecycle actions (`start/park/stop/preinitialize/undeploy` via `request_transition`) on attached deployments → `Err(UnsupportedCombination)`-class structured error; router dispatches to the attached endpoint's URL; status marks `restart_guarantees: unavailable` when no supervisor integration is configured.

- [ ] **Step 1: Failing tests**: attach a fake URL → routing works (mock upstream answers chat); `park` on attached → structured rejection; status shows restart-guarantees unavailable; attached usage represented conservatively (admission treats attached byte estimates as non-reclaimable — assert `admit` blocks a candidate that would overlap the attached reservation). **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Commit** `feat(controller): attachment with ownership-separated lifecycle rejection`.

---

### Task 13: Policy gate wiring + vllm-sleep profile + compatibility doc (T21)

**Files:**
- Modify: `crates/capyctl-config/src/schema.rs` (`security.allow_development_engine_controls` parsed — false default), `crates/capyctl-agent/src/lib.rs` (`Host::park_policy()` already landed in F0 — wire profile-level gating), `crates/capyctl-controller/src/operations.rs` (profile launch refuses vllm-sleep without opt-in)
- Create: `docs/runbooks/vllm-development-mode-warning.md`
- Test: `crates/capyctl-controller/tests/policy_gate.rs`

**Interfaces:**
- Produces: profile-level gate — a profile whose args render sleep/development flags (`sleep_flags` non-empty) cannot launch unless host policy opted in, regardless of the requested operation (design §7: the opt-in gates the profile). Controller returns a structured `policy_denied` error; journal records the denial evidence.
- Doc content: the vLLM security warning (development mode not for production; collective RPC surface dangerous) quoted with the source link, the mitigation layers (policy opt-in, private bind, per-deployment engine API key), and the explicit statement that production qualification requires a reviewed control path.

- [ ] **Step 1: Failing tests**: deploy with vllm-sleep profile + default policy → launch rejected at controller with structured policy error, nothing spawned; with `allow_development_engine_controls: true` → launches (fake backend); stock profile launches without opt-in. **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Write the warning doc. Commit:** `feat(controller): profile-level development-mode gate with carried upstream warning`.

---

### Task 14: Park/restore through the adapter + preinitialize contract (T20 sim)

**Files:**
- Modify: `crates/capyctl-controller/src/operations.rs` (park/restore steps call the real adapter path; Parked phase from Task 1 used in reconciliation; ambiguous park → RECONCILING), preinitialize operation per design §7
- Test: `crates/capyctl-controller/tests/park_flow.rs`

**Interfaces:**
- Produces: park flow — drain → `prepare_park` → `park(level)`; on `AdapterError::Uncertain`/`Crash`: RECONCILING → park attempt is NOT repeated blindly (one reconcile pass; second failure → FAILED + journal); reconciliation uses `Phase::Parked` + provenance (never `/v1/models` alone). preinitialize: start → readiness validate → park; on unqualified/denied parking → structured unsupported-capability error (design §7).

- [ ] **Step 1: Failing tests** (fake with ambiguous_park + policy gate): park → ambiguous → RECONCILING → Failed; park → clean → Parked; wake → Ready with generation check; preinitialize on restart-only deployment → unsupported-capability error; preinitialize on gated-in fake → start/validate/park sequence journaled. **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Commit** `feat(controller): park/restore reconciliation and preinitialize qualification contract`.

---

### Task 15: CLI role wiring — real standalone with router (T01 carried)

**Files:**
- Modify: `crates/capyctl-cli/src/{main.rs, roles.rs}` (`start standalone` boots store + embedded agent + router listener; `deploy model --file` path reads deployment YAML via capyctl-config, derives idempotency key over (context, name, canonical bytes), submits through the controller; `status deployment` reads store; lifecycle commands dispatch)
- Test: `crates/capyctl-cli/tests/roles_f1.rs`

**Interfaces:**
- Produces: full local CLI loop: `capyctl start standalone --config ...` serves the router; `capyctl deploy model --file deployment.yaml [--activate] [--wait]` returns durable ID from stdout (JSON with `--output json`); `capyctl status deployment <id>` reads without activating.

- [ ] **Step 1: Failing tests** (spawn the built binary as a child process against a temp state dir + fake-engine profile): start standalone → listener responds; deploy --wait → exits 0 with deployment ID on stdout; deploy without --wait returns immediately after durable acceptance (ID present, operation continues — verified via status); status never changes observed_state; undeploy removes route (models list shrinks). **Step 2 FAIL → Step 3 implement → Step 4 PASS.**
- [ ] **Step 5: Commit** `feat(cli): F1 standalone loop with deploy/status/lifecycle over the real router`.

---

### Task 16: Spark live qualification — restart-only first (T07, T10, T11, T12, T14, T15, T16 live)

**Files:**
- Create: `docs/runbooks/qualification-f1.md` (the executed evidence record, filled during execution)
- Modify: `docs/runbooks/vllm-env.md` (pin `<PINNED_VERSION>` + approved checkpoint from live doctor capture)

**Interfaces:**
- Consumes: everything; `CAPYCTL_SPARK_SSH` env var (git-ignored; e.g. `export CAPYCTL_SPARK_SSH="user@spark-host"`) — never commit connection details.

**This task executes live. Every step runs `ssh "$CAPYCTL_SPARK_SSH" ...` and records output into the qualification runbook doc. If any step fails on platform grounds, STOP and report — the recipe is revised (owner decision: no downgrade), not skipped.**

- [ ] **Step 1: Environment capture** — ssh: check OS (`uname -a`), free memory (`free -b`), GPU device visibility (`nvidia-smi` or platform equivalent); operator installs vLLM per runbook; run `capyctl doctor host` remotely → record real fingerprints + `MemTotal`. Freeze `<PINNED_VERSION>` + checkpoint pin into both runbooks with the owner's approval noted in the commit message.
- [ ] **Step 2: Restart-only qualification** — deploy recipe A (stock profile, pinned checkpoint) → READY (readiness = `/v1/models` serving the model, not `/health`); serve one streaming completion; stop; verify process-group termination; re-deploy; attach the running service and verify routing + lifecycle rejection (T11 live); A→B→A across two restart-only profiles (stock + stock-alt) with release evidence (T16 live); simultaneous first requests → single wake (T15 live). Record measured request-to-first-token and memory (SPEC §17; page-cache conditions noted).
- [ ] **Step 3: Failure reconciliation live** — kill the engine process mid-serving → observe RECONCILING → Failed; stale-generation dispatch rejected (T18 evidence at the live tier where reachable).
- [ ] **Step 4: Record evidence tier** — every claim in the runbook labeled `live-tier (Spark)`; simulator-only claims stay labeled. Commit: `docs: F1 live restart-only qualification evidence on DGX Spark`.

---

### Task 17: Park/reload live validation — the core feature (T20, T21 live)

**Files:**
- Modify: `docs/runbooks/qualification-f1.md` (park/reload section)

**Owner decision binding here: park/reload is core F1 functionality — if the pinned build's sleep path misbehaves on the Spark, revise the recipe (adapter flags, vLLM release, platform configuration) and retry. No downgrade, no deferral.**

- [ ] **Step 1: Opt-in host policy** — Spark host policy sets `security.allow_development_engine_controls: true` for the isolated vllm-sleep profile; deploy the sleep-enabled recipe; verify the gated launch works (step-3 stock alternation still ran without opt-in earlier).
- [ ] **Step 2: Park → wake cycles** — serve → park (level 2: weights+KV discarded, restored from checkpoint) → verify parked-state observability (engine-side signal or provenance; `/v1/models` alone must NOT read as Ready — assert the adapter reports `Phase::Parked`) → wake → generation check → serve again → measure request-to-first-token and memory before/after each phase (SPEC §17, page-cache conditions recorded). Repeat until **3 consecutive clean cycles**.
- [ ] **Step 3: Failure injection** — ambiguous park (kill mid-sleep) → RECONCILING → verified restart; no blind repeated collective (T20 live). Policy denial: with opt-in removed, park → `PolicyDenied` (T21 live).
- [ ] **Step 4: Record + commit** — all evidence tiered `live-tier (Spark)`; simulator-tier claims never cited for these. Commit: `docs: F1 park/reload live validation on DGX Spark`.

---

### Task 18: F1 coverage audit

**Files:**
- Modify: `tests/mapping/README.md` (extend the table: T07, T10, T11, T12, T14, T15, T16, T17, T18, T19, T20, T21 → exact test fns; live-tier evidence rows → runbook sections)
- Test: audit only.

- [ ] **Step 1: Grep actual test names** (never invent) and map every F1 target id; live evidence cites `docs/runbooks/qualification-f1.md` sections.
- [ ] **Step 2: `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean.**
- [ ] **Step 3: Commit** `test(mapping): F1 coverage audit — all targeted spec ids claimed`.

---

## Verification workflow (per AGENT_HANDOFF)

Simulator-tier tests run everywhere; live-tier claims exist only in the two runbooks and only for the Spark. F1 claims nothing about SGLang (F2), remote hosts (F3), multi-node (F4), or production safety of development mode — the upstream warning stands in the compatibility doc regardless of validation success.