# F2A2c Durable Dispatch Ownership Implementation Plan

**Goal:** Make admission closure atomic with request registration, retain uncertain backend work across local restart, and fence callbacks from obsolete coordinator sessions.

**Architecture:** Add bounded request-lease records to the existing SQLite store. Dispatch checks the current coordinator session, deployment revision/generation, readiness, suspension, and admission gate in the same transaction that registers work. Completion is explicit; dropping a Rust ticket or losing a client does not release a lease.

**Tech Stack:** Existing Rust 2021 workspace, rusqlite 0.37, thiserror, ulid, tempfile. No new dependencies, engine control, public API, or GPU work.

**Spec:** [Approved F2 design](../../design/milestones/f2-sglang-design.md), §§3/5/6, Q7/Q8. Dependencies: [F2A2a store schema](2026-09-12-f2a2a-durable-reservation-transactions.md) and [F2A2b evidence contract](2026-09-12-f2a2b-runtime-evidence.md).

## Global Constraints

- “Dispatch atomically checks current generation and open admission while registering in-flight work, preventing races with admission closure.”
- “Client disconnect is not proof of engine cancellation.”
- “Unknown engine work does not qualify as safe quiescence for parking.”
- “Drain timeout does not implicitly authorize a process kill.”
- “Streams interrupted by a controller crash fail honestly, without an exactly-once or resumable-inference promise.”
- “Only host-a is authorized for subsequent live work.” No live work is part of this plan.
- Never persist request bodies, prompts, authorization headers, or response content in a request lease.
- Preserve the unrelated `crates/mllm-cli/tests/live_interactive.rs`; do not edit, stage, or execute it.

---

## 1. Integration decisions

The F1 router separates readiness lookup from in-flight registration. Its stream pump
also releases accounting on abnormal backend termination, while the non-streaming
error path can release through guard destruction. F2 must not use those paths as
proof of safe drain.

Use durable metadata leases for accepted backend work. This introduces one SQLite
transaction at dispatch and one at confirmed completion; measure that overhead during
F2C rather than claiming it is free. Normal inference does not hold a database or
coordinator mutex while waiting for engine output.

Keep route availability separate from runtime dispatch. The existing
`admission_enabled` controls model listing; v4 adds `dispatch_enabled`, defaulting to
closed. Parking and recovery close only dispatch. A route remains discoverable and
eligible to queue for activation according to its policy. Only the later qualified
completion transaction may open the runtime gate. Tests set this field directly as
fixture setup; no production bypass method is added.

The semantics are:

| Event | Lease | Admission/readiness consequence |
|---|---|---|
| Dispatch wins the transaction before close | Created | Drain must account for this work, even if forwarding has not started |
| Close wins before dispatch | Not created | Request remains queued within its deadline or receives a retryable rejection |
| Client disconnect, task panic, transport error, premature backend close | Retained; mark uncertain when possible | Cannot establish safe quiescence |
| Qualified backend terminal completion or acknowledged cancellation | Removed explicitly | Other outstanding leases still count |
| Controller restart | All retained leases become uncertain; all dispatch gates close | Reconcile before opening any gate |
| Callback from an old coordinator session | Rejected | Does not change the new session's evidence |

A new session is a **database fence**, not an OS process lock. The later coordinator
must acquire a lifetime single-controller lock before beginning the session. SQLite
fencing alone cannot stop an old process from issuing an unfenced engine HTTP call.
Never start a replacement controller while the previous controller retains that lock.

This plan adds no API for deleting old-session uncertain leases. The qualified drain
or cleanup completion transaction must settle those leases together with runtime state
and resource evidence. An operator cannot clear them merely to make a capacity check pass.

Keep this primitive disconnected from the production router until the coordinator
cutover. That cutover must remove the old in-memory accounting as an independent
authority, replace profile-keyed forwarding with deployment-bound forwarding, and
use one activation path for administrative and routed requests. Do not partially wire
durable leases around an activation path that still bypasses the resource coordinator.

## 2. File map

| File | Responsibility |
|---|---|
| Modify `crates/mllm-store/src/schema.rs` | Forward-only v4 coordinator/session and request-lease tables |
| Modify `crates/mllm-store/src/migrations.rs` | Append v4 migration |
| Modify `crates/mllm-store/src/lib.rs` | Export `dispatch` module |
| Create `crates/mllm-store/src/dispatch.rs` | Session fence, atomic gate/lease operations, inspection and explicit completion |
| Create `crates/mllm-store/src/dispatch/tests.rs` | Closure race, stale callbacks, uncertain work, limits and restart tests |

### Task 1: Persist session fences and close admission on recovery

**Interfaces:** Produces `CoordinatorSession`, `DispatchError`, and
`Store::begin_coordinator_session(&self) -> Result<CoordinatorSession, DispatchError>`.
Calling this method is a recovery action, not a request retry. It closes all existing
gates and changes retained leases to uncertain without deleting any resource ownership.

**Files:** All store files in the file map except the test file, which
begins in Task 2. Migration test is appended to the existing migration test module.

- [ ] Add this migration test, then run `cargo test -p mllm-store v4_dispatch_schema`.
  Expected RED: missing coordinator table.

```rust
#[test]
fn v4_dispatch_schema() {
    let conn = Connection::open_in_memory().unwrap();
    apply(&conn).unwrap();
    apply(&conn).unwrap();
    let state: (i64, String) = conn.query_row(
        "SELECT epoch, session_id FROM coordinator_session WHERE singleton=1",
        [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(state, (0, String::new()));
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);
}
```

- [ ] Insert this schema constant before the test module in `schema.rs`:

```rust
pub const SCHEMA_V4: &str = r#"
ALTER TABLE deployments ADD COLUMN dispatch_enabled INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_enabled IN (0,1));
CREATE TABLE coordinator_session(
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    epoch INTEGER NOT NULL CHECK(epoch>=0),
    session_id TEXT NOT NULL
);
INSERT INTO coordinator_session(singleton,epoch,session_id) VALUES (1,0,'');
CREATE TABLE request_leases(
    id TEXT PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES deployments(id),
    revision INTEGER NOT NULL CHECK(revision>=1),
    generation INTEGER NOT NULL CHECK(generation>=1),
    session_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK(disposition IN ('inflight','uncertain'))
);
CREATE INDEX request_leases_deployment ON request_leases(deployment_id);
"#;
```

- [ ] Replace the migration import/list with:

```rust
use crate::schema::{SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4};
pub const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4];
```

- [ ] Export `pub mod dispatch;` from the store library and create `dispatch.rs`:

```rust
use rusqlite::{params, Transaction, TransactionBehavior};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorSession {
    epoch: i64,
    id: String,
}
impl CoordinatorSession {
    pub fn epoch(&self) -> i64 { self.epoch }
    pub fn id(&self) -> &str { &self.id }
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error("stale coordinator session")]
    StaleSession,
    #[error("deployment revision or generation changed")]
    Conflict,
    #[error("dispatch admission is closed")]
    Closed,
    #[error("outstanding work limit reached")]
    Full,
    #[error("invalid dispatch parameters or stored data")]
    Invalid,
}

impl crate::Store {
    pub fn begin_coordinator_session(&self) -> Result<CoordinatorSession, DispatchError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let old: i64 = transaction.query_row(
            "SELECT epoch FROM coordinator_session WHERE singleton=1", [], |r| r.get(0))?;
        let epoch = old.checked_add(1).filter(|e| *e > 0).ok_or(DispatchError::Invalid)?;
        let session = CoordinatorSession { epoch, id: ulid::Ulid::new().to_string() };
        transaction.execute("UPDATE coordinator_session SET epoch=?1,session_id=?2 WHERE singleton=1",
            params![session.epoch, session.id])?;
        transaction.execute("UPDATE deployments SET dispatch_enabled=0", [])?;
        transaction.execute("UPDATE request_leases SET disposition='uncertain'", [])?;
        transaction.commit()?;
        Ok(session)
    }
}
```

- [ ] Run `cargo test -p mllm-store`. Expect PASS without warnings. Stage the
  session-check helper and its imports with their first consumer in Task 2.
- [ ] Commit only these files:

```bash
git add crates/mllm-store/src/schema.rs crates/mllm-store/src/migrations.rs crates/mllm-store/src/lib.rs crates/mllm-store/src/dispatch.rs
git commit -m "feat(store): fence coordinator sessions and retain uncertain work"
```

### Task 2: Register dispatch atomically with its lifecycle gate

**Interfaces:** Produces `DispatchRequest<'a>`, `DispatchTicket`,
`Store::grant_dispatch(&self, session: &CoordinatorSession, request: DispatchRequest<'_>) -> Result<DispatchTicket, DispatchError>`,
and `Store::close_dispatch(&self, session: &CoordinatorSession, deployment: &str, revision: i64, generation: i64) -> Result<usize, DispatchError>`.

Closing returns the number of all outstanding leases for the deployment, including
uncertain and previous-generation work. It does not return a quiescence certificate.
Per-deployment and total limits count uncertain leases too. Actual queue/body byte
limits are separate and remain required in the router integration.

**Files:** `dispatch.rs` and new `src/dispatch/tests.rs`.

- [ ] Start the private unit-test file with:

```rust
use mllm_domain::{DeploymentId, LifecycleState, OperationId};
use crate::{AcceptDeployment, Store};
use super::*;

// Fixture-only readiness: production readiness must come from completion evidence.
fn ready_deployment(store: &Store, name: &str) -> String {
    let id = DeploymentId::new();
    store.accept_deployment(AcceptDeployment { id, name: name.into(), kind: "model".into(),
        route_model_id: Some(name.into()), desired_state: LifecycleState::Ready,
        schema_version: 1, idempotency_key: name.into(),
        initial_operation_id: OperationId(format!("op-{name}")),
    }).unwrap();
    store.set_observed_state(&id.to_string(), LifecycleState::Ready).unwrap();
    let gate: i64 = store.conn.query_row("SELECT dispatch_enabled FROM deployments WHERE id=?1",
        [id.to_string()], |row| row.get(0)).unwrap();
    assert_eq!(gate, 0);
    store.conn.execute("UPDATE deployments SET dispatch_enabled=1 WHERE id=?1", [id.to_string()]).unwrap();
    id.to_string()
}

fn wanted(deployment: &str) -> DispatchRequest<'_> {
    DispatchRequest { deployment_id: deployment, revision: 1, generation: 1,
        max_per_deployment: 2, max_total: 4 }
}

#[test]
fn closure_sees_registered_work_and_rejects_later_dispatch() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let ticket = store.grant_dispatch(&session, wanted(&deployment)).unwrap();
    assert_eq!(ticket.deployment_id(), deployment);
    assert_eq!(ticket.generation(), 1);
    assert_eq!(store.close_dispatch(&session, &deployment, 1, 1).unwrap(), 1);
    assert!(matches!(store.grant_dispatch(&session, wanted(&deployment)), Err(DispatchError::Closed)));
}
```

- [ ] Run `cargo test -p mllm-store dispatch::tests::closure_sees`.
  Expected RED: unresolved ticket/request types and store methods. Export the test
  module using `#[cfg(test)] mod tests;` in `dispatch.rs` before this test run.
- [ ] Append these definitions before any test modules in `dispatch.rs`:

```rust
use rusqlite::{Connection, OptionalExtension};

fn check_session(conn: &Connection, session: &CoordinatorSession) -> Result<(), DispatchError> {
    let current: (i64, String) = conn.query_row(
        "SELECT epoch,session_id FROM coordinator_session WHERE singleton=1",
        [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if session.epoch <= 0 || session.id.is_empty() || current != (session.epoch, session.id.clone()) {
        return Err(DispatchError::StaleSession);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct DispatchRequest<'a> {
    pub deployment_id: &'a str,
    pub revision: i64,
    pub generation: i64,
    pub max_per_deployment: usize,
    pub max_total: usize,
}

#[must_use = "a dispatch ticket represents retained backend work"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchTicket {
    id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    session_id: String,
}
impl DispatchTicket {
    pub fn id(&self) -> &str { &self.id }
    pub fn deployment_id(&self) -> &str { &self.deployment_id }
    pub fn revision(&self) -> i64 { self.revision }
    pub fn generation(&self) -> i64 { self.generation }
}

fn outstanding(conn: &Connection, deployment: &str) -> Result<usize, DispatchError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
        [deployment], |r| r.get(0))?;
    usize::try_from(count).map_err(|_| DispatchError::Invalid)
}

impl crate::Store {
    pub fn grant_dispatch(&self, session: &CoordinatorSession, request: DispatchRequest<'_>)
        -> Result<DispatchTicket, DispatchError> {
        if request.deployment_id.is_empty() || request.revision < 1 || request.generation < 1
            || request.max_per_deployment == 0 || request.max_total == 0 {
            return Err(DispatchError::Invalid);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&transaction, session)?;
        let row: Option<(i64, i64, String, i64, i64)> = transaction.query_row(
            "SELECT revision,current_generation,observed_state,
                CASE WHEN admission_enabled=1 AND dispatch_enabled=1 THEN 1 ELSE 0 END,suspended
             FROM deployments WHERE id=?1", [request.deployment_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
        let Some((revision, generation, state, enabled, suspended)) = row else {
            return Err(DispatchError::Conflict);
        };
        if revision != request.revision || generation != request.generation {
            return Err(DispatchError::Conflict);
        }
        if state != "ready" || enabled != 1 || suspended != 0 { return Err(DispatchError::Closed); }
        let total: i64 = transaction.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r.get(0))?;
        if outstanding(&transaction, request.deployment_id)? >= request.max_per_deployment
            || usize::try_from(total).map_err(|_| DispatchError::Invalid)? >= request.max_total {
            return Err(DispatchError::Full);
        }
        let ticket = DispatchTicket { id: ulid::Ulid::new().to_string(),
            deployment_id: request.deployment_id.into(), revision, generation, session_id: session.id.clone() };
        transaction.execute("INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition)
            VALUES (?1,?2,?3,?4,?5,'inflight')",
            params![ticket.id, ticket.deployment_id, ticket.revision, ticket.generation, ticket.session_id])?;
        transaction.commit()?;
        Ok(ticket)
    }

    pub fn close_dispatch(&self, session: &CoordinatorSession, deployment: &str,
        revision: i64, generation: i64) -> Result<usize, DispatchError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&transaction, session)?;
        let changed = transaction.execute("UPDATE deployments SET dispatch_enabled=0
            WHERE id=?1 AND revision=?2 AND current_generation=?3",
            params![deployment, revision, generation])?;
        if changed != 1 { return Err(DispatchError::Conflict); }
        let count = outstanding(&transaction, deployment)?;
        transaction.commit()?;
        Ok(count)
    }
}
```

`DispatchTicket` deliberately has no `Drop` implementation. It is a correlation
record, not an RAII release guard and not an exactly-once network-send mechanism.
The router's owned pump must perform at most one dispatch for each ticket.

- [ ] Run `cargo test -p mllm-store dispatch::tests`. Expect PASS.
- [ ] Commit only the task files:

```bash
git add crates/mllm-store/src/dispatch.rs crates/mllm-store/src/dispatch/tests.rs
git commit -m "feat(store): atomically gate and register backend work"
```

### Task 3: Retain uncertain work and settle confirmed completion explicitly

**Interfaces:** Produces `PendingDispatch`, `Store::pending_dispatches`,
`Store::mark_dispatch_uncertain`, and `Store::finish_dispatch` below.
`finish_dispatch` is restricted by call-site policy to a qualified backend terminal
event or acknowledged cancellation. A caller-controlled status value is not proof.
The function checks the ticket's original generation, not the deployment's newest
generation: legitimate old-generation work may finish after the gate closes.

**Files:** Same files as Task 2.

- [ ] Add this test and run `cargo test -p mllm-store dispatch::tests::uncertainty_survives`.
  Expected RED: new methods unresolved.

```rust
#[test]
fn uncertainty_survives_ticket_drop_and_completion_is_idempotent() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let ticket = store.grant_dispatch(&session, wanted(&deployment)).unwrap();
    let ticket_copy = ticket.clone();
    drop(ticket);
    assert_eq!(store.pending_dispatches(&deployment).unwrap().len(), 1);
    assert!(store.mark_dispatch_uncertain(&session, &ticket_copy).unwrap());
    let leases = store.pending_dispatches(&deployment).unwrap();
    assert_eq!(leases.len(), 1);
    assert!(leases[0].uncertain);
    // A completion correlated with the original ticket can settle draining work.
    store.bump_generation(&deployment).unwrap();
    assert!(store.finish_dispatch(&session, &ticket_copy).unwrap());
    assert!(!store.finish_dispatch(&session, &ticket_copy).unwrap());
    assert!(store.pending_dispatches(&deployment).unwrap().is_empty());
}
```

- [ ] Add these definitions before test modules in `dispatch.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDispatch {
    pub id: String,
    pub revision: i64,
    pub generation: i64,
    pub session_id: String,
    pub uncertain: bool,
}

fn settle_ticket(conn: &Connection, session: &CoordinatorSession, ticket: &DispatchTicket,
    complete: bool) -> Result<bool, DispatchError> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    check_session(&transaction, session)?;
    if ticket.session_id != session.id { return Err(DispatchError::StaleSession); }
    let sql = if complete {
        "DELETE FROM request_leases WHERE id=?1 AND deployment_id=?2 AND revision=?3
         AND generation=?4 AND session_id=?5"
    } else {
        "UPDATE request_leases SET disposition='uncertain' WHERE id=?1 AND deployment_id=?2
         AND revision=?3 AND generation=?4 AND session_id=?5"
    };
    let changed = transaction.execute(sql,
        params![ticket.id, ticket.deployment_id, ticket.revision, ticket.generation, ticket.session_id])?;
    transaction.commit()?;
    Ok(changed == 1)
}

impl crate::Store {
    pub fn pending_dispatches(&self, deployment: &str) -> Result<Vec<PendingDispatch>, DispatchError> {
        let mut statement = self.conn.prepare(
            "SELECT id,revision,generation,session_id,disposition FROM request_leases
             WHERE deployment_id=?1 ORDER BY id")?;
        let rows = statement.query_map([deployment], |r| {
            let disposition: String = r.get(4)?;
            Ok(PendingDispatch { id: r.get(0)?, revision: r.get(1)?, generation: r.get(2)?,
                session_id: r.get(3)?, uncertain: disposition != "inflight" })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mark_dispatch_uncertain(&self, session: &CoordinatorSession, ticket: &DispatchTicket)
        -> Result<bool, DispatchError> {
        settle_ticket(&self.conn, session, ticket, false)
    }

    pub fn finish_dispatch(&self, session: &CoordinatorSession, ticket: &DispatchTicket)
        -> Result<bool, DispatchError> {
        settle_ticket(&self.conn, session, ticket, true)
    }
}
```

- [ ] Run `cargo test -p mllm-store dispatch::tests`. Expect PASS.
- [ ] Commit the two task files:

```bash
git add crates/mllm-store/src/dispatch.rs crates/mllm-store/src/dispatch/tests.rs
git commit -m "feat(store): retain uncertain requests until confirmed completion"
```

### Task 4: Exercise restart fencing, limits, and the close-versus-dispatch race

**Interfaces:** No new production APIs.

**Files:** `src/dispatch/tests.rs`.

- [ ] Append these boundary tests. Expected PASS with the previous implementations.

```rust
#[test]
fn catalog_survives_runtime_gate_closure() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    store.close_dispatch(&session, &deployment, 1, 1).unwrap();
    assert_eq!(store.list_enabled_route_ids().unwrap(), vec!["a".to_string()]);
    assert!(store.find_deployment_by_route("a").unwrap().is_some());
}

#[test]
fn stale_or_suspended_deployments_cannot_dispatch() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let mut request = wanted(&deployment);
    request.revision = 2;
    assert!(matches!(store.grant_dispatch(&session, request), Err(DispatchError::Conflict)));
    request = wanted(&deployment); request.generation = 2;
    assert!(matches!(store.grant_dispatch(&session, request), Err(DispatchError::Conflict)));
    store.set_suspended(&deployment, true).unwrap();
    assert!(matches!(store.grant_dispatch(&session, wanted(&deployment)), Err(DispatchError::Closed)));
    assert!(store.pending_dispatches(&deployment).unwrap().is_empty());
}

#[test]
fn unknown_work_counts_against_per_deployment_and_host_limits() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let a = ready_deployment(&store, "a");
    let b = ready_deployment(&store, "b");
    let first = store.grant_dispatch(&session, wanted(&a)).unwrap();
    store.mark_dispatch_uncertain(&session, &first).unwrap();
    let second = store.grant_dispatch(&session, wanted(&a)).unwrap();
    assert!(matches!(store.grant_dispatch(&session, wanted(&a)), Err(DispatchError::Full)));
    let mut bounded = wanted(&b); bounded.max_total = 2;
    assert!(matches!(store.grant_dispatch(&session, bounded), Err(DispatchError::Full)));
    store.finish_dispatch(&session, &second).unwrap();
    let accepted = store.grant_dispatch(&session, bounded).unwrap();
    assert_eq!(accepted.deployment_id(), b);
}

#[test]
fn reopen_retains_work_and_new_session_fences_old_callbacks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite3");
    let store = Store::open(&path).unwrap();
    let old = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let ticket = store.grant_dispatch(&old, wanted(&deployment)).unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    let current = reopened.begin_coordinator_session().unwrap();
    assert!(current.epoch() > old.epoch());
    assert_ne!(current.id(), old.id());
    assert!(matches!(reopened.finish_dispatch(&old, &ticket), Err(DispatchError::StaleSession)));
    assert!(matches!(reopened.finish_dispatch(&current, &ticket), Err(DispatchError::StaleSession)));
    assert!(matches!(reopened.grant_dispatch(&current, wanted(&deployment)), Err(DispatchError::Closed)));
    let pending = reopened.pending_dispatches(&deployment).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].uncertain);
}

#[test]
fn independent_connections_serialize_close_against_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite3");
    let left_store = Store::open(&path).unwrap();
    let right_store = Store::open(&path).unwrap();
    let session = left_store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&left_store, "a");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let left_session = session.clone();
    let left_deployment = deployment.clone();
    let left_barrier = barrier.clone();
    let dispatch = std::thread::spawn(move || {
        left_barrier.wait();
        left_store.grant_dispatch(&left_session, wanted(&left_deployment))
    });
    let right_deployment = deployment.clone();
    let close = std::thread::spawn(move || {
        barrier.wait();
        right_store.close_dispatch(&session, &right_deployment, 1, 1)
    });
    let dispatched = dispatch.join().unwrap();
    let closed = close.join().unwrap();
    // Normal SQLite busy waiting should serialize these short transactions.
    // A busy error is not a successful close and must never count as quiescence.
    match (&dispatched, &closed) {
        (Ok(_), Ok(1)) | (Err(DispatchError::Closed), Ok(0)) => {}
        (Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(e, _))), Ok(0))
            if e.code == rusqlite::ErrorCode::DatabaseBusy => {}
        (Ok(_), Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(e, _))))
            if e.code == rusqlite::ErrorCode::DatabaseBusy => {}
        (Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(left, _))),
         Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(right, _))))
            if left.code == rusqlite::ErrorCode::DatabaseBusy
                && right.code == rusqlite::ErrorCode::DatabaseBusy => {}
        other => panic!("invalid race outcome: {other:?}"),
    }
    let pending = Store::open(&path).unwrap().pending_dispatches(&deployment).unwrap();
    assert_eq!(pending.len(), usize::from(dispatched.is_ok()));
}
```

- [ ] Run `cargo test -p mllm-store dispatch::tests` and repeat the race
  test 20 times. Every invocation must pass; inspect any failure before rerunning.

```bash
for attempt in {1..20}; do
  cargo test -p mllm-store dispatch::tests::independent_connections_serialize_close_against_dispatch || exit 1
done
```

- [ ] Run `cargo test -p mllm-store` and
  `cargo clippy -p mllm-store --all-targets -- -D warnings`.
- [ ] Run `git diff --check`. Confirm only named store files changed and no production
  database was opened. These CPU tests do not qualify a live router or engine.
- [ ] Commit only the test file:

```bash
git add crates/mllm-store/src/dispatch/tests.rs
git commit -m "test(store): prove durable dispatch fencing and closure ordering"
```

## 3. Coordinator/router cutover contract

The coordinator integration plan must wire these exact boundaries, not add another
activation or accounting path:

1. Acquire single-controller lifetime ownership, begin the session, reconcile existing
   processes/reservations/leases, then publish availability. Beginning a session alone
   cannot mark any existing deployment ready.
2. Authenticate and bound queue/body memory before waiting for activation. Resolve
   the route to a deployment and immutable revision; missing rows are errors, never
   “already ready.” Administrative start and routed wake join the same durable operation.
3. Once readiness is established, call `grant_dispatch` with the current session and
   exact generation. Select the forwarder by that deployment binding, not by profile
   kind. The binding cannot be replaced until its old-generation leases are settled.
4. Transfer the ticket to an owned backend pump before returning a streaming response.
   If the handler is cancelled before that transfer, retain/mark the lease uncertain
   unless the router can establish that no backend submission occurred.
5. Call `finish_dispatch` only on a qualified completed backend response or confirmed
   cancellation. `StreamEnded::BackendClosed` and adapter errors call
   `mark_dispatch_uncertain`; no unconditional release at the bottom of the pump.
6. Preserve streamed chunks or fail the stream explicitly. The current bounded-channel
   `try_send` path must not silently discard chunks and then emit `[DONE]`. A downstream
   delivery failure and confirmed backend completion are separate facts: delivery can
   fail while backend work is safely settled.
7. Non-streaming response assembly must establish the same backend terminal contract.
   The current vLLM non-streaming adapter ignores `StreamEnd`; an `Ok(JSON)` synthesized
   from an incomplete stream cannot authorize lease completion or a successful response.
8. Before parking, close dispatch transactionally. A zero lease count only describes
   mllm's tracked work; pair it with the qualified engine barrier and private-ingress
   ownership. Unknown work requires qualified reconciliation, never a deadline-based
   lease sweep. Resolve old-session leases only within that completion transaction.

These integration steps are requirements for the remaining executable coordinator
plan, not instructions to patch the router partially in this task set. Full F2 planning
still includes lifecycle-step persistence, runtime bindings, resource/evidence commits,
preinitialization, management/API/CLI, SGLang, and mixed-engine qualification.
