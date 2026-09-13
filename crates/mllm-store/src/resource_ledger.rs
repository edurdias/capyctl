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

fn read_snapshot(conn: &Connection) -> Result<LedgerSnapshot, ResourceStoreError> {
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

impl crate::Store {
    pub fn resource_snapshot(&self) -> Result<LedgerSnapshot, ResourceStoreError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let snapshot = read_snapshot(&transaction)?;
        transaction.commit()?;
        Ok(snapshot)
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
}
