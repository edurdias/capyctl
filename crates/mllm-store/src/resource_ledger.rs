use mllm_domain::resources::*;
use mllm_scheduler::residency::{admit_phase, validate_footprint, AdmissionContext, ResourceError};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
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
    if stored.version != 1 {
        return Err(ResourceStoreError::Invalid);
    }
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
        allocations: stored
            .allocations
            .into_iter()
            .map(|(domain, bytes, host_kv_bytes)| Allocation {
                domain,
                bytes,
                host_kv_bytes,
            })
            .collect(),
        devices: stored
            .devices
            .into_iter()
            .map(|(device, shared)| DeviceClaim {
                device,
                sharing: if shared {
                    Sharing::Shared
                } else {
                    Sharing::Exclusive
                },
            })
            .collect(),
    };
    validate_footprint(&footprint)?;
    Ok(footprint)
}

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
        None if next.phase != ResourcePhase::Cold => Err(ResourceStoreError::Conflict),
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
            Ok(())
        }
        None => Ok(()),
    }
}

pub(crate) fn read_snapshot(conn: &Connection) -> Result<LedgerSnapshot, ResourceStoreError> {
    let legacy: i64 = conn.query_row("SELECT COUNT(*) FROM reservations", [], |r| r.get(0))?;
    if legacy != 0 {
        return Err(ResourceStoreError::NeedsReconciliation);
    }
    let epoch: i64 = conn.query_row(
        "SELECT epoch FROM resource_ledger_meta WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let mut snapshot = LedgerSnapshot {
        epoch: u64::try_from(epoch).map_err(|_| ResourceStoreError::Invalid)?,
        owners: Default::default(),
    };
    let mut statement =
        conn.prepare("SELECT owner_id, footprint_json FROM resource_owners ORDER BY owner_id")?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (owner, json) = row?;
        if owner.is_empty() {
            return Err(ResourceStoreError::Invalid);
        }
        snapshot.owners.insert(owner, decode(&json)?);
    }
    Ok(snapshot)
}

pub(crate) fn reserve_increase_in_transaction(
    transaction: &Transaction<'_>,
    request: &GrantRequest,
    context: AdmissionContext<'_>,
) -> Result<GrantReceipt, ResourceStoreError> {
    if request.id.is_empty() || request.deployment_id.is_empty()
        || request.operation_id.is_empty() || request.revision < 1 || request.generation < 1 {
        return Err(ResourceStoreError::Invalid);
    }
    let encoded = encode(&request.next)?;
    let identity = serde_json::to_string(&(
        &request.deployment_id, &request.operation_id, request.revision,
        request.generation, request.expected_epoch, &encoded,
    ))?;
    let prior: Option<(String, i64)> = transaction.query_row(
        "SELECT request_json, committed_epoch FROM resource_grants WHERE id=?1",
        [&request.id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if let Some((previous, epoch)) = prior {
        if previous != identity { return Err(ResourceStoreError::Conflict); }
        let epoch = u64::try_from(epoch).map_err(|_| ResourceStoreError::Invalid)?;
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
    let snapshot = read_snapshot(transaction)?;
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
    Ok(GrantReceipt::New { epoch: epoch as u64 })
}

impl crate::Store {
    pub fn resource_snapshot(&self) -> Result<LedgerSnapshot, ResourceStoreError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let snapshot = read_snapshot(&transaction)?;
        transaction.commit()?;
        Ok(snapshot)
    }

    pub fn reserve_increase(&self, request: &GrantRequest, context: AdmissionContext<'_>)
        -> Result<GrantReceipt, ResourceStoreError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let receipt = reserve_increase_in_transaction(&transaction, request, context)?;
        transaction.commit()?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_footprints_decode_and_fail_closed() {
        let footprint = PhaseFootprint {
            phase: ResourcePhase::Ready,
            allocations: vec![Allocation {
                domain: "system".into(),
                bytes: 64,
                host_kv_bytes: 8,
            }],
            devices: vec![DeviceClaim {
                device: "gpu0".into(),
                sharing: Sharing::Shared,
            }],
        };
        let json = r#"{"version":1,"phase":"ready","allocations":[["system",64,8]],"devices":[["gpu0",true]]}"#;
        assert_eq!(decode(json).unwrap(), footprint);
        assert!(decode(&json.replace("\"version\":1", "\"version\":2")).is_err());
        assert!(decode(&json.replace("\"ready\"", "\"unknown\"")).is_err());
        assert!(decode(&json.replace(",64,8", ",-1,8")).is_err());
        assert!(decode(&json.replace("\"ready\"", "\"parked\"")).is_err());
    }

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
}

#[cfg(test)]
mod transaction_fault_tests {
    use super::*;

    fn reservation_context<'a>(
        observations: &'a [MemoryObservation],
        limits: &'a [MemoryLimit],
    ) -> AdmissionContext<'a> {
        AdmissionContext::new(observations, limits, 101, 60, 4)
    }

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
    fn caller_rollback_reverts_transaction_local_reservation() {
        let (store, request) = fixture();
        let observations = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
        let transaction = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();

        assert_eq!(
            reserve_increase_in_transaction(
                &transaction,
                &request,
                reservation_context(&observations, &limits),
            ).unwrap(),
            GrantReceipt::New { epoch: 1 },
        );
        transaction.rollback().unwrap();

        assert_eq!(store.resource_snapshot().unwrap(), LedgerSnapshot {
            epoch: 0,
            owners: Default::default(),
        });
        let grants: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM resource_grants", [], |row| row.get(0)).unwrap();
        assert_eq!(grants, 0);
    }

    #[test]
    fn caller_sql_failure_then_drop_reverts_transaction_local_reservation() {
        let (store, request) = fixture();
        let observations = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
        let transaction = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        reserve_increase_in_transaction(
            &transaction,
            &request,
            reservation_context(&observations, &limits),
        ).unwrap();

        let failure = transaction.execute(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version)
             VALUES ('a','duplicate','model','stopped',1,0,1,1)",
            [],
        );
        assert!(matches!(failure, Err(rusqlite::Error::SqliteFailure(_, _))));
        drop(transaction);

        assert_eq!(store.resource_snapshot().unwrap(), LedgerSnapshot {
            epoch: 0,
            owners: Default::default(),
        });
        let grants: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM resource_grants", [], |row| row.get(0)).unwrap();
        assert_eq!(grants, 0);
    }

    #[test]
    fn caller_commit_persists_transaction_local_reservation() {
        let (store, request) = fixture();
        let observations = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
        let transaction = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();

        let receipt = reserve_increase_in_transaction(
            &transaction,
            &request,
            reservation_context(&observations, &limits),
        ).unwrap();
        transaction.commit().unwrap();

        assert_eq!(receipt, GrantReceipt::New { epoch: 1 });
        assert_eq!(store.resource_snapshot().unwrap(), LedgerSnapshot {
            epoch: 1,
            owners: [(request.deployment_id.clone(), request.next.clone())].into(),
        });
        let grant: (String, String, i64) = store.conn.query_row(
            "SELECT deployment_id, operation_id, committed_epoch FROM resource_grants WHERE id=?1",
            [&request.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(grant, (request.deployment_id, request.operation_id, 1));
    }

    #[test]
    fn caller_transaction_replay_is_recorded_and_changed_identity_conflicts() {
        let (store, request) = fixture();
        let observations = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
        let transaction = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        let context = || reservation_context(&observations, &limits);

        assert_eq!(reserve_increase_in_transaction(&transaction, &request, context()).unwrap(),
            GrantReceipt::New { epoch: 1 });
        assert_eq!(reserve_increase_in_transaction(&transaction, &request, context()).unwrap(),
            GrantReceipt::Recorded { epoch: 1 });
        let snapshot = read_snapshot(&transaction).unwrap();
        assert_eq!(snapshot.epoch, 1);
        assert_eq!(snapshot.owners.get(&request.deployment_id), Some(&request.next));

        let mut changed = request.clone();
        changed.operation_id = "changed-operation".into();
        assert!(matches!(
            reserve_increase_in_transaction(&transaction, &changed, context()),
            Err(ResourceStoreError::Conflict),
        ));
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
