use capyctl_domain::resources::*;
use capyctl_scheduler::residency::{
    admit_phase, validate_footprint, AdmissionContext, ResourceError,
};
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

pub(crate) fn decode(json: &str) -> Result<PhaseFootprint, ResourceStoreError> {
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

pub(crate) fn encode(footprint: &PhaseFootprint) -> Result<String, ResourceStoreError> {
    validate_footprint(footprint)?;
    let phase = match footprint.phase {
        ResourcePhase::Cold => "cold",
        ResourcePhase::Ready => "ready",
        ResourcePhase::Parking => "parking",
        ResourcePhase::Parked => "parked",
        ResourcePhase::Wake => "wake",
    };
    let mut allocations: Vec<_> = footprint
        .allocations
        .iter()
        .map(|a| (a.domain.clone(), a.bytes, a.host_kv_bytes))
        .collect();
    let mut devices: Vec<_> = footprint
        .devices
        .iter()
        .map(|d| (d.device.clone(), d.sharing == Sharing::Shared))
        .collect();
    allocations.sort();
    devices.sort();
    Ok(serde_json::to_string(&StoredFootprint {
        version: 1,
        phase: phase.into(),
        allocations,
        devices,
    })?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRequest {
    pub id: String,
    /// ADR 0013 §5: the resource owner charged, `instance_owner_id` of the
    /// deployment and the instance the fence names. Instance 0's owner is the
    /// deployment id, as every reservation made before instances existed.
    pub owner_id: String,
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

fn ensure_increasing(
    old: Option<&PhaseFootprint>,
    next: &PhaseFootprint,
) -> Result<(), ResourceStoreError> {
    if !matches!(
        next.phase,
        ResourcePhase::Cold | ResourcePhase::Parking | ResourcePhase::Wake
    ) {
        return Err(ResourceStoreError::Conflict);
    }
    match old {
        None if next.phase != ResourcePhase::Cold => Err(ResourceStoreError::Conflict),
        Some(old) => {
            if !matches!(
                (old.phase, next.phase),
                (ResourcePhase::Ready, ResourcePhase::Parking)
                    | (ResourcePhase::Parked, ResourcePhase::Wake)
            ) {
                return Err(ResourceStoreError::Conflict);
            }
            for previous in &old.allocations {
                let current = next
                    .allocations
                    .iter()
                    .find(|a| a.domain == previous.domain)
                    .ok_or(ResourceStoreError::Conflict)?;
                if current.bytes < previous.bytes || current.host_kv_bytes < previous.host_kv_bytes
                {
                    return Err(ResourceStoreError::Conflict);
                }
            }
            for claim in &old.devices {
                if !next.devices.contains(claim) {
                    return Err(ResourceStoreError::Conflict);
                }
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

/// The host a registered ledger key belongs to, or `None` when the key is not in
/// the host-scoped registry.
fn registered_host(
    transaction: &Transaction<'_>,
    kind: &str,
    ledger_key: &str,
) -> Result<Option<String>, ResourceStoreError> {
    Ok(transaction
        .query_row(
            "SELECT host_id FROM host_resource_keys WHERE kind=?1 AND ledger_key=?2",
            params![kind, ledger_key],
            |r| r.get(0),
        )
        .optional()?)
}

/// SPEC §7 (T26, T27): one ledger holds every host's owners, but a host's
/// admission is decided against that host's own limits and observations. An
/// owner whose every domain and device key the host-scoped registry assigns to
/// some other host cannot change this admission, so it is set aside here. It is
/// not released or rewritten, and the epoch check above still covers the whole
/// ledger. An owner with any unregistered key, or any key on a host in scope,
/// stays in scope and is judged exactly as before, so an unknown charge still
/// fails closed.
fn scoped_to_context_hosts(
    transaction: &Transaction<'_>,
    snapshot: &LedgerSnapshot,
    context: &AdmissionContext<'_>,
) -> Result<LedgerSnapshot, ResourceStoreError> {
    scoped_to_domain_hosts(
        transaction,
        snapshot,
        context.limits.iter().map(|limit| limit.domain.as_str()),
    )
}

/// The same scoping for any judgement made against one host's domains: the
/// hosts owning `domains` are in scope, and an unregistered domain keeps the
/// whole ledger (Phase B, SPEC §7, T26 T27). The resource-policy overcommit
/// report and the park/switch planners use it as well as admission.
pub(crate) fn scoped_to_domain_hosts<'a>(
    transaction: &Transaction<'_>,
    snapshot: &LedgerSnapshot,
    domains: impl IntoIterator<Item = &'a str>,
) -> Result<LedgerSnapshot, ResourceStoreError> {
    let mut in_scope = std::collections::BTreeSet::new();
    for domain in domains {
        match registered_host(transaction, "domain", domain)? {
            Some(host) => {
                in_scope.insert(host);
            }
            // An unscoped limit (an unregistered ledger) keeps the whole ledger.
            None => return Ok(snapshot.clone()),
        }
    }
    let mut scoped = LedgerSnapshot {
        epoch: snapshot.epoch,
        owners: Default::default(),
    };
    for (owner, footprint) in &snapshot.owners {
        let mut elsewhere = true;
        let keys = footprint
            .allocations
            .iter()
            .map(|a| ("domain", a.domain.as_str()))
            .chain(
                footprint
                    .devices
                    .iter()
                    .map(|d| ("device", d.device.as_str())),
            );
        for (kind, key) in keys {
            match registered_host(transaction, kind, key)? {
                Some(host) if !in_scope.contains(&host) => {}
                _ => {
                    elsewhere = false;
                    break;
                }
            }
        }
        if !elsewhere || (footprint.allocations.is_empty() && footprint.devices.is_empty()) {
            scoped.owners.insert(owner.clone(), footprint.clone());
        }
    }
    Ok(scoped)
}

pub(crate) fn reserve_increase_in_transaction(
    transaction: &Transaction<'_>,
    request: &GrantRequest,
    context: AdmissionContext<'_>,
) -> Result<GrantReceipt, ResourceStoreError> {
    reserve_in_transaction(transaction, request, context, GrantTransition::Increase)
}

enum GrantTransition {
    Increase,
}
fn reserve_in_transaction(
    transaction: &Transaction<'_>,
    request: &GrantRequest,
    context: AdmissionContext<'_>,
    transition: GrantTransition,
) -> Result<GrantReceipt, ResourceStoreError> {
    if request.id.is_empty()
        || request.owner_id.is_empty()
        || request.deployment_id.is_empty()
        || request.operation_id.is_empty()
        || request.revision < 1
        || request.generation < 1
    {
        return Err(ResourceStoreError::Invalid);
    }
    let encoded = encode(&request.next)?;
    let identity = serde_json::to_string(&(
        &request.deployment_id,
        &request.operation_id,
        request.revision,
        request.generation,
        request.expected_epoch,
        &encoded,
    ))?;
    let prior: Option<(String, i64)> = transaction
        .query_row(
            "SELECT request_json, committed_epoch FROM resource_grants WHERE id=?1",
            [&request.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((previous, epoch)) = prior {
        if previous != identity {
            return Err(ResourceStoreError::Conflict);
        }
        let epoch = u64::try_from(epoch).map_err(|_| ResourceStoreError::Invalid)?;
        return Ok(GrantReceipt::Recorded { epoch });
    }
    // ADR 0013 §5: the fence names one instance, whose owner id is fixed.
    let state: Option<(i64, i64, String, String, u32)> = transaction
        .query_row(
            "SELECT i.revision, i.generation, d.kind, o.state, i.instance_index
         FROM deployment_instances i JOIN deployments d ON d.id=i.deployment_id
         JOIN operations o ON o.deployment_id=d.id
         WHERE d.id=?1 AND o.id=?2 AND i.generation=?3",
            params![
                request.deployment_id,
                request.operation_id,
                request.generation
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((revision, generation, kind, operation_state, instance)) = state else {
        return Err(ResourceStoreError::Conflict);
    };
    if revision != request.revision
        || generation != request.generation
        || request.owner_id != crate::instances::instance_owner_id(&request.deployment_id, instance)
        || kind != "model"
        || !matches!(operation_state.as_str(), "pending" | "running")
    {
        return Err(ResourceStoreError::Conflict);
    }
    let snapshot = read_snapshot(transaction)?;
    if snapshot.epoch != request.expected_epoch {
        return Err(ResourceStoreError::Conflict);
    }
    match transition {
        GrantTransition::Increase => {
            ensure_increasing(snapshot.owners.get(&request.owner_id), &request.next)?
        }
    }
    let scoped = scoped_to_context_hosts(transaction, &snapshot, &context)?;
    match admit_phase(&scoped, &request.owner_id, &request.next, context) {
        Ok(()) => {}
        // Found live 2026-09-23 (matrix M27): a phase that allocates nothing
        // beyond what the owner holds (a park's parking phase) needs no free
        // memory; its own charge is already in use and the host's published
        // free memory cannot cover it a second time. The limits it could
        // breach are the category ones, which admit_phase reports separately.
        Err(capyctl_domain::resources::ResourceError::Insufficient)
            if snapshot
                .owners
                .get(&request.owner_id)
                .is_some_and(|current| !allocates_more(current, &request.next)) => {}
        Err(error) => return Err(error.into()),
    }
    let epoch = i64::try_from(snapshot.epoch)
        .ok()
        .and_then(|e| e.checked_add(1))
        .ok_or(ResourceStoreError::Invalid)?;
    transaction.execute(
        "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index) VALUES(?1,?2,?3,?4)
         ON CONFLICT(owner_id) DO UPDATE SET footprint_json=excluded.footprint_json",
        params![request.owner_id, encoded, request.deployment_id, instance],
    )?;
    transaction.execute(
        "UPDATE resource_ledger_meta SET epoch=?1 WHERE singleton=1",
        [epoch],
    )?;
    transaction.execute(
        "INSERT INTO resource_grants(id, deployment_id, operation_id, request_json, committed_epoch)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![request.id, request.deployment_id, request.operation_id, identity, epoch])?;
    Ok(GrantReceipt::New {
        epoch: epoch as u64,
    })
}

/// Whether `next` allocates anything `current` does not hold: more bytes or
/// host KV in any domain, or a device claim it does not already have.
pub(crate) fn allocates_more(current: &PhaseFootprint, next: &PhaseFootprint) -> bool {
    next.allocations.iter().any(|wanted| {
        !current.allocations.iter().any(|held| {
            held.domain == wanted.domain
                && held.bytes >= wanted.bytes
                && held.host_kv_bytes >= wanted.host_kv_bytes
        })
    }) || next
        .devices
        .iter()
        .any(|device| !current.devices.contains(device))
}

pub(crate) fn advance_completion_epoch(
    tx: &rusqlite::Transaction<'_>,
) -> Result<u64, crate::lifecycle::LifecycleError> {
    let epoch: i64 = tx.query_row(
        "SELECT epoch FROM resource_ledger_meta WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let next = epoch
        .checked_add(1)
        .filter(|v| *v > 0)
        .ok_or(crate::lifecycle::LifecycleError::CorruptStoredData)?;
    tx.execute(
        "UPDATE resource_ledger_meta SET epoch=?1 WHERE singleton=1",
        [next],
    )?;
    Ok(next as u64)
}
impl crate::Store {
    pub fn resource_snapshot(&self) -> Result<LedgerSnapshot, ResourceStoreError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let snapshot = read_snapshot(&transaction)?;
        transaction.commit()?;
        Ok(snapshot)
    }

    /// The ledger as one host's judgement sees it (SPEC §7, T26 T27): owners
    /// whose every key belongs to another host are set aside, so a park or
    /// switch plan for this host is not refused over another host's charges and
    /// `max_parked` counts this host's parked owners only. The epoch is the whole
    /// ledger's. An unregistered domain keeps every owner, failing closed.
    pub fn host_scoped_resource_snapshot(
        &self,
        domains: &[&str],
    ) -> Result<LedgerSnapshot, ResourceStoreError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let snapshot = read_snapshot(&transaction)?;
        let scoped = scoped_to_domain_hosts(&transaction, &snapshot, domains.iter().copied())?;
        transaction.commit()?;
        Ok(scoped)
    }

    pub fn reserve_increase(
        &self,
        request: &GrantRequest,
        context: AdmissionContext<'_>,
    ) -> Result<GrantReceipt, ResourceStoreError> {
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
        let footprint = PhaseFootprint {
            phase: ResourcePhase::Ready,
            allocations: vec![
                Allocation {
                    domain: "gpu-memory:0".into(),
                    bytes: 64,
                    host_kv_bytes: 0,
                },
                Allocation {
                    domain: "system".into(),
                    bytes: 32,
                    host_kv_bytes: 8,
                },
            ],
            devices: vec![
                DeviceClaim {
                    device: "gpu0".into(),
                    sharing: Sharing::Shared,
                },
                DeviceClaim {
                    device: "gpu1".into(),
                    sharing: Sharing::Exclusive,
                },
            ],
        };
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
        let request = GrantRequest {
            id: "grant-a".into(),
            owner_id: "a".into(),
            deployment_id: "a".into(),
            operation_id: "op-a".into(),
            revision: 1,
            generation: 1,
            expected_epoch: 0,
            next: PhaseFootprint {
                phase: ResourcePhase::Cold,
                allocations: vec![Allocation {
                    domain: "system".into(),
                    bytes: 60,
                    host_kv_bytes: 0,
                }],
                devices: vec![],
            },
        };
        (store, request)
    }

    #[test]
    fn receipt_failure_rolls_back_owner_and_epoch() {
        let (store, request) = fixture();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_grant BEFORE INSERT ON resource_grants
            BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;",
            )
            .unwrap();
        let obs = [MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        }];
        let limits = [MemoryLimit {
            domain: "system".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let context = AdmissionContext::new(&obs, &limits, 101, 60, 4);
        assert!(matches!(
            store.reserve_increase(&request, context),
            Err(ResourceStoreError::Sql(_))
        ));
        assert_eq!(
            store.resource_snapshot().unwrap(),
            LedgerSnapshot {
                epoch: 0,
                owners: Default::default()
            }
        );
        store
            .conn
            .execute_batch("DROP TRIGGER reject_grant;")
            .unwrap();
        assert_eq!(
            store.reserve_increase(&request, context).unwrap(),
            GrantReceipt::New { epoch: 1 }
        );
    }

    #[test]
    fn caller_rollback_reverts_transaction_local_reservation() {
        let (store, request) = fixture();
        let observations = [MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        }];
        let limits = [MemoryLimit {
            domain: "system".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let transaction =
            Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();

        assert_eq!(
            reserve_increase_in_transaction(
                &transaction,
                &request,
                reservation_context(&observations, &limits),
            )
            .unwrap(),
            GrantReceipt::New { epoch: 1 },
        );
        transaction.rollback().unwrap();

        assert_eq!(
            store.resource_snapshot().unwrap(),
            LedgerSnapshot {
                epoch: 0,
                owners: Default::default(),
            }
        );
        let grants: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM resource_grants", [], |row| row.get(0))
            .unwrap();
        assert_eq!(grants, 0);
    }

    #[test]
    fn caller_sql_failure_then_drop_reverts_transaction_local_reservation() {
        let (store, request) = fixture();
        let observations = [MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        }];
        let limits = [MemoryLimit {
            domain: "system".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let transaction =
            Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        reserve_increase_in_transaction(
            &transaction,
            &request,
            reservation_context(&observations, &limits),
        )
        .unwrap();

        let failure = transaction.execute(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version)
             VALUES ('a','duplicate','model','stopped',1,0,1,1)",
            [],
        );
        assert!(matches!(failure, Err(rusqlite::Error::SqliteFailure(_, _))));
        drop(transaction);

        assert_eq!(
            store.resource_snapshot().unwrap(),
            LedgerSnapshot {
                epoch: 0,
                owners: Default::default(),
            }
        );
        let grants: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM resource_grants", [], |row| row.get(0))
            .unwrap();
        assert_eq!(grants, 0);
    }

    #[test]
    fn caller_commit_persists_transaction_local_reservation() {
        let (store, request) = fixture();
        let observations = [MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        }];
        let limits = [MemoryLimit {
            domain: "system".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let transaction =
            Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();

        let receipt = reserve_increase_in_transaction(
            &transaction,
            &request,
            reservation_context(&observations, &limits),
        )
        .unwrap();
        transaction.commit().unwrap();

        assert_eq!(receipt, GrantReceipt::New { epoch: 1 });
        assert_eq!(
            store.resource_snapshot().unwrap(),
            LedgerSnapshot {
                epoch: 1,
                owners: [(request.deployment_id.clone(), request.next.clone())].into(),
            }
        );
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
        let observations = [MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        }];
        let limits = [MemoryLimit {
            domain: "system".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let transaction =
            Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        let context = || reservation_context(&observations, &limits);

        assert_eq!(
            reserve_increase_in_transaction(&transaction, &request, context()).unwrap(),
            GrantReceipt::New { epoch: 1 }
        );
        assert_eq!(
            reserve_increase_in_transaction(&transaction, &request, context()).unwrap(),
            GrantReceipt::Recorded { epoch: 1 }
        );
        let snapshot = read_snapshot(&transaction).unwrap();
        assert_eq!(snapshot.epoch, 1);
        assert_eq!(
            snapshot.owners.get(&request.deployment_id),
            Some(&request.next)
        );

        let mut changed = request.clone();
        changed.operation_id = "changed-operation".into();
        assert!(matches!(
            reserve_increase_in_transaction(&transaction, &changed, context()),
            Err(ResourceStoreError::Conflict),
        ));
    }

    /// Registers `host` with one domain and one device under host-scoped keys.
    fn register_host(store: &crate::Store, host: &str) {
        store
            .conn
            .execute(
                "INSERT INTO host_resource_namespaces(host_id,policy_key,kind) VALUES (?1,?1,'remote')",
                [host],
            )
            .unwrap();
        for (kind, local) in [("domain", "unified"), ("device", "gpu0")] {
            store
                .conn
                .execute(
                    "INSERT INTO host_resource_keys(host_id,kind,local_id,ledger_key) VALUES (?1,?2,?3,?4)",
                    params![host, kind, local, format!("{host}/{kind}/{local}")],
                )
                .unwrap();
        }
    }

    // T26 T27: found live (Phase B, two hosts): a Ready SGLang deployment on
    // host-b made every activation on host-a fail with "unknown physical
    // domain", because the other host's charge was judged against this host's
    // limits. Another host's owner is set aside; an unregistered charge is not.
    #[test]
    fn another_hosts_owner_does_not_block_admission_but_an_unknown_charge_still_does() {
        let (store, mut request) = fixture();
        register_host(&store, "host-a");
        register_host(&store, "host-b");
        store
            .conn
            .execute_batch(
                "INSERT INTO deployments(id,name,kind,desired_state,
          admission_enabled,suspended,current_generation,schema_version)
          VALUES ('b','b','model','ready',1,0,1,1);",
            )
            .unwrap();
        let elsewhere = PhaseFootprint {
            phase: ResourcePhase::Ready,
            allocations: vec![Allocation {
                domain: "host-b/domain/unified".into(),
                bytes: 90,
                host_kv_bytes: 0,
            }],
            devices: vec![DeviceClaim {
                device: "host-b/device/gpu0".into(),
                sharing: Sharing::Shared,
            }],
        };
        store
            .conn
            .execute(
                "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('b',?1,'b')",
                [encode(&elsewhere).unwrap()],
            )
            .unwrap();
        request.next.allocations[0].domain = "host-a/domain/unified".into();
        let observations = [MemoryObservation {
            domain: "host-a/domain/unified".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        }];
        let limits = [MemoryLimit {
            domain: "host-a/domain/unified".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        assert_eq!(
            store
                .reserve_increase(&request, reservation_context(&observations, &limits))
                .unwrap(),
            GrantReceipt::New { epoch: 1 }
        );
        // The other host's owner is untouched, not released.
        let snapshot = store.resource_snapshot().unwrap();
        assert_eq!(snapshot.owners.get("b"), Some(&elsewhere));
        assert_eq!(snapshot.owners.len(), 2);

        // A charge whose domain no host registered still fails closed.
        let (store, mut request) = fixture();
        register_host(&store, "host-a");
        store
            .conn
            .execute_batch(
                "INSERT INTO deployments(id,name,kind,desired_state,
          admission_enabled,suspended,current_generation,schema_version)
          VALUES ('b','b','model','ready',1,0,1,1);",
            )
            .unwrap();
        let mut unknown = elsewhere;
        unknown.allocations[0].domain = "unregistered".into();
        unknown.devices.clear();
        store
            .conn
            .execute(
                "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('b',?1,'b')",
                [encode(&unknown).unwrap()],
            )
            .unwrap();
        request.next.allocations[0].domain = "host-a/domain/unified".into();
        assert!(matches!(
            store.reserve_increase(&request, reservation_context(&observations, &limits)),
            Err(ResourceStoreError::Admission(ResourceError::UnknownDomain))
        ));
    }

    #[test]
    fn legacy_reservations_are_not_silently_ignored() {
        let (store, _) = fixture();
        store.conn.execute_batch("INSERT INTO owners(id,kind,deployment_id) VALUES ('a','model','a');
            INSERT INTO reservations(owner_id,domain_id,bytes,phase) VALUES ('a','system',60,'activation');").unwrap();
        assert!(matches!(
            store.resource_snapshot(),
            Err(ResourceStoreError::NeedsReconciliation)
        ));
    }

    #[test]
    fn increasing_writer_cannot_release_memory_or_claims() {
        let (_, request) = fixture();
        let mut old = request.next;
        old.phase = ResourcePhase::Ready;
        old.devices.push(DeviceClaim {
            device: "gpu0".into(),
            sharing: Sharing::Shared,
        });
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
        let obs = [MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        }];
        let limits = [MemoryLimit {
            domain: "system".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let context = AdmissionContext::new(&obs, &limits, 101, 60, 4);
        store
            .conn
            .execute("UPDATE deployments SET revision=2 WHERE id='a'", [])
            .unwrap();
        assert!(matches!(
            store.reserve_increase(&request, context),
            Err(ResourceStoreError::Conflict)
        ));
        request.revision = 2;
        store
            .conn
            .execute("UPDATE operations SET state='failed' WHERE id='op-a'", [])
            .unwrap();
        assert!(matches!(
            store.reserve_increase(&request, context),
            Err(ResourceStoreError::Conflict)
        ));
        store
            .conn
            .execute("UPDATE operations SET state='running' WHERE id='op-a'", [])
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('a','{}','a')",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.reserve_increase(&request, context),
            Err(ResourceStoreError::Json(_))
        ));
        let epoch: i64 = store
            .conn
            .query_row(
                "SELECT epoch FROM resource_ledger_meta WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let receipts: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM resource_grants", [], |r| r.get(0))
            .unwrap();
        assert_eq!((epoch, receipts), (0, 0));
    }
}
