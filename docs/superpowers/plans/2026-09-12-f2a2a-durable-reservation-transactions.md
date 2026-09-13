# F2A2a Durable Reservation Transactions Implementation Plan

**Goal:** Persist resource-increasing reservations with atomic ledger, deployment-revision, and generation checks, without granting permission to execute an engine action.

**Architecture:** Extend the existing SQLite store with a versioned resource ledger and durable grant receipts. Evaluate admission inside an immediate write transaction; commit the owner replacement, epoch increment, and receipt together. Keep this storage primitive disconnected from production lifecycle execution until the runtime-coordination integration is complete.

**Tech Stack:** Existing Rust 2021 workspace, rusqlite 0.37, serde/serde_json, thiserror, tempfile, and the F2A1 scheduler. No engine dependency or hardware access.

**Spec:** [Approved F2 design](../../design/milestones/f2-sglang-design.md), §§3–6, Q2/Q6/Q8. Dependency: [F2A1 resource kernel](2026-09-12-f2a1-resource-contracts-and-admission.md).

## Global Constraints

- “Only host-a is authorized for subsequent live work.” This plan contains no live work.
- “Reservation decisions and competing transition claims are atomic across deployments.”
- “A low usage sample does not authorize shrinking a qualified peak reservation.”
- “Failed states, timeouts, controller loss, and expired leases do not free reservations or ports.”
- “No blind repetition of a possibly applied engine operation is allowed.”
- “Material configuration changes require an explicit revision-aware operation.”
- Preserve `crates/mllm-cli/tests/live_interactive.rs`; do not edit, stage, or run it.
- Execute tests against in-memory stores and temporary directories only. Do not open a production database with new migrations.

---

## Scope and integration boundary

This is the persistence part of F2A2, not its complete runtime coordinator. Separating
it permits independent transaction and crash-boundary tests. F2A2 still requires host
observations, release evidence, process/endpoint ownership, lifecycle arbitration,
routing, and reconciliation. F2A3, F2B, and F2C remain required.

The new writer only adds or increases reservations. It cannot drop an owner, reduce
memory, release a device claim, mark a deployment ready, or authorize a launch.
Retained peak reservations survive restart. A receipt recovered after an uncertain
result means **recorded**, not “execute again.”

The existing reservation writer remains for F1 callers until the coordinated cutover.
The new writer refuses a database containing legacy reservation rows. Do not copy
those rows into the new ledger heuristically, clear them, or run both authorities.
The integration plan must provide an explicit, reconciled migration of live owners
and remove the old writers before enabling production use. Empty legacy rows alone
are not evidence that no runtime exists.

This primitive trusts the coordinator to supply reconciled observations, effective
limits, and a policy-authorized phase. It enforces freshness and numerical admission,
but cannot verify that caller-provided samples describe real hardware. The coordinator
must hold lifecycle ownership and revalidate observations before physical effects.
Grant IDs identify individual transition steps, not whole multi-step operations.

## File map

| File | Responsibility |
|---|---|
| Modify `crates/mllm-store/src/schema.rs` | Add forward-only v3 schema; leave v1/v2 unchanged |
| Modify `crates/mllm-store/src/migrations.rs` | Append v3 to migration list |
| Modify `crates/mllm-store/src/lib.rs` | Export `resource_ledger` module |
| Modify `crates/mllm-store/Cargo.toml` | Add existing workspace scheduler dependency |
| Create `crates/mllm-store/src/resource_ledger.rs` | Private storage format, coherent snapshot reads, atomic increasing grants |
| Create `crates/mllm-store/tests/resource_transactions.rs` | Public API, independent-connection contention, replay and restart tests |

The store already depends on the domain crate. The scheduler does not depend on the
store; adding `mllm-scheduler` here does not introduce a dependency cycle. Resource
JSON is a private storage format, not the F2A3 management schema.

### Task 1: Migrate the durable resource ledger without changing legacy evidence

**Interfaces:** Adds schema version 3, deployment `revision` defaulting to 1,
`resource_ledger_meta`, `resource_owners`, and `resource_grants`.

**Files:** `schema.rs` and `migrations.rs` from the file map.

- [ ] Add this test to the existing `migrations.rs` test module.

```rust
#[test]
fn v3_upgrade_preserves_legacy_rows_and_sets_revision() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_V1).unwrap();
    conn.execute_batch(crate::schema::SCHEMA_V2).unwrap();
    conn.execute_batch("INSERT INTO schema_migrations VALUES (1), (2);
        INSERT INTO deployments(id, name, kind, desired_state, admission_enabled,
          suspended, current_generation, schema_version)
        VALUES ('a', 'a', 'model', 'stopped', 1, 0, 1, 1);
        INSERT INTO owners(id, kind, deployment_id) VALUES ('a', 'model', 'a');
        INSERT INTO reservations(owner_id, domain_id, bytes, phase)
        VALUES ('a', 'system', 64, 'activation');").unwrap();
    apply(&conn).unwrap();
    apply(&conn).unwrap();
    let revision: i64 = conn.query_row(
        "SELECT revision FROM deployments WHERE id='a'", [], |r| r.get(0)).unwrap();
    let bytes: i64 = conn.query_row(
        "SELECT bytes FROM reservations WHERE owner_id='a'", [], |r| r.get(0)).unwrap();
    let epoch: i64 = conn.query_row(
        "SELECT epoch FROM resource_ledger_meta WHERE singleton=1", [], |r| r.get(0)).unwrap();
    assert_eq!((revision, bytes, epoch), (1, 64, 0));
}
```

- [ ] Run `cargo test -p mllm-store v3_upgrade_preserves_legacy_rows_and_sets_revision`.
  Expected RED: missing `revision` column.
- [ ] Insert this constant in `schema.rs` before its `#[cfg(test)]` module.

```rust
pub const SCHEMA_V3: &str = r#"
ALTER TABLE deployments ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1);
CREATE TABLE resource_ledger_meta(
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    epoch INTEGER NOT NULL CHECK(epoch >= 0)
);
INSERT INTO resource_ledger_meta(singleton, epoch) VALUES (1, 0);
CREATE TABLE resource_owners(
    owner_id TEXT PRIMARY KEY REFERENCES deployments(id),
    footprint_json TEXT NOT NULL
);
CREATE TABLE resource_grants(
    id TEXT PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES deployments(id),
    operation_id TEXT NOT NULL REFERENCES operations(id),
    request_json TEXT NOT NULL,
    committed_epoch INTEGER NOT NULL UNIQUE CHECK(committed_epoch > 0)
);
"#;
```

- [ ] Replace the migration import and list with:

```rust
use crate::schema::{SCHEMA_V1, SCHEMA_V2, SCHEMA_V3};
pub const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2, SCHEMA_V3];
```

- [ ] Run `cargo test -p mllm-store`. Expect PASS, including unchanged v1 schema tests.
- [ ] Commit only these files:

```bash
git add crates/mllm-store/src/schema.rs crates/mllm-store/src/migrations.rs
git commit -m "feat(store): add versioned resource ledger schema"
```

### Task 2: Read consistent snapshots through a validated private storage format

**Interfaces:** Produces `ResourceStoreError` and
`Store::resource_snapshot(&self) -> Result<LedgerSnapshot, ResourceStoreError>`.
Consumes F2A1 `PhaseFootprint`, `LedgerSnapshot`, and `validate_footprint`.
Private `decode` preserves all allocations and sharing claims; Task 3 adds canonical
encoding when the writer needs it. Unknown storage
versions, phases, and malformed footprints fail closed.

**Files:** Create `resource_ledger.rs` and `tests/resource_transactions.rs`; modify
the store manifest and `lib.rs`.

- [ ] Start `resource_transactions.rs` with this test, then run
  `cargo test -p mllm-store --test resource_transactions new_store_has_empty_epoch_zero`.
  Expected RED: `resource_snapshot` is unresolved.

```rust
use mllm_store::Store;

#[test]
fn new_store_has_empty_epoch_zero() {
    let store = Store::open_in_memory().unwrap();
    let snapshot = store.resource_snapshot().unwrap();
    assert_eq!(snapshot.epoch, 0);
    assert!(snapshot.owners.is_empty());
}
```

- [ ] Add `mllm-scheduler = { workspace = true }` to the store's `[dependencies]`.
  Export `pub mod resource_ledger;` in `lib.rs`.
- [ ] Create the module with this implementation. The read transaction prevents an
  epoch from one committed state being paired with owner rows from another.

```rust
use mllm_domain::resources::*;
use mllm_scheduler::residency::{validate_footprint, ResourceError};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ResourceStoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Admission(#[from] ResourceError),
    #[error("resource precondition conflict")]
    Conflict,
    #[error("legacy reservations require reconciled migration")]
    NeedsReconciliation,
    #[error("invalid durable resource data")]
    Invalid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFootprint {
    version: u32,
    phase: String,
    allocations: Vec<(String, i64, i64)>,
    devices: Vec<(String, bool)>,
}

fn decode(json: &str) -> Result<PhaseFootprint, ResourceStoreError> {
    let stored: StoredFootprint = serde_json::from_str(json)?;
    if stored.version != 1 { return Err(ResourceStoreError::Invalid); }
    let phase = match stored.phase.as_str() {
        "cold" => ResourcePhase::Cold,
        "ready" => ResourcePhase::Ready,
        "parking" => ResourcePhase::Parking,
        "parked" => ResourcePhase::Parked,
        "wake" => ResourcePhase::Wake,
        _ => return Err(ResourceStoreError::Invalid),
    };
    let footprint = PhaseFootprint {
        phase,
        allocations: stored.allocations.into_iter().map(|(domain, bytes, host_kv_bytes)|
            Allocation { domain, bytes, host_kv_bytes }).collect(),
        devices: stored.devices.into_iter().map(|(device, shared)| DeviceClaim {
            device, sharing: if shared { Sharing::Shared } else { Sharing::Exclusive },
        }).collect(),
    };
    validate_footprint(&footprint)?;
    Ok(footprint)
}

fn read_snapshot(conn: &Connection) -> Result<LedgerSnapshot, ResourceStoreError> {
    let legacy: i64 = conn.query_row("SELECT COUNT(*) FROM reservations", [], |r| r.get(0))?;
    if legacy != 0 { return Err(ResourceStoreError::NeedsReconciliation); }
    let epoch: i64 = conn.query_row(
        "SELECT epoch FROM resource_ledger_meta WHERE singleton=1", [], |r| r.get(0))?;
    let mut snapshot = LedgerSnapshot {
        epoch: u64::try_from(epoch).map_err(|_| ResourceStoreError::Invalid)?,
        owners: Default::default(),
    };
    let mut statement = conn.prepare(
        "SELECT owner_id, footprint_json FROM resource_owners ORDER BY owner_id")?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (owner, json) = row?;
        if owner.is_empty() { return Err(ResourceStoreError::Invalid); }
        snapshot.owners.insert(owner, decode(&json)?);
    }
    Ok(snapshot)
}

impl crate::Store {
    pub fn resource_snapshot(&self) -> Result<LedgerSnapshot, ResourceStoreError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let snapshot = read_snapshot(&transaction)?;
        transaction.commit()?;
        Ok(snapshot)
    }
}
```

- [ ] Add these private decoder tests at the bottom of the module. Task 3 adds
  the encoder and round-trip test together, avoiding unused production helpers.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_footprints_decode_and_fail_closed() {
        let footprint = PhaseFootprint { phase: ResourcePhase::Ready,
            allocations: vec![Allocation { domain: "system".into(), bytes: 64, host_kv_bytes: 8 }],
            devices: vec![DeviceClaim { device: "gpu0".into(), sharing: Sharing::Shared }] };
        let json = r#"{"version":1,"phase":"ready","allocations":[["system",64,8]],"devices":[["gpu0",true]]}"#;
        assert_eq!(decode(json).unwrap(), footprint);
        assert!(decode(&json.replace("\"version\":1", "\"version\":2")).is_err());
        assert!(decode(&json.replace("\"ready\"", "\"unknown\"")).is_err());
        assert!(decode(&json.replace(",64,8", ",-1,8")).is_err());
        assert!(decode(&json.replace("\"ready\"", "\"parked\"")).is_err());
    }
}
```

- [ ] Run `cargo test -p mllm-store`. Expect PASS with warning-free code.
- [ ] Commit only the four task files:

```bash
git add crates/mllm-store/src/resource_ledger.rs crates/mllm-store/src/lib.rs crates/mllm-store/Cargo.toml crates/mllm-store/tests/resource_transactions.rs
git commit -m "feat(store): read validated resource ledger snapshots"
```

### Task 3: Commit increasing reservations and receipts in one transaction

**Interfaces:** Produces `GrantRequest`, `GrantReceipt`, and
`Store::reserve_increase(&self, request: &GrantRequest, context: AdmissionContext<'_>) -> Result<GrantReceipt, ResourceStoreError>`.

`New` means the database transaction committed. It is not a dispatch token.
`Recorded` is an exact retry and must lead to observation/reconciliation, not replay.
Request identity includes deployment, operation, revision, generation, expected epoch,
and canonical footprint. Changing content under the same grant ID is a conflict.

**Files:** `resource_ledger.rs` and `tests/resource_transactions.rs`.

- [ ] Add the following helpers and test to the integration test file.

```rust
use mllm_domain::{DeploymentId, LifecycleState, OperationId};
use mllm_domain::resources::*;
use mllm_scheduler::residency::{AdmissionContext, ResourceError};
use mllm_store::AcceptDeployment;
use mllm_store::resource_ledger::{GrantReceipt, GrantRequest, ResourceStoreError};

fn deployment(store: &Store, name: &str) -> String {
    let id = DeploymentId::new();
    store.accept_deployment(AcceptDeployment {
        id, name: name.into(), kind: "model".into(), route_model_id: None,
        desired_state: LifecycleState::Stopped, schema_version: 1,
        idempotency_key: name.into(), initial_operation_id: OperationId(format!("op-{name}")),
    }).unwrap();
    id.to_string()
}

fn request(deployment: &str, name: &str, bytes: i64) -> GrantRequest {
    GrantRequest { id: format!("grant-{name}"), deployment_id: deployment.into(),
        operation_id: format!("op-{name}"), revision: 1, generation: 1, expected_epoch: 0,
        next: PhaseFootprint { phase: ResourcePhase::Cold,
            allocations: vec![Allocation { domain: "system".into(), bytes, host_kv_bytes: 0 }],
            devices: vec![] } }
}

fn observations() -> [MemoryObservation; 1] {
    [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 128, sampled_at_ms: 100 }]
}
fn limits() -> [MemoryLimit; 1] {
    [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }]
}

#[test]
fn grants_are_atomic_and_retries_are_not_dispatch_authority() {
    let store = Store::open_in_memory().unwrap();
    let a = deployment(&store, "a");
    let b = deployment(&store, "b");
    let obs = observations();
    let bounds = limits();
    let context = AdmissionContext::new(&obs, &bounds, 101, 60, 4);
    let first = request(&a, "a", 60);
    assert_eq!(store.reserve_increase(&first, context).unwrap(), GrantReceipt::New { epoch: 1 });
    assert_eq!(store.reserve_increase(&first, context).unwrap(), GrantReceipt::Recorded { epoch: 1 });
    let mut changed = first.clone();
    changed.next.allocations[0].bytes = 61;
    assert!(matches!(store.reserve_increase(&changed, context), Err(ResourceStoreError::Conflict)));
    let mut second = request(&b, "b", 60);
    assert!(matches!(store.reserve_increase(&second, context), Err(ResourceStoreError::Conflict)));
    second.expected_epoch = 1;
    assert!(matches!(store.reserve_increase(&second, context),
        Err(ResourceStoreError::Admission(ResourceError::Insufficient))));
    let snapshot = store.resource_snapshot().unwrap();
    assert_eq!(snapshot.epoch, 1);
    assert_eq!(snapshot.owners.len(), 1);
    assert_eq!(snapshot.owners[&a], first.next);
}
```

- [ ] Run `cargo test -p mllm-store --test resource_transactions grants_are_atomic`.
  Expected RED: new grant types/method unresolved.
- [ ] Insert this implementation before the test modules in `resource_ledger.rs`.

```rust
use mllm_scheduler::residency::{admit_phase, AdmissionContext};
use rusqlite::{params, OptionalExtension};

fn encode(footprint: &PhaseFootprint) -> Result<String, ResourceStoreError> {
    validate_footprint(footprint)?;
    let phase = match footprint.phase {
        ResourcePhase::Cold => "cold",
        ResourcePhase::Ready => "ready",
        ResourcePhase::Parking => "parking",
        ResourcePhase::Parked => "parked",
        ResourcePhase::Wake => "wake",
    };
    let mut allocations: Vec<_> = footprint.allocations.iter()
        .map(|a| (a.domain.clone(), a.bytes, a.host_kv_bytes)).collect();
    let mut devices: Vec<_> = footprint.devices.iter()
        .map(|d| (d.device.clone(), d.sharing == Sharing::Shared)).collect();
    allocations.sort();
    devices.sort();
    Ok(serde_json::to_string(&StoredFootprint {
        version: 1, phase: phase.into(), allocations, devices,
    })?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRequest {
    pub id: String,
    pub deployment_id: String,
    pub operation_id: String,
    pub revision: i64,
    pub generation: i64,
    pub expected_epoch: u64,
    pub next: PhaseFootprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantReceipt {
    New { epoch: u64 },
    Recorded { epoch: u64 },
}

fn ensure_increasing(old: Option<&PhaseFootprint>, next: &PhaseFootprint)
    -> Result<(), ResourceStoreError> {
    if !matches!(next.phase, ResourcePhase::Cold | ResourcePhase::Parking | ResourcePhase::Wake) {
        return Err(ResourceStoreError::Conflict);
    }
    match old {
        None if next.phase != ResourcePhase::Cold => return Err(ResourceStoreError::Conflict),
        Some(old) => {
            if !matches!((old.phase, next.phase),
                (ResourcePhase::Ready, ResourcePhase::Parking) |
                (ResourcePhase::Parked, ResourcePhase::Wake)) {
                return Err(ResourceStoreError::Conflict);
            }
            for previous in &old.allocations {
                let current = next.allocations.iter().find(|a| a.domain == previous.domain)
                    .ok_or(ResourceStoreError::Conflict)?;
                if current.bytes < previous.bytes || current.host_kv_bytes < previous.host_kv_bytes {
                    return Err(ResourceStoreError::Conflict);
                }
            }
            for claim in &old.devices {
                if !next.devices.contains(claim) { return Err(ResourceStoreError::Conflict); }
            }
        }
        None => {}
    }
    Ok(())
}

impl crate::Store {
    pub fn reserve_increase(&self, request: &GrantRequest, context: AdmissionContext<'_>)
        -> Result<GrantReceipt, ResourceStoreError> {
        if request.id.is_empty() || request.deployment_id.is_empty()
            || request.operation_id.is_empty() || request.revision < 1 || request.generation < 1 {
            return Err(ResourceStoreError::Invalid);
        }
        let encoded = encode(&request.next)?;
        let identity = serde_json::to_string(&(
            &request.deployment_id, &request.operation_id, request.revision,
            request.generation, request.expected_epoch, &encoded,
        ))?;
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let prior: Option<(String, i64)> = transaction.query_row(
            "SELECT request_json, committed_epoch FROM resource_grants WHERE id=?1",
            [&request.id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((previous, epoch)) = prior {
            if previous != identity { return Err(ResourceStoreError::Conflict); }
            let epoch = u64::try_from(epoch).map_err(|_| ResourceStoreError::Invalid)?;
            transaction.commit()?;
            return Ok(GrantReceipt::Recorded { epoch });
        }
        let state: Option<(i64, i64, String, String)> = transaction.query_row(
            "SELECT d.revision, d.current_generation, d.kind, o.state
             FROM deployments d JOIN operations o ON o.deployment_id=d.id
             WHERE d.id=?1 AND o.id=?2",
            params![request.deployment_id, request.operation_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        let Some((revision, generation, kind, operation_state)) = state else {
            return Err(ResourceStoreError::Conflict);
        };
        if revision != request.revision || generation != request.generation || kind != "model"
            || !matches!(operation_state.as_str(), "pending" | "running") {
            return Err(ResourceStoreError::Conflict);
        }
        let snapshot = read_snapshot(&transaction)?;
        if snapshot.epoch != request.expected_epoch { return Err(ResourceStoreError::Conflict); }
        ensure_increasing(snapshot.owners.get(&request.deployment_id), &request.next)?;
        admit_phase(&snapshot, &request.deployment_id, &request.next, context)?;
        let epoch = i64::try_from(snapshot.epoch).ok().and_then(|e| e.checked_add(1))
            .ok_or(ResourceStoreError::Invalid)?;
        transaction.execute(
            "INSERT INTO resource_owners(owner_id, footprint_json) VALUES (?1, ?2)
             ON CONFLICT(owner_id) DO UPDATE SET footprint_json=excluded.footprint_json",
            params![request.deployment_id, encoded])?;
        transaction.execute("UPDATE resource_ledger_meta SET epoch=?1 WHERE singleton=1", [epoch])?;
        transaction.execute(
            "INSERT INTO resource_grants(id, deployment_id, operation_id, request_json, committed_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![request.id, request.deployment_id, request.operation_id, identity, epoch])?;
        transaction.commit()?;
        Ok(GrantReceipt::New { epoch: epoch as u64 })
    }
}
```

- [ ] Run `cargo test -p mllm-store`. Expect PASS. Do not add automatic retries around
  `SQLITE_BUSY`: the coordinator must retry within its deadline using fresh state.
- [ ] Add a private encoder round-trip regression alongside Task 2's decoder cases:

```rust
#[test]
fn canonical_encoding_roundtrips_all_claims() {
    let footprint = PhaseFootprint { phase: ResourcePhase::Ready, allocations: vec![
        Allocation { domain: "gpu-memory:0".into(), bytes: 64, host_kv_bytes: 0 },
        Allocation { domain: "system".into(), bytes: 32, host_kv_bytes: 8 },
    ], devices: vec![
        DeviceClaim { device: "gpu0".into(), sharing: Sharing::Shared },
        DeviceClaim { device: "gpu1".into(), sharing: Sharing::Exclusive },
    ] };
    let encoded = encode(&footprint).unwrap();
    assert_eq!(decode(&encoded).unwrap(), footprint);
    let mut reordered = footprint;
    reordered.allocations.reverse();
    reordered.devices.reverse();
    assert_eq!(encode(&reordered).unwrap(), encoded);
}
```

  Compare canonical sorted allocations/claims and every byte/KV field. Reversing
  input vector order must produce identical encoded bytes.
- [ ] Commit only the task files:

```bash
git add crates/mllm-store/src/resource_ledger.rs crates/mllm-store/tests/resource_transactions.rs
git commit -m "feat(store): atomically reserve increasing resource phases"
```

### Task 4: Prove fencing, rollback, replay, and independent-connection contention

**Interfaces:** No new production interface. Tests exercise the existing public API
and inject database failures in a private unit-test module.

**Files:** Both files from Task 3.

- [ ] Append these integration tests. Run
  `cargo test -p mllm-store --test resource_transactions`.
  Expected PASS with Task 3; these are boundary regressions, not claimed RED/GREEN tests.

```rust
#[test]
fn stale_revision_generation_and_observation_leave_no_grant() {
    let store = Store::open_in_memory().unwrap();
    let a = deployment(&store, "a");
    let obs = observations();
    let bounds = limits();
    let context = AdmissionContext::new(&obs, &bounds, 101, 60, 4);
    let valid = request(&a, "a", 60);
    let mut stale = valid.clone();
    stale.revision = 2;
    assert!(matches!(store.reserve_increase(&stale, context), Err(ResourceStoreError::Conflict)));
    store.bump_generation(&a).unwrap();
    assert!(matches!(store.reserve_increase(&valid, context), Err(ResourceStoreError::Conflict)));
    stale = valid;
    stale.generation = 2;
    assert!(matches!(store.reserve_increase(&stale,
        AdmissionContext::new(&obs, &bounds, 161, 60, 4)),
        Err(ResourceStoreError::Admission(ResourceError::StaleObservation))));
    assert_eq!(store.resource_snapshot().unwrap().epoch, 0);
    assert_eq!(store.reserve_increase(&stale, context).unwrap(), GrantReceipt::New { epoch: 1 });
}

#[test]
fn committed_reservation_survives_reopen_without_authorizing_replay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite3");
    let store = Store::open(&path).unwrap();
    let a = deployment(&store, "a");
    let grant = request(&a, "a", 60);
    let obs = observations();
    let bounds = limits();
    store.reserve_increase(&grant, AdmissionContext::new(&obs, &bounds, 101, 60, 4)).unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.resource_snapshot().unwrap().owners[&a], grant.next);
    // Expired observations cannot authorize new work. Historical receipt lookup remains valid.
    assert_eq!(reopened.reserve_increase(&grant,
        AdmissionContext::new(&obs, &bounds, 10_000, 60, 4)).unwrap(),
        GrantReceipt::Recorded { epoch: 1 });
}

#[test]
fn independent_connections_cannot_spend_one_epoch_twice() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite3");
    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    let a = deployment(&first, "a");
    let b = deployment(&first, "b");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let launch = |store: Store, grant: GrantRequest, barrier: std::sync::Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            let obs = observations();
            let bounds = limits();
            barrier.wait();
            store.reserve_increase(&grant, AdmissionContext::new(&obs, &bounds, 101, 60, 4))
        })
    };
    let left = launch(first, request(&a, "a", 60), barrier.clone());
    let right = launch(second, request(&b, "b", 60), barrier);
    let outcomes = [left.join().unwrap(), right.join().unwrap()];
    assert_eq!(outcomes.iter().filter(|r| matches!(r, Ok(GrantReceipt::New { epoch: 1 }))).count(), 1);
    for outcome in &outcomes {
        match outcome {
            Ok(GrantReceipt::New { epoch: 1 }) | Err(ResourceStoreError::Conflict) => {}
            Err(ResourceStoreError::Sql(rusqlite::Error::SqliteFailure(error, _)))
                if error.code == rusqlite::ErrorCode::DatabaseBusy => {}
            other => panic!("unexpected race result: {other:?}"),
        }
    }
    let recovered = Store::open(&path).unwrap().resource_snapshot().unwrap();
    assert_eq!(recovered.epoch, 1);
    assert_eq!(recovered.owners.len(), 1);
}
```

- [ ] Add this separate private test module to `resource_ledger.rs`. Direct SQL is
  test-only fault injection, not an alternate production reservation writer.

```rust
#[cfg(test)]
mod transaction_fault_tests {
    use super::*;

    fn fixture() -> (crate::Store, GrantRequest) {
        let store = crate::Store::open_in_memory().unwrap();
        store.conn.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,
          admission_enabled,suspended,current_generation,schema_version)
          VALUES ('a','a','model','stopped',1,0,1,1);
          INSERT INTO operations(id,deployment_id,kind,state) VALUES ('op-a','a','start','running');").unwrap();
        let request = GrantRequest { id: "grant-a".into(), deployment_id: "a".into(),
            operation_id: "op-a".into(), revision: 1, generation: 1, expected_epoch: 0,
            next: PhaseFootprint { phase: ResourcePhase::Cold,
                allocations: vec![Allocation { domain: "system".into(), bytes: 60, host_kv_bytes: 0 }],
                devices: vec![] } };
        (store, request)
    }

    #[test]
    fn receipt_failure_rolls_back_owner_and_epoch() {
        let (store, request) = fixture();
        store.conn.execute_batch("CREATE TRIGGER reject_grant BEFORE INSERT ON resource_grants
            BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;").unwrap();
        let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
        let context = AdmissionContext::new(&obs, &limits, 101, 60, 4);
        assert!(matches!(store.reserve_increase(&request, context), Err(ResourceStoreError::Sql(_))));
        assert_eq!(store.resource_snapshot().unwrap(), LedgerSnapshot { epoch: 0, owners: Default::default() });
        store.conn.execute_batch("DROP TRIGGER reject_grant;").unwrap();
        assert_eq!(store.reserve_increase(&request, context).unwrap(), GrantReceipt::New { epoch: 1 });
    }

    #[test]
    fn legacy_reservations_are_not_silently_ignored() {
        let (store, _) = fixture();
        store.conn.execute_batch("INSERT INTO owners(id,kind,deployment_id) VALUES ('a','model','a');
            INSERT INTO reservations(owner_id,domain_id,bytes,phase) VALUES ('a','system',60,'activation');").unwrap();
        assert!(matches!(store.resource_snapshot(), Err(ResourceStoreError::NeedsReconciliation)));
    }

    #[test]
    fn increasing_writer_cannot_release_memory_or_claims() {
        let (_, request) = fixture();
        let mut old = request.next;
        old.phase = ResourcePhase::Ready;
        old.devices.push(DeviceClaim { device: "gpu0".into(), sharing: Sharing::Shared });
        let mut next = old.clone();
        next.phase = ResourcePhase::Parking;
        assert!(ensure_increasing(Some(&old), &next).is_ok());
        next.allocations[0].bytes = 59;
        assert!(ensure_increasing(Some(&old), &next).is_err());
        next.allocations[0].bytes = 60;
        next.devices.clear();
        assert!(ensure_increasing(Some(&old), &next).is_err());
        next.phase = ResourcePhase::Parked;
        assert!(ensure_increasing(Some(&old), &next).is_err());
    }

    #[test]
    fn changed_revision_terminal_operation_and_corrupt_ledger_block_grants() {
        let (store, mut request) = fixture();
        let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
        let context = AdmissionContext::new(&obs, &limits, 101, 60, 4);
        store.conn.execute("UPDATE deployments SET revision=2 WHERE id='a'", []).unwrap();
        assert!(matches!(store.reserve_increase(&request, context), Err(ResourceStoreError::Conflict)));
        request.revision = 2;
        store.conn.execute("UPDATE operations SET state='failed' WHERE id='op-a'", []).unwrap();
        assert!(matches!(store.reserve_increase(&request, context), Err(ResourceStoreError::Conflict)));
        store.conn.execute("UPDATE operations SET state='running' WHERE id='op-a'", []).unwrap();
        store.conn.execute("INSERT INTO resource_owners(owner_id,footprint_json) VALUES ('a','{}')", []).unwrap();
        assert!(matches!(store.reserve_increase(&request, context), Err(ResourceStoreError::Json(_))));
        let epoch: i64 = store.conn.query_row(
            "SELECT epoch FROM resource_ledger_meta WHERE singleton=1", [], |r| r.get(0)).unwrap();
        let receipts: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM resource_grants", [], |r| r.get(0)).unwrap();
        assert_eq!((epoch, receipts), (0, 0));
    }
}
```

- [ ] Run `cargo test -p mllm-store` and repeat the independent-connection test 20 times:

```bash
for attempt in {1..20}; do
  cargo test -p mllm-store --test resource_transactions independent_connections_cannot_spend_one_epoch_twice || exit 1
done
```

  All 20 invocations must pass. Any failed invocation fails the gate; a subsequent
  pass does not erase it. Thread order may vary; exactly one reservation commits.
- [ ] Run `cargo test --workspace --exclude mllm-cli`,
  `cargo test -p mllm-cli --lib`, and
  `cargo clippy --workspace --exclude mllm-cli --all-targets -- -D warnings`, and
  `cargo clippy -p mllm-cli --lib --bin mllm -- -D warnings`.
  The explicit CLI exclusion avoids executing the unrelated live integration file;
  Clippy excludes that target too. Run other named CLI CPU-only integration
  targets only after inspecting their current gates. No live target is authorized here.
- [ ] Run `git diff --check`. Review changed files and confirm no controller, launcher,
  router, credentials, or production configuration changed.
- [ ] Commit the task files only:

```bash
git add crates/mllm-store/src/resource_ledger.rs crates/mllm-store/tests/resource_transactions.rs
git commit -m "test(store): prove resource grant fencing and rollback"
```

## Completion and remaining work

This plan covers durable increasing-reservation transaction mechanics within Q2/Q6
and storage portions of Q8. It does not close those gates by itself.

The next F2A2 plan must supply all of the following before this API controls hardware:

The [F2A2b observation and completion-evidence plan](2026-09-12-f2a2b-runtime-evidence.md)
now specifies the initial observer and validation interfaces. It does not replace
the durable coordinator integration requirements below.

- Reconciled host observations and owner usage, including external and attached services.
- Pinned deployment resource contracts and policy-aware phase selection. The new
  `revision` column is a fence, not a complete configuration-update implementation.
- Exclusive lifecycle ownership, operation-step state, and process/worker start identities.
- Durable endpoint/credential ownership and safe launch argument rendering.
- Verified release evidence bound to the operation, generation, process identities,
  and qualified recipe before reducing reservations or removing owners.
- Atomic cutover from legacy writers with conservative treatment of unknown owners.
- Ready/parked completion transactions, crash reconciliation, and no-replay dispatch.
- Routing, queues, safe drain, preparation, and administrative-start coordination.

The observation and release protocols must be selected from repository evidence and
pinned runtime sources before drafting their executable steps. No receipt produced
by this plan substitutes for that evidence. Receipt retention/compaction must preserve
retry identity; this plan does not add an unsafe automatic history-deletion policy.
