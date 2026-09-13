# F2A2d Coordinator Integration Implementation Plan

**Goal:** Connect resource reservations, completion evidence, and request leases through one deployment-bound lifecycle coordinator, then replace the F1 activation and routing authorities together.

**Architecture:** One owned worker serializes lifecycle planning and engine-control sequences on the local host; inference does not take that worker's lock. SQLite transactions fence accepted operations, all affected lifecycle owners, resource changes, and step dispatch. A pure sequence planner and explicit runtime bindings keep policy separate from engine protocols.

**Tech Stack:** Existing Rust 2021 workspace, Tokio, rusqlite, async-trait, serde, thiserror, ulid, Axum, reqwest, and Unix process supervision. Reuse workspace dependencies. Use the existing nix dependency with its `fs` feature for the lifetime file lock.

**Spec:** [Approved F2 design](../../design/milestones/f2-sglang-design.md), §§3–7 and Q1–Q8. Read and implement [A1](2026-09-12-f2a1-resource-contracts-and-admission.md), [A2a](2026-09-12-f2a2a-durable-reservation-transactions.md), [A2b](2026-09-12-f2a2b-runtime-evidence.md), and [A2c](2026-09-12-f2a2c-durable-dispatch-ownership.md) first. The amendments below supersede their disconnected primitive interfaces at production cutover.

## Global Constraints

- “Ordinary switching must not require a cold process restart.”
- “Reservation decisions and competing transition claims are atomic across deployments.”
- “It does not serialize normal inference.”
- “Client disconnect is not proof of engine cancellation.”
- “Unknown engine work does not qualify as safe quiescence for parking.”
- “Drain timeout does not implicitly authorize a process kill.”
- “Preinitialization does not evict live user work solely to prepare idle models.”
- “Their consumption cannot be assumed free capacity for managed deployments.” This includes attached services.
- “Only host-a is authorized for subsequent live work.” This plan's execution tests use local fake runtimes, never a GPU. Do not access host-b.
- Preserve `crates/mllm-cli/tests/live_interactive.rs`; do not edit, stage, or run it.
- Finish A3, B, and C planning before executing this plan. Writing this document authorizes neither product changes nor live qualification.

---

## 1. Decisions and integration boundary

Lifecycle serialization is deliberately conservative for this single-host milestone.
The worker never holds a SQLite transaction across an await. Ready requests acquire
durable dispatch leases directly and do not wait for unrelated initialization.
Acceptance, administrative fencing, inspection, and request settlement also remain
available while the worker awaits an engine command.

The worker is owned by the application, not a requesting HTTP task. Callers receive
an operation ID and observe durable state; dropping a caller does not cancel an
accepted operation. Notifications are hints: subscribe before reading state and
reread the database after notification or timeout. Do not make correctness depend
on a watch receiver surviving.

Every engine effect has a persisted step. `planned` means no permission to send;
`armed` means it may have been sent. Commit `armed`, the full peak reservation, and
the command identity in one transaction. Only the task receiving the new arm result
may send. Finding an existing arm result never authorizes replay. This is at-most-one
dispatch by mllm per step, not exactly-once execution by an engine.

Separate four identities: deployment revision, lifecycle generation, controller
session, and runtime incarnation. Parking/waking preserves the incarnation and the
endpoint lease. An authorized cold replacement creates a new incarnation. A process
PID is only one component of identity, not an incarnation identifier.

Keep route publication (`admission_enabled`) distinct from runtime dispatch
(`dispatch_enabled`). A parked route remains discoverable. Administrative stop or
suspend fences automatic activation immediately, even while an older control call
is outstanding. That old call may finish physically; its stale completion cannot
publish Ready or release capacity. Reconciliation then accounts for its result.

### Dependency amendments

1. Consume A2b's existing `mllm-domain/src/completion.rs` contract and controller
   re-export. Resource validation also lives in domain from A1; scheduler
   re-exports it. No module relocation or domain-to-scheduler dependency.
2. Make A2a footprint encoding/decoding and snapshot reading `pub(crate)`.
   Extract its increase implementation into a transaction-scoped helper. The
   coordinator calls that helper inside its own immediate transaction; never call
   `reserve_increase` and write a step in separate transactions.
3. Make A2c's session check `pub(crate)`. Fence every mutating lifecycle method,
   including completion, binding changes, reconciliation, and lease settlement.
4. At cutover remove public unfenced resource mutation. Legacy store setters may
   remain only for migration/tests; production controllers cannot call them.
5. A2a's `kind == "model"` check becomes managed-binding ownership verification.
   Existing profile strings are not silently rewritten into model identities.
   A3 supplies explicit effective bindings and legacy reconciliation input.

### Execution order across A2d and A3

Task numbering groups responsibilities, not an instruction to finish A2d before
starting A3. Use this dependency order; keep every partial task visibly incomplete:

1. A2d Tasks 1–3. In Task 2, define `RuntimeError`, `RuntimeAction`, and
   `RuntimeCommand` once in adapters `traits.rs`; controller re-exports them.
   Task 7 still changes `EngineAdapter` and all consumers together. Task 2 also
   starts store `lifecycle.rs` and its private tests: bind incarnation and endpoint
   in one immediate transaction under current session and deployment fence.
   Include store `lib.rs` and `dispatch.rs` in Task 2 scope. Stage Task 3's exact
   `DeploymentFence` and `LifecycleError` declarations with this first writer;
   reuse them rather than introducing temporary parallel contracts. Task 3 extends this
   module. No public unfenced binding writer or temporary release bypass.
2. A3 Task 1 configuration foundations, then Task 2 V6 policy/run persistence
   and Task 3 V7 event-schema/writer foundations, before completing A2d Task 4.
   Preserve migration order V5, V6, V7. Management writers emit events in their
   own transactions from introduction; no separate event commit. Finish remaining
   A3 acceptance/snapshot work after coordinator interfaces exist. Partial slices
   do not close their parent tasks.
3. A2d Tasks 4–6 and preparatory slices of Tasks 7–9, plus Task 10 fake composition
   and regression work. Stage persisted-command entry additively on the same
   adapters-owned trait; test new coordinator/router internals through test-only
   composition. Keep Tasks 7–9 open. Production consumer replacement, router
   dependency replacement, and legacy-method removal belong to the joint switch.
   Arm validates real
   persisted policy and run authority in the same transaction as the grant.
   Missing or mismatched authority denies effects. No allow-all policy, nonempty
   qualification-token shortcut, or production compilation stub. Run update/arm
   race tests once both writers exist before marking either acceptance complete.
4. Finish A3 Tasks 1–5 and Task 6 logging/security foundations. Perform one joint
   A2d Task 10/A3 Task 6 production switch: validated configuration, separate
   credentials, policy enforcement, and new coordinator become active while F1
   lifecycle authorities are retired. Before that switch, keep new composition
   test-only. Until then retain only existing F1 production entry points; expose
   no alternate activation authority. At the switch remove all old lifecycle
   methods and consumers together, then close Tasks 7–10 and A3 Task 6.

Task 2 ownership persistence stores credential references only. Reserve before
spawn; identity updates require exact session, fence, and incarnation. Retain
bindings/endpoints through ambiguous spawn or bind failure. Task 5 verified cleanup
is the sole managed-runtime release path. Attached accounting uses A3's separate
verified external-accounting reconciliation, never managed cleanup or engine control.
Tests before Task 5 assert retention, not fixture cleanup
through a production bypass.

This sequencing adds no F2 scope or live authority. A2d is not complete until the
joint production gate passes; A3 is not complete while any sliced task remains open.

## 2. File map

| Files | Responsibility |
|---|---|
| Existing `crates/mllm-domain/src/completion.rs` | Consume shared A2b verification without relocation |
| `crates/mllm-store/src/schema.rs`, `migrations.rs` | V5 lifecycle, binding, endpoint, and evidence schema |
| `crates/mllm-store/src/lifecycle.rs`, `src/lifecycle/tests.rs` | Atomic acceptance, claims, arm/complete/reconcile transactions |
| `crates/mllm-store/src/resource_ledger.rs`, `dispatch.rs`, `lib.rs` | Transaction-scoped helpers and removal of bypasses |
| `crates/mllm-controller/src/sequence.rs`, `tests/sequence.rs` | Fit-based sequence search and preinitialization forecast |
| `crates/mllm-controller/src/coordinator.rs`, `tests/coordinator.rs` | Owned worker, common activation path, deadlines, administrative fencing |
| `crates/mllm-controller/src/runtime.rs`, `tests/runtime_bindings.rs` | Immutable adapter/forwarder/credential bindings |
| `crates/mllm-controller/src/recovery.rs`, `tests/recovery.rs` | Restart inspection and explicit uncertain-state reconciliation |
| `crates/mllm-launchers/src/ownership.rs`, `tests/ownership.rs`, `src/exec.rs`, `src/lib.rs`, `Cargo.toml` | Controller file lock and runtime process identity/cleanup |
| `crates/mllm-adapters/src/traits.rs`, `src/fake.rs`, `src/vllm/mod.rs`, `src/vllm/adapter.rs`, `src/vllm/http.rs` | Engine-neutral controls and terminal-aware forwarding |
| `crates/mllm-router/src/chat.rs`, `stream.rs`, `admission.rs`, `switch.rs`, `lib.rs`, `tests/router_core.rs`, `tests/router_stream.rs`, `tests/switching.rs` | Queue-only router policy and deployment-bound dispatch |
| `crates/mllm-controller/src/operations.rs`, `src/lib.rs`, `Cargo.toml`, `crates/mllm-cli/src/roles.rs` | Compatibility facade and single production cutover |
| `docs/design/adr/0008-single-host-coordinator.md` | Record transaction, identity, and uncertainty decisions |

Each task below has a RED/GREEN cycle. Its named additional cases are acceptance
requirements, not permission to replace assertions with comments. Commit only the
listed task files at execution time; no commit is requested during planning.

### Task 1: Share completion validation and add durable lifecycle schema

**Interfaces:** Preserve all A2b type names and signatures under
`mllm_domain::completion`. Append `SCHEMA_V5` to `MIGRATIONS`.

- [ ] Keep A2b completion tests in domain and observation tests in agent;
  add the migration test below to `migrations.rs` and run
  `cargo test -p mllm-store v5_lifecycle_schema`. Expect RED: missing table.

```rust
#[test]
fn v5_lifecycle_schema() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    apply(&conn).unwrap();
    apply(&conn).unwrap();
    for table in ["runtime_bindings", "endpoint_leases", "lifecycle_runs",
                  "lifecycle_claims", "lifecycle_steps", "lifecycle_evidence"] {
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "{table}");
    }
}
```

- [ ] Add this DDL. JSON payloads use versioned private store DTOs, bounded to
  1 MiB each before serialization/deserialization. They contain identities,
  recipes, observations, and evidence only, never secrets or inference content.

```sql
CREATE TABLE runtime_bindings(
  id TEXT PRIMARY KEY,
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  incarnation TEXT NOT NULL UNIQUE,
  ownership TEXT NOT NULL CHECK(ownership IN ('managed','attached')),
  binding_json TEXT NOT NULL,
  identities_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('reserved','live','uncertain','released'))
);
CREATE UNIQUE INDEX one_retained_binding ON runtime_bindings(deployment_id)
  WHERE state!='released';
CREATE TABLE endpoint_leases(
  host TEXT NOT NULL,
  port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  PRIMARY KEY(host,port)
);
CREATE TABLE lifecycle_runs(
  operation_id TEXT PRIMARY KEY REFERENCES operations(id),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  generation INTEGER NOT NULL CHECK(generation>0),
  session_id TEXT NOT NULL,
  action TEXT NOT NULL CHECK(action IN ('activate','park','stop','prepare','reconcile')),
  state TEXT NOT NULL CHECK(state IN ('queued','running','uncertain','succeeded','failed')),
  deadline_ms INTEGER NOT NULL,
  plan_json TEXT NOT NULL
);
CREATE UNIQUE INDEX one_activation ON lifecycle_runs(deployment_id,revision,generation)
  WHERE action='activate' AND state IN ('queued','running','uncertain');
CREATE TABLE lifecycle_claims(
  deployment_id TEXT PRIMARY KEY REFERENCES deployments(id),
  operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  revision INTEGER NOT NULL CHECK(revision>0),
  generation INTEGER NOT NULL CHECK(generation>0)
);
CREATE TABLE lifecycle_steps(
  id TEXT PRIMARY KEY,
  operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  session_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('planned','armed','uncertain','completed','cancelled')),
  step_json TEXT NOT NULL,
  grant_id TEXT UNIQUE REFERENCES resource_grants(id),
  UNIQUE(operation_id,ordinal)
);
CREATE TABLE lifecycle_evidence(
  step_id TEXT PRIMARY KEY REFERENCES lifecycle_steps(id),
  evidence_json TEXT NOT NULL,
  committed_epoch INTEGER NOT NULL CHECK(committed_epoch>=0)
);
```

`step_json` freezes action, revision/generation, incarnation, qualification ID,
full recipe, process identities when known, issue/deadline times, and expected
completion milestones. `plan_json` freezes selected members and ordered steps.
For cold launch, identities are initially empty and must be established by the
owned launch receipt, not learned by accepting whichever server answers the port.

- [ ] Run the migration test and `cargo test -p mllm-domain -p mllm-store`.
  Expect GREEN; forward-only upgrades preserve legacy reservations and close gates.
- [ ] Commit: `git commit -m "feat: persist coordinator steps and shared completion contracts"`
  after staging only Task 1 files.

### Task 2: Establish lifetime controller and immutable runtime ownership

**Files:** Launcher ownership, controller runtime, adapters shared declarations,
and store lifecycle ownership files named in the cross-plan execution order.
**Interfaces:** `ControllerLock::acquire(path: &Path) -> io::Result<ControllerLock>`;
the guard owns the locked file for the entire worker lifetime. Runtime lookup is
`binding(deployment_id: &str, revision: i64) -> Result<Arc<RuntimeBinding>, RuntimeError>`.
Adapters-owned `RuntimeError` variants are `Missing`, `StaleRevision`, `Unsupported`,
`Uncertain(String)`; controller re-exports the same type.

- [ ] Add this test to launcher `tests/ownership.rs`. Run
  `cargo test -p mllm-launchers --test ownership`; expect RED: missing exported type.

```rust
#[test]
fn controller_ownership_lasts_until_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("controller.lock");
    let first = mllm_launchers::ControllerLock::acquire(&path).unwrap();
    assert!(mllm_launchers::ControllerLock::acquire(&path).is_err());
    drop(first);
    assert!(mllm_launchers::ControllerLock::acquire(&path).is_ok());
}
```

- [ ] Implement the guard using `nix::fcntl::Flock<File>` and
  `FlockArg::LockExclusiveNonblock`; return lock acquisition errors without
  beginning a coordinator session. Open with `create(true)`, `truncate(false)`,
  `read(true)`, `write(true)`, Unix mode `0o600`. Keep the lock path under the
  protected canonical state directory, do not unlink it, and do not inherit its
  descriptor into engine children. Test a child process cannot start another
  controller while the original process holds the lock.

```rust
pub struct ControllerLock {
    _file: nix::fcntl::Flock<std::fs::File>,
}
impl ControllerLock {
    pub fn acquire(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().create(true).truncate(false)
            .read(true).write(true).mode(0o600).open(path)?;
        let locked = nix::fcntl::Flock::lock(
            file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| std::io::Error::from_raw_os_error(error as i32))?;
        Ok(Self { _file: locked })
    }
}
```

- [ ] Define `RuntimeBinding` with `id`, `deployment_id`, `revision`,
  `incarnation`, `qualification_id`, `recipe: RecipeFootprints`,
  `ownership: RuntimeOwnership`, `endpoint: String`, `credential_ref: String`,
  `driver: Arc<dyn EngineAdapter>`, and `forward: Arc<dyn ChatForward>`.
  Identity strings are owned `String`; revision is `i64`.
  `RuntimeOwnership` is `Managed | Attached`. Keep secret material in a separate
  non-Debug object; serializable DTOs store only the credential reference.
  Never replace a retained binding in place.
- [ ] Reserve a distinct loopback host/port tuple durably before launching.
  Probe binding availability with a temporary listener; a subsequent bind race
  is a launch failure, never grounds for adopting/killing the occupant.
  Keep the endpoint lease through parking and uncertainty. Release it only in
  the verified cleanup transaction. A launch with ambiguous ownership retains
  its reservation even when no API PID was captured.
- [ ] Replace the launcher's in-memory counter identity with persisted boot ID,
  `/proc/<pid>/stat` start ticks, and qualified API/worker role membership.
  Persist a unique launch incarnation before spawn. Launch supervision must
  associate children with that incarnation before allowing model initialization;
  an unacknowledged spawn is reconciled, never retried under a new incarnation.
  Compare identities immediately before signaling; do not signal a reused PID
  or infer whole-runtime exit from API-server exit. Where complete worker
  ownership cannot be established, return `Uncertain` and retain accounting.
- [ ] Stage durable-launch supervision alongside legacy launcher compatibility.
  Keep child gated before initialization until its actual API boot/start identity
  is persisted under the existing incarnation and current session/deployment fence.
  An API receipt is not full runtime proof; qualified worker enrollment requires
  the trusted recipe collector. Missing collector support remains `Uncertain`.
  Test gate association and ambiguous-spawn retention without retry. Concrete
  engine collectors belong to adapter work. Retire legacy authority at joint cutover.
- [ ] Add assertions for two same-engine deployments getting different bindings,
  endpoints, and credential references; parked lookup retains all three; wrong
  revision is rejected; attached binding refuses every lifecycle control.
  Run launcher ownership tests and controller runtime-binding tests to GREEN.
- [ ] Commit: `git commit -m "feat: bind local runtimes to durable deployment ownership"`.

### Task 3: Accept one operation and claim all affected lifecycles

**Files:** Store `lifecycle.rs` and its private tests.
**Interfaces:** Export the following request and result types. All fields below
are public except the `CoordinatorSession` internals from A2c.

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentFence {
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunAction { Activate, Park, Stop, Prepare, Reconcile }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedRun { pub operation_id: String, pub joined: bool }
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("store: {0}")] Sql(#[from] rusqlite::Error),
    #[error("stale coordinator or deployment fence")] Stale,
    #[error("lifecycle conflict")] Conflict,
    #[error("activation disabled")] Disabled,
    #[error("invalid lifecycle input")] Invalid,
    #[error("resource or evidence check failed: {0}")] Rejected(String),
}
```

Produce these `Store` methods:

```text
accept_activation(&self, session: &CoordinatorSession,
  target: &DeploymentFence, deadline_ms: i64) -> Result<AcceptedRun, LifecycleError>
claim_sequence(&self, session: &CoordinatorSession, operation_id: &str,
  members: &[DeploymentFence], plan_json: &str) -> Result<(), LifecycleError>
fence_stop(&self, session: &CoordinatorSession, target: &DeploymentFence,
  deadline_ms: i64) -> Result<AcceptedRun, LifecycleError>
fence_suspend(&self, session: &CoordinatorSession, target: &DeploymentFence,
  deadline_ms: i64) -> Result<AcceptedRun, LifecycleError>
handoff_claims(&self, session: &CoordinatorSession, successor_operation_id: &str,
  predecessor_operation_id: &str, members: &[DeploymentFence])
  -> Result<(), LifecycleError>
```

Suspend records a Reconcile lifecycle action without cleanup authority. Handoff
accepts an already accepted Stop/Reconcile successor and store-generated history;
caller plan input never supplies evidence or creates stop permission.

- [ ] Test that concurrent independent SQLite connections accepting activation
  for the same fence return one operation ID, with exactly one `joined == false`.
  Test that different deadlines do not extend an already accepted operation;
  each caller still has its own shorter waiting deadline. Task3 proves the durable
  deadline stays unchanged; Task9 proves caller timeouts through its real wait path.
  Run
  `cargo test -p mllm-store lifecycle::tests::activation`; expect RED.
- [ ] Implement each method with `TransactionBehavior::Immediate`. Check the
  current session and exact deployment fence before mutation. Acceptance rejects
  disabled admission, suspended deployments, and an explicit stopped desired
  state unless the caller is an authorized administrative start. Represent that
  authorized start by setting desired state and accepting activation in the same
  transaction; add `accept_start` with the same signature as `accept_activation`.
  Routed activation never changes desired state. An uncertain matching operation
  is joined for observation, not replaced by a fresh activation.
- [ ] Use the partial unique activation index as the final race barrier. Insert
  the operation row and lifecycle row atomically. `claim_sequence` sorts and
  deduplicates member IDs, validates every revision/generation, and inserts every
  claim in one transaction. Any conflicting claim rolls back all new claims.

```sql
INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation)
VALUES (?1,?2,?3,?4);
-- Do not use INSERT OR REPLACE: it would steal another operation's claim.
```

- [ ] `fence_stop` increments the target generation, closes dispatch, disables
  automatic activation through desired state, and records the explicit stop
  operation atomically. It does not delete leases, old claims, bindings, or
  reservations. The worker reconciles a superseded armed command before cleanup.
  Implement suspend with the same immediate fence but without implicit cleanup.
- [ ] Add fenced claim handoff for superseding stop/reconciliation. In one immediate
  transaction validate current session, current member fences, predecessor operation,
  and every affected retained claim. Transfer those claims to the new reconciliation
  operation while recording predecessor claim/step links in versioned durable history.
  Preserve old steps, grants, leases, bindings, and reservations; transfer is not release.
  Normal claim acquisition still cannot steal claims. Old callbacks fail their original
  fences. For same-session handoff, increment generation and close dispatch on each
  transferred member still at its predecessor claim generation. Preserve other members'
  desired/suspended state and all resources. Already-fenced stop targets do not bump
  again. Record generation history and resulting fences; synchronize the successor's
  target run fence without rewriting predecessor runs or evidence. The owned worker
  cannot send new cleanup controls until the preceding owned
  command task has ended; restart first requires the lifetime controller lock.
  When observation cannot resolve an old effect, explicit Stop may proceed to cleanup
  only with verified full owned process identities and its original cleanup authority.
  Unknown ownership stays uncertain. Transfer of other sequence members grants only
  reconciliation, not permission to stop them. Release claims only after verified
  evidence settles their effects. Test stop-during-Restore and restart through this
  handoff, including stale handoff, partial rollback, and uncertain-identity rejection.
- [ ] Add opposing-sequence assertions: one claim transaction wins; the loser
  leaves zero partial claims. Verify stale sessions, stale revisions, and stop
  versus ready completion cannot mutate ownership. Run the store lifecycle suite
  to GREEN, then commit `feat: coordinate durable activation and lifecycle claims`.

### Task 4: Make reservation and dispatch intent one atomic transaction

**Files:** Store `resource_ledger.rs`, `lifecycle.rs`, private lifecycle tests.
**Interfaces:** `ArmResult` below; `arm_step` consumes a persisted planned step,
not caller-provided replacement footprints.

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum ArmResult { New { step_id: String }, AlreadyRecorded }
```

```text
arm_step(&self, session: &CoordinatorSession, step_id: &str,
  context: AdmissionContext<'_>) -> Result<ArmResult, LifecycleError>
```

- [ ] Add failure injection after grant insertion but before step update. Assert
  rollback leaves epoch, owner footprint, grant count, and step state unchanged.
  Add a retry assertion that an existing arm returns `AlreadyRecorded`, never
  `New`. Run `cargo test -p mllm-store lifecycle::tests::arm`; expect RED.
- [ ] Extract A2a into `reserve_increase_in_transaction` with parameters
  `(&rusqlite::Transaction<'_>, &GrantRequest, AdmissionContext<'_>)` and return
  `Result<GrantReceipt, ResourceStoreError>`. It must not begin/commit a nested
  transaction. Its standalone wrapper becomes test-only at cutover.
- [ ] Implement `arm_step`: begin immediate transaction; check session; load
  frozen step and binding; check every sequence claim and current member fence;
  check preceding steps completed; check operation deadline, qualification and
  observation freshness; derive the phase from the frozen recipe; invoke the
  increase helper; update the step to armed; commit. Ready→Parking reserves the
  parking peak even if parking will eventually free memory.
- [ ] Revalidate the current host-policy revision inside this same transaction.
  A3 policy updates serialize against arm transactions. Never execute a planned
  increase using superseded limits; an already armed grant remains charged.
  Normal warm operations require Qualified evidence. Candidate qualification
  steps instead require A3's exact run-scoped authorization, frozen recipe,
  host identity, conservative phase grants, deadline, and owned cleanup scope.
  Candidate completion never promotes qualification or opens ordinary routing.
  Candidate `qualification_id` is a namespaced reference to its persisted run
  authorization, not an entry in the Qualified catalog. Verify its kind and exact
  scope at arm/completion; never treat a nonempty token string as qualification.

```sql
UPDATE lifecycle_steps SET state='armed',grant_id=?2
WHERE id=?1 AND state='planned' AND session_id=?3;
-- Require exactly one changed row before committing the grant transaction.
```

- [ ] Include exact runtime launch settings authorized by the committed grant and
  qualified recipe in the frozen step; adapters cannot select larger KV allocations
  or a different recipe. A3's normalized owned settings include every allocator
  request in qualification identity. Authorize those settings unchanged or deny;
  changing a rendered value requires a new effective recipe and qualification.
  Total phase allocations and host KV accounting do not uniquely determine backend
  KV pools. Validate against qualified backend geometry and allocation evidence.
  Resource-neutral control substeps still require a unique persisted intent but
  do not mint a second resource grant. A Stop intent is armed under its explicit
  authorization without inventing a resource-increasing phase.
- [ ] Persist a shared domain execution context with the complete TransitionToken,
  binding ID/incarnation, issue/deadline times, identity scope, settled completion
  target, grant reference and exact normalized launch settings. Expose a session-
  fenced read of this context and persisted action. Loading/cloning context grants
  no send permission; only `ArmResult::New` permits dispatch. Validate action/context
  combinations. The completion target is settled Ready/Parked, not the temporary peak.
- [ ] Test process exit immediately after commit and before send: restart marks
  the arm uncertain, retains its peak, and sends nothing automatically. Also test
  stale-session arm, stale ledger epoch, expired observation, altered binding,
  failed claim checks, and parking peak denial. Run store tests to GREEN.
- [ ] Commit: `git commit -m "feat: atomically reserve and arm lifecycle effects"`.

### Task 5: Commit qualified completion and cleanup without caller-chosen release

**Files:** Store lifecycle files and shared completion module.
**Interfaces:**

```text
complete_step(&self, session: &CoordinatorSession, step_id: &str,
  evidence: &CompletionEvidence, now_ms: i64, ttl_ms: i64)
  -> Result<(), LifecycleError>
```

`CompletionEvidence` is the shared A2b type. It is constructed only by trusted
runtime collectors; there is no management endpoint accepting this object.

- [ ] Test a valid parked completion, then mutate each token field, worker start
  identity, qualification, deadline, and expected milestone in separate cases.
  Assert every invalid case leaves the peak, gate, leases, and endpoint unchanged.
  Run `cargo test -p mllm-store lifecycle::tests::complete`; expect RED.
- [ ] In one immediate transaction: check session; load the armed step, binding,
  operation, and all claims; derive `CompletionExpectation` from their frozen
  data; run `verify_completion`; validate current fences; replace the footprint;
  increment the ledger epoch; insert evidence; complete the step. Reject a
  caller trying to select a smaller footprint: this API has no footprint argument.
  Compare a repeated completion to recorded canonical evidence and return success
  without incrementing the epoch again; mismatched replay is a conflict.
- [ ] Cold Initialize completion requires the complete API/worker identity set
  already associated durably with the exact owned binding incarnation. Construct
  expected identities from that association, not from the incoming evidence alone.
  A collector correlates observations with its supplied token; never rewrite a
  mismatched returned token to make completion validate.

```sql
UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1;
UPDATE resource_ledger_meta SET epoch=epoch+1 WHERE singleton=1;
INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch)
VALUES (?1,?2,?3);
UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed';
```

- [ ] Park completion requires qualified quiescence plus release, exact retained
  identities, and a closed gate. Only evidence establishing *all* backend work
  drained may settle all prior-generation/session leases in that transaction.
  A2b's ordered `Quiesced, MemoryReleased` evidence supplies that assertion only
  for a recipe whose collector qualifies it. Do not delete leases based on a
  zero local counter, an idle-looking HTTP server, or a lost acknowledgement.
- [ ] Ready completion requires allocations, weights, cache validity, and model
  usability. Open dispatch only when all request leases requiring reconciliation
  have been settled, the current desired state permits serving, suspension is
  false, and revision/generation/session remain current. A cold launch first
  persists its verified launch identities; it cannot use an empty identity list
  to satisfy A2b. A recovery probe alone cannot settle uncertain old requests.
- [ ] Add `CleanupEvidence { binding_id: String, incarnation: String,
  identities: Vec<ProcessIdentity>, observed_at_ms: i64, receipt: String }` and
  `complete_cleanup(&self, session: &CoordinatorSession, step_id: &str,
  evidence: &CleanupEvidence, now_ms: i64, ttl_ms: i64)` returning
  `Result<(), LifecycleError>`. The collector produces it only after the entire
  owned runtime is verified gone. Store checks the frozen cleanup authority,
  binding, identities, freshness, and session, then deletes the resource owner
  and endpoint leases, settles work, marks binding released and state stopped,
  and records evidence in one transaction. Empty receipts, attached ownership,
  partial worker exit, and PID reuse cannot satisfy cleanup.
- [ ] Keep claims while any step is armed/uncertain. Release claims only after
  completed effects or verified reconciliation; failed-but-uncertain is not a
  resource-free terminal state. Task 3 fenced handoff transfers retained ownership
  with predecessor history; it does not release claims or their resource charges.
  Run completion/cleanup/replay tests to GREEN.
- [ ] Commit: `git commit -m "feat: require qualified evidence for release and readiness"`.

### Task 6: Build fit-based sequences, including usable preinitialization

**Files:** Controller `sequence.rs`, `tests/sequence.rs`.
**Interfaces:** Pure planner consumes A1 `LedgerSnapshot`, `RecipeFootprints`,
`AdmissionContext`, and `ForecastStep`; produces ordered `Vec<ForecastStep>`.
`PlanError` contains `Invalid`, `NoSafeSequence`, and `SearchLimit` variants.
The planner constructs contextual diagnostics from its known owner, recipe, limit,
observation, failed step, and `ResourceError` inputs; `ResourceError` remains unchanged.
Diagnostics distinguish observed values from forecasted or required bounds. A unit
error does not justify naming a unique causal owner or domain. Diagnostic construction
must not duplicate admission calculations or change admission denial decisions.

- [ ] Add a pure helper and its test as the first RED/GREEN step:

```rust
pub fn may_reclaim(warm: bool, qualified_park: bool, allow_cold_stop: bool) -> bool {
    qualified_park || (!warm && allow_cold_stop)
}
#[test]
fn warm_commitment_never_grants_cold_eviction() {
    assert!(!may_reclaim(true, false, true));
    assert!(may_reclaim(true, true, false));
    assert!(may_reclaim(false, false, true));
    assert!(!may_reclaim(false, false, false));
}
```

- [ ] Implement deterministic breadth-first sequence search. State is the full
  owner→phase mapping, not aggregate free bytes. Start with direct target
  Cold→Ready or Wake→Ready; if admission denies it, enumerate qualified eligible
  Ready→Parking→Parked transitions in deployment-ID order. Evaluate each peak
  and steady step with `forecast_sequence`. An operation may park multiple owners;
  it may not ignore retained services or attached owners. Use a configured finite
  state-expansion budget and return `SearchLimit`, not a false capacity diagnosis,
  when exhausted. Stop-based reclamation is a separate authorized cleanup step
  for restart-only deployments, never a synthetic zero-byte parking footprint.
- [ ] Reject victims with an opposing claim, disabled automatic reclamation,
  attached ownership, an unqualified park recipe, or an unexpired admission
  window. Do not reset a window when more requests arrive. Expiry closes new
  admission; it does not promise an existing stream will finish or authorize kill.
- [ ] For preparation, freeze the explicitly requested ordered member set and
  requested final Ready/Parked arrangement. Preflight every initialization,
  park, and final wake before changing anything. Do not select unrelated active
  deployments as victims. From a final both-parked state, separately forecast
  each member's future wake with all other retained owners still charged. If
  either has no safe path, reject the requested usable warm arrangement.
- [ ] Add numerical fixture assertions using bytes (small integers, not GiB):
  domain capacity/managed 100, headroom 0; A Ready 40/Parking 50/Parked 10;
  B Cold 70/Ready 40/Wake 60/Parked 10. Direct cold B with ready A fails; parking
  A then cold B succeeds and retains A's 10. A Parking 110 fails before closure.
  With B Cold 50, both can become ready without parking A. Set available memory
  below a required increase and assert failure despite ledger fit. Include an
  extra retained owner of 31 and assert the original 10+70 sequence now fails.
- [ ] Run `cargo test -p mllm-controller --test sequence` to GREEN and commit
  `feat: plan coexistence and warm preparation from complete footprints`.

### Task 7: Drive durable steps from one owned coordinator worker

**Files:** Controller coordinator/runtime files and tests; adapter traits/fake/vLLM;
agent composition, router consumers, and harness fakes using the old trait.
**Interfaces:** Evolve existing `mllm_adapters::traits::EngineAdapter` in place.
Define `RuntimeAction`, `RuntimeCommand`, and `RuntimeError` in that module from
the start. Controller runtime re-exports shared types. No second lifecycle trait,
adapter-to-controller dependency, or later SGLang-driven relocation. Adapters
never choose victims or change reservations. Retain async-trait in adapters.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeAction { Initialize, Drain, Park, Restore, Probe, Stop, Inspect }
#[derive(Clone, Debug)]
pub struct RuntimeCommand {
    pub action: RuntimeAction,
    pub context: mllm_domain::execution::StepExecutionContext,
}
#[async_trait::async_trait]
pub trait EngineAdapter: Send + Sync {
    async fn execute(&self, command: &RuntimeCommand)
        -> Result<mllm_domain::completion::CompletionEvidence, RuntimeError>;
}
```

Introduce the shared context with Task 4's first persisted consumer. Domain
`StepExecutionContext` contains the full existing `TransitionToken`, `binding_id`,
`incarnation`, `issued_at_ms`, `deadline_ms`, `identities`, `completion_target`,
`grant_id`, and optional `launch_settings: ProfileLaunchSettings` from domain's
launch module. Identity scope is `Retained(Vec<ProcessIdentity>)` or `OwnedLaunch`;
the latter is legal only for exact managed cold initialization. Launch settings
are mandatory for initialization/allocation actions and frozen from the qualified
recipe. Control-only and cleanup steps carry no invented allocation settings or
Ready/Parked completion target. No second token or lifecycle trait.

The immutable adapter construction context includes the binding's frozen engine,
model, recipe, qualification, executable, endpoint and protected credential references.
Before I/O, reject detectable command/context mismatches and unsupported actions.
Never re-read a mutable profile to choose a retained runtime's endpoint or recipe.
Store independently rechecks current session, claims and generations. Full token,
settings and identity-scope round trips, wrong-token evidence, context replay,
profile edits after binding creation and unknown cold identities require tests.

Stop uses the launcher's cleanup collector, not this ready/park completion result.
Drain/Inspect may return `Uncertain` until the recipe's collector can establish
the required evidence. Read-only observations do not themselves arm resource
effects. Each driver is constructed from its immutable binding and cannot resolve
another deployment's endpoint through a global profile lookup.

- [ ] Migrate every existing EngineAdapter implementation and consumer: fake,
  vLLM, controller, agent, router, and harness. Replace legacy lifecycle methods
  with the persisted-command contract; retain separately useful observation
  methods only with explicit evidence semantics. Compile all affected crates.
  Do not leave a public RuntimeDriver or an unfenced legacy lifecycle facade.
  These removal/production-consumer requirements close at the joint cutover.
  Preparatory work adds the command entry to the same trait without enabling
  new production composition; keep this task open until legacy removal completes.

- [ ] Add this send-decision helper and assertion before implementing the worker:

```rust
pub fn permits_send(result: &mllm_store::lifecycle::ArmResult) -> bool {
    matches!(result, mllm_store::lifecycle::ArmResult::New { .. })
}
#[test]
fn recorded_intent_is_not_replay_permission() {
    use mllm_store::lifecycle::ArmResult;
    assert!(permits_send(&ArmResult::New { step_id: "s".into() }));
    assert!(!permits_send(&ArmResult::AlreadyRecorded));
}
```

- [ ] Worker loop: select the oldest eligible queued operation; read fresh
  observations and complete ledger; compute sequence; atomically claim/freeze it;
  for each step revalidate observation and fences, close dispatch and drain when
  needed, arm the effect, execute only on `New`, collect evidence, complete
  transactionally, and notify observers. Persist partial progress after every
  completed step. A later failure leaves earlier members' actual states visible.
  B cannot reserve its peak until A's release completion commits.
  Native meminfo provides no per-owner resident floors. A1 admission charges all
  outstanding owner commitments with zero credit unless a qualified collector
  establishes attributed residency in the same observation. Never reuse synthetic
  forecast floors or infer physical credit from a reservation/global free delta.
- [ ] Drain prevents new dispatch first and counts leases across all generations.
  Keep waiting until terminal settlements or qualified all-work quiescence;
  `Unknown` is not Idle. Before invoking a drain barrier that changes engine
  scheduling state, persist a resource-neutral control intent. If barrier outcome
  is uncertain, keep the runtime closed; do not optimistically reopen it.
- [ ] Bound each await by the smaller of operation and protocol deadlines. Timeout,
  worker panic, or dropped control future marks the step uncertain when possible;
  the durable arm is sufficient for recovery even if that write fails. Never
  continue to the next resource-increasing step after uncertainty. Stop accepting
  lifecycle work on an unrecoverable store error; do not continue effects using
  an in-memory approximation of ownership.
- [ ] Adapt `request_transition(Start)` and `auto_activate` to common acceptance.
  `OperationHandle` remains an observer of the same durable operation ID. Keep
  caller deadlines separate from the accepted operation deadline; expired queued
  inference must not later be forwarded merely because activation succeeded.
- [ ] Use a fake driver with an event vector and Tokio barriers to assert:
  concurrent routed/admin activation has one Initialize; dropping both waiters
  does not cancel it; ready A still serves while B initializes; opposing plans
  never overlap claims; timeout has no automatic Stop; stop during Restore prevents
  stale Ready; failed second preparation member preserves the first parked member.
  Run `cargo test -p mllm-controller --test coordinator` to GREEN.
- [ ] Commit: `git commit -m "feat: run activation and preparation through one coordinator"`.

### Task 8: Reconcile restart without replay, adoption, or invented free capacity

**Files:** Controller recovery, store lifecycle, launcher ownership and tests.
**Interfaces:** `RecoveryDisposition` is `VerifiedReady | VerifiedParked |
VerifiedGone | Uncertain`; only collectors, followed by fenced store transactions,
can convert these classifications into state changes.

- [ ] Add this conservative classification kernel and test:

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryDisposition { VerifiedReady, VerifiedParked, VerifiedGone, Uncertain }
pub fn classify_recovery(
    identity_matches: bool, all_owned_gone: bool, ready_proof: bool, park_proof: bool,
) -> RecoveryDisposition {
    if all_owned_gone && !ready_proof && !park_proof {
        return RecoveryDisposition::VerifiedGone;
    }
    if !identity_matches || ready_proof == park_proof || all_owned_gone {
        return RecoveryDisposition::Uncertain;
    }
    if ready_proof { RecoveryDisposition::VerifiedReady }
    else { RecoveryDisposition::VerifiedParked }
}
#[test]
fn answering_http_does_not_recover_a_reused_runtime() {
    assert_eq!(classify_recovery(false, false, true, false), RecoveryDisposition::Uncertain);
    assert_eq!(classify_recovery(true, false, false, false), RecoveryDisposition::Uncertain);
    assert_eq!(classify_recovery(true, false, true, true), RecoveryDisposition::Uncertain);
}
```

The booleans are collector conclusions, not raw HTTP flags. `all_owned_gone`
means the entire known incarnation is gone, not that its original API PID vanished.

- [ ] Startup order: lifetime file lock; open/migrate store; begin session (close
  gates and retain leases); mark prior armed/running work uncertain; reconstruct
  immutable bindings from protected references; inspect complete owned runtimes;
  record a new reconciliation operation and evidence; commit verified outcomes.
  Old-session steps are not relabeled as newly dispatched. Recovery uses new step
  IDs linked to the old operation and binding.
- [ ] Surviving ready processes need identity, qualification, work reconciliation,
  cache/model proof before reopening. Surviving parked processes need qualified
  release evidence before reducing an uncertain peak. A vLLM `is_sleeping=true`
  response alone establishes neither deep release nor retained workers.
- [ ] Preserve legacy `reservations` as a hard launch block until an explicit
  migration/reconciliation maps every row to a current owner and conservative
  footprint. Retained attached consumption is a fixed, non-reclaimable owner;
  update its bound only through fresh explicit configuration/evidence, never by
  treating low observed global use as release. Unknown ownership stays charged.
- [ ] Recovery tests use a fresh store connection and session for each crash point:
  before arm, after arm/before send, after send/before acknowledgement, after
  acknowledgement/before completion, after completion/before notification.
  Assert no replay, no duplicate resource release, no port reuse, no old-session
  settlement, and no inference replay. Add partial worker exit and PID reuse.
- [ ] Run `cargo test -p mllm-controller --test recovery` to GREEN and commit
  `feat: reconcile retained runtime ownership without blind replay`.

### Task 9: Route through immutable bindings and preserve truthful stream outcomes

**Files:** Router files/tests and adapter forwarding files from the map.
**Interfaces:** Router receives an `Arc<Coordinator>` plus queue limits and public
authentication, not a profile-keyed forwarder map. Resolution returns a current
`DeploymentFence`; binding lookup verifies that exact revision before forwarding.
Use A2c `DispatchTicket` as the sole backend-work accounting authority.
This is the final production interface. Prepare and test new routing internals
before the joint switch; replace production dependencies and close this task there.

- [ ] Introduce distinct backend and delivery results and test settlement first:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendEnd { Completed, CancelledAcknowledged, Uncertain }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryEnd { Delivered, Disconnected, BackpressureTimeout }
pub fn settles_lease(end: BackendEnd) -> bool {
    matches!(end, BackendEnd::Completed | BackendEnd::CancelledAcknowledged)
}
#[test]
fn backend_uncertainty_retains_request_ownership() {
    assert!(!settles_lease(BackendEnd::Uncertain));
    assert!(settles_lease(BackendEnd::Completed));
    assert!(settles_lease(BackendEnd::CancelledAcknowledged));
}
```

- [ ] Change `ChatForward` streaming to an awaited bounded sink rather than
  synchronous `FnMut` plus ignored `try_send`. Define `ChunkSink` with async
  `send(&mut self, chunk: String) -> Result<(), DeliveryEnd>`; forwarding returns
  `Result<BackendEnd, AdapterError>`. After downstream disconnect or send timeout,
  stop delivering but continue bounded backend drain or qualified cancellation.
  Expiry/transport failure returns Uncertain. The owned pump outlives the client
  response; a panic retains the durable ticket for reconciliation.
- [ ] Backend `[DONE]` plus valid protocol termination establishes Completed only
  for a qualified parser. Premature close is Uncertain. Emit downstream `[DONE]`
  only after complete ordered delivery and successful backend completion. A
  disconnected client may still have a Completed backend and a settled lease;
  do not conflate those outcomes. Non-streaming collection must use the same
  terminal-aware parser; remove the vLLM path returning successful JSON after
  ignoring a premature stream end.
- [ ] Request path: authenticate; validate/bound body; resolve current published
  route; acquire queue count/byte budget and fixed deadline; join activation if
  needed; atomically grant dispatch for the current fence; capture its immutable
  binding; forward once. Retry only failed admission before any backend send and
  within the original deadline. Missing state and store errors fail closed.
  There is no `unwrap_or(true)` readiness fallback and no inference replay.
- [ ] Enforce queue count/bytes during the entire activation wait. Ready inference
  bypasses lifecycle serialization but not dispatch registration. Queue permits
  may use ordinary RAII; backend-work leases may not release through Drop.
  Client cancellation before dispatch releases only queue ownership.
- [ ] Add two callers joining the same accepted activation with different wait deadlines:
  short caller times out, long caller continues, operation ID/deadline remain
  unchanged, no duplicate backend send or cancellation of accepted work. Then test
  two same-engine routes return distinct fake model sentinels; close
  versus dispatch has one legal winner; generation changes force re-resolution;
  100 ordered chunks through a 16-slot slow sink lose none; full sink either
  backpressures or fails honestly; premature backend end emits no `[DONE]` and
  retains its lease; disconnect with later confirmed completion settles; uncertain
  cancellation does not settle. Run the three named router test targets to GREEN.
- [ ] Commit: `git commit -m "feat: route with durable leases and terminal-aware forwarding"`.

### Task 10: Cut over production wiring and retire competing authorities

**Files:** Controller operations/lib, CLI roles, router switch/admission/lib,
affected named tests, and ADR 0008.
**Interfaces:** Keep public operation observation compatible where safe; all
activation, stop, park, preparation, and routing delegate to the coordinator.
A3 owns strict configuration and the management surface; it supplies validated
immutable bindings. B supplies the SGLang driver through these same contracts.

- [ ] Run the existing named controller/router/CLI regression targets before
  cutover. Add a composition test using fake bindings for two deployments and
  the real SQLite coordinator; do not use synthetic production memory defaults.
- [ ] Replace standalone's single adapter/forward map with a binding registry,
  lifetime controller guard, session, owned coordinator task, and fresh observer.
  Until A3 wiring is present, expose the composition constructor to tests only;
  do not ship an environment-variable fallback that bypasses effective schemas.
- [ ] In the same cutover remove production references to controller synthetic
  128-GiB/4096-byte activation admission, empty-ledger admission, legacy reservation
  mutation, router-owned `SwitchEngine` eviction, profile-keyed forwarding,
  `InFlight`/`StaticStreamGuard` backend release, and request-owned activation
  leadership. Preserve only independently useful bounded queue/window logic.
  Move legacy-only test fixtures under test configuration, not a runtime flag.
  Remove obsolete lifecycle trait exports and implementations; every engine
  control uses the single adapters-owned EngineAdapter contract.
- [ ] vLLM driver must use qualified drain/park/restore semantics: explicit
  wait-mode level-2 parking for the pinned supported recipe; verified retained
  identities; restore allocations, reload weights where required, invalidate
  caches, then model probe. HTTP liveness or `/v1/models` alone cannot complete
  Ready. A new unqualified recipe returns Unsupported for normal warm use,
  not simulated residue. Only A3's scoped qualification run may exercise reviewed
  candidate controls under conservative grants; it does not advertise Qualified.
  The A2b pinned-source findings are protocol inputs, not live qualification.
- [ ] Add composition assertions covering all Task 6 numerical scenarios through
  routed fake requests, both parked preparation, simultaneous ready inference,
  warm switching without spawn count increase, and restart uncertainty. Assert
  endpoint/credential separation and that rejected activation makes no engine call.
- [ ] Run these explicit targets; do not run the unrelated live-interactive target:

```bash
cargo test -p mllm-domain -p mllm-store -p mllm-scheduler -p mllm-launchers
cargo test -p mllm-controller
cargo test -p mllm-router --test router_core --test router_stream --test switching
cargo test -p mllm-cli --lib
cargo clippy -p mllm-domain -p mllm-store -p mllm-controller -p mllm-router --lib -- -D warnings
git diff --check
```

- [ ] Record fresh test counts and failure-injection results in ADR 0008; do not
  claim GPU qualification or latency improvements from fake tests. Stage only
  cutover files and commit `feat: make the coordinator the single lifecycle authority`.

## 3. Acceptance and remaining milestone coverage

This plan connects Q1 deployment isolation, Q2 transition accounting, Q3 explicit
preparation, Q4 coexistence, Q5 warm switching, Q6 pressure-safe admission, Q7 request
ownership, and Q8 local recovery. Fake tests establish control-plane behavior, not
qualified engine release, concrete recipe footprints, or live serving performance.

A3 must finish effective schemas/revisions, authenticated management and CLI,
attachment input validation, bounded logs/secrets, and UI-ready snapshots/resumable
events (Q9/Q10). B must qualify the SGLang protocol implementation and supported
capabilities alongside the vLLM contract. C must set and measure numerical
guardrails and Q1–Q11 live evidence on host-a. None is implicitly deferred beyond F2.

Before execution, cross-check those plans against this plan's constructor, binding,
operation, evidence, and event transaction boundaries. Do not publish the new
production wiring with A3 validation absent or advertise an unqualified adapter.

## 4. Planning verification record

This is an implementation plan, not implemented product code. Planning checks
cover the DDL, selected pure safety helpers, file links, and inline review of
transaction/identity boundaries. The full coordinator, launcher, adapter, router,
and failure-injection tests listed above must be written and executed during
implementation. No live engine, GPU, or Spark access is part of this record.
