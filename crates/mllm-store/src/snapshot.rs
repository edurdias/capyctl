//! Read-only durable-state projection, not a readiness or mutation authority.
//!
//! The cursor and every collection share one SQLite read transaction. No engine,
//! credential provider, event pruning, or coordinator action is invoked. This is
//! the Store foundation, not the complete management API snapshot: capability
//! hints, trusted live observations and missing lifecycle timestamps still need
//! coordinator integration. Recorded identities are historical facts, not fresh
//! proof of process ownership. Callers must never turn these DTOs into authority.

use crate::{events::EventCursor, Store};
use mllm_domain::resources::{ResourcePhase, Sharing};
use rusqlite::{types::ValueRef, Row, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize, Serializer};

const MAX_ROWS: usize = 4096;
const MAX_FIELD_BYTES: usize = 16 * 1024;
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot exceeds bounded read limits")]
    TooLarge,
    #[error("invalid durable snapshot data")]
    CorruptData,
    // Do not reflect SQLite messages or stored data through public diagnostics.
    #[error("snapshot database read failed")]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub api_version: &'static str,
    /// Explicitly identifies this partial Store projection to API integrators.
    pub scope: &'static str,
    #[serde(serialize_with = "serialize_cursor")]
    pub cursor: EventCursor,
    pub ledger_epoch: String,
    pub session_epoch: String,
    pub deployments: Vec<DeploymentSnapshot>,
    pub routes: Vec<RouteSnapshot>,
    pub operations: Vec<OperationSnapshot>,
    pub runs: Vec<RunSnapshot>,
    pub steps: Vec<StepSnapshot>,
    pub claims: Vec<ClaimSnapshot>,
    pub bindings: Vec<BindingSnapshot>,
    pub reservations: Vec<ReservationSnapshot>,
    pub legacy_reservations: Vec<LegacyReservationSnapshot>,
    pub grants: Vec<GrantSnapshot>,
    pub host_policy_revisions: Vec<HostPolicyRevision>,
    pub observations: Vec<DomainObservation>,
}

fn serialize_cursor<S: Serializer>(cursor: &EventCursor, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(cursor)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentSnapshot {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub legacy_route: Option<String>,
    pub revision: String,
    pub generation: String,
    pub desired_state: String,
    pub observed_state: String,
    pub admission_enabled: bool,
    pub dispatch_enabled: bool,
    pub suspended: bool,
    /// Current revision fingerprint only; never the raw effective configuration.
    pub effective_fingerprint: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteSnapshot {
    pub deployment_id: String,
    pub route: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationSnapshot {
    pub id: String,
    /// Null for host-scoped operations; never invent a deployment association.
    pub deployment_id: Option<String>,
    pub action: String,
    pub state: String,
    pub has_error: bool,
    /// Existing persisted ISO timestamp, not a fabricated millisecond value.
    pub accepted_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSnapshot {
    pub operation_id: String,
    pub deployment_id: String,
    pub revision: String,
    pub generation: String,
    pub action: String,
    pub state: String,
    pub deadline_ms: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepSnapshot {
    pub id: String,
    pub operation_id: String,
    pub ordinal: String,
    pub deployment_id: String,
    pub binding_id: String,
    pub state: String,
    pub grant_id: Option<String>,
    pub evidence_epoch: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimSnapshot {
    pub deployment_id: String,
    pub operation_id: String,
    pub revision: String,
    pub generation: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingSnapshot {
    pub id: String,
    pub deployment_id: String,
    pub revision: String,
    pub incarnation: String,
    pub ownership: String,
    pub state: String,
    pub recorded_identities: Vec<RecordedIdentity>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordedIdentity {
    pub role: String,
    pub pid: u32,
    pub boot_id: String,
    pub start_ticks: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIdentity {
    role: String,
    pid: u32,
    boot_id: String,
    start_ticks: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReservationSnapshot {
    pub owner_id: String,
    pub phase: &'static str,
    pub allocations: Vec<AllocationSnapshot>,
    pub devices: Vec<DeviceSnapshot>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllocationSnapshot {
    pub domain: String,
    pub bytes: String,
    pub host_kv_bytes: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceSnapshot {
    pub device: String,
    pub shared: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyReservationSnapshot {
    pub owner_id: String,
    pub domain_id: Option<String>,
    pub bytes: String,
    pub phase: String,
    pub exclusive_devices: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrantSnapshot {
    pub id: String,
    pub deployment_id: String,
    pub operation_id: String,
    pub committed_epoch: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostPolicyRevision {
    pub host_id: String,
    pub revision: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DomainObservation {
    pub domain_id: String,
    pub kind: String,
    pub observed_bytes: Option<String>,
    pub observed_at: Option<String>,
}

#[derive(Default)]
struct Budget {
    rows: usize,
    bytes: usize,
}
impl Budget {
    fn read<T>(
        &mut self,
        tx: &Transaction<'_>,
        sql: &str,
        map: impl Fn(&Row<'_>) -> Result<T, SnapshotError>,
    ) -> Result<Vec<T>, SnapshotError> {
        let mut statement = tx.prepare(sql)?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            self.rows += 1;
            if self.rows > MAX_ROWS {
                return Err(SnapshotError::TooLarge);
            }
            // Inspect borrowed SQLite values BEFORE allocating Rust strings.
            for column in 0..columns {
                let bytes = match row.get_ref(column)? {
                    ValueRef::Text(value) | ValueRef::Blob(value) => value.len(),
                    _ => 8,
                };
                if bytes > MAX_FIELD_BYTES {
                    return Err(SnapshotError::TooLarge);
                }
                self.bytes += bytes;
                if self.bytes > MAX_INPUT_BYTES {
                    return Err(SnapshotError::TooLarge);
                }
            }
            result.push(map(row)?);
        }
        Ok(result)
    }
}
fn number(row: &Row<'_>, column: usize) -> Result<String, SnapshotError> {
    let value: i64 = row.get(column)?;
    if value < 0 {
        return Err(SnapshotError::CorruptData);
    }
    Ok(value.to_string())
}
fn optional_number(row: &Row<'_>, column: usize) -> Result<Option<String>, SnapshotError> {
    let value: Option<i64> = row.get(column)?;
    if value.is_some_and(|v| v < 0) {
        return Err(SnapshotError::CorruptData);
    }
    Ok(value.map(|v| v.to_string()))
}
fn boolean(row: &Row<'_>, column: usize) -> Result<bool, SnapshotError> {
    match row.get::<_, i64>(column)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(SnapshotError::CorruptData),
    }
}

impl Store {
    /// Bounded coherent read. Overflow is an error, never a partial snapshot.
    /// Numeric counters/bytes/revisions are decimal strings for future JSON clients.
    pub fn snapshot(&self) -> Result<Snapshot, SnapshotError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let mut budget = Budget::default();
        let mut meta = budget.read(&tx, "SELECT incarnation,COALESCE((SELECT seq FROM sqlite_sequence WHERE name='management_events'),0),(SELECT epoch FROM resource_ledger_meta WHERE singleton=1),(SELECT epoch FROM coordinator_session WHERE singleton=1) FROM event_meta WHERE singleton=1", |r| {
            let incarnation: String = r.get(0)?;
            let sequence: i64 = r.get(1)?;
            if incarnation.parse::<ulid::Ulid>().is_err() || sequence < 0 { return Err(SnapshotError::CorruptData); }
            Ok((EventCursor { incarnation, sequence }, number(r,2)?, number(r,3)?))
        })?;
        let (cursor, ledger_epoch, session_epoch) = meta.pop().ok_or(SnapshotError::CorruptData)?;
        let deployments = budget.read(&tx, "SELECT d.id,d.name,d.kind,d.route_model_id,d.revision,d.current_generation,d.desired_state,d.observed_state,d.admission_enabled,d.dispatch_enabled,d.suspended,e.fingerprint FROM deployments d LEFT JOIN effective_revisions e ON e.deployment_id=d.id AND e.revision=d.revision ORDER BY d.id", |r| Ok(DeploymentSnapshot {
            id:r.get(0)?, name:r.get(1)?, kind:r.get(2)?, legacy_route:r.get(3)?, revision:number(r,4)?, generation:number(r,5)?, desired_state:r.get(6)?, observed_state:r.get(7)?, admission_enabled:boolean(r,8)?, dispatch_enabled:boolean(r,9)?, suspended:boolean(r,10)?, effective_fingerprint:r.get(11)?,
        }))?;
        let routes = budget.read(
            &tx,
            "SELECT deployment_id,route FROM deployment_routes ORDER BY deployment_id,route",
            |r| {
                Ok(RouteSnapshot {
                    deployment_id: r.get(0)?,
                    route: r.get(1)?,
                })
            },
        )?;
        let operations = budget.read(
            &tx,
            "SELECT id,deployment_id,kind,state,error_code IS NOT NULL,accepted_at
             FROM operations ORDER BY id",
            |r| {
                Ok(OperationSnapshot {
                    id: r.get(0)?,
                    deployment_id: r.get(1)?,
                    action: r.get(2)?,
                    state: r.get(3)?,
                    has_error: boolean(r, 4)?,
                    accepted_at: r.get(5)?,
                })
            },
        )?;
        let runs = budget.read(
            &tx,
            "SELECT operation_id,deployment_id,revision,generation,action,state,deadline_ms
             FROM lifecycle_runs ORDER BY operation_id",
            |r| {
                Ok(RunSnapshot {
                    operation_id: r.get(0)?,
                    deployment_id: r.get(1)?,
                    revision: number(r, 2)?,
                    generation: number(r, 3)?,
                    action: r.get(4)?,
                    state: r.get(5)?,
                    deadline_ms: number(r, 6)?,
                })
            },
        )?;
        let steps = budget.read(&tx,
            "SELECT s.id,s.operation_id,s.ordinal,s.deployment_id,s.binding_id,s.state,s.grant_id,e.committed_epoch
             FROM lifecycle_steps s LEFT JOIN lifecycle_evidence e ON e.step_id=s.id
             ORDER BY s.operation_id,s.ordinal", |r| {
                Ok(StepSnapshot {
                    id: r.get(0)?, operation_id: r.get(1)?, ordinal: number(r,2)?,
                    deployment_id: r.get(3)?, binding_id: r.get(4)?, state: r.get(5)?,
                    grant_id: r.get(6)?, evidence_epoch: optional_number(r,7)?,
                })
            })?;
        let claims = budget.read(&tx,
            "SELECT deployment_id,operation_id,revision,generation FROM lifecycle_claims ORDER BY deployment_id", |r| {
                Ok(ClaimSnapshot {
                    deployment_id: r.get(0)?, operation_id: r.get(1)?, revision: number(r,2)?, generation: number(r,3)?,
                })
            })?;
        let bindings = budget.read(&tx, "SELECT id,deployment_id,revision,incarnation,ownership,state,identities_json FROM runtime_bindings WHERE state!='released' ORDER BY id", |r| {
            let json: String = r.get(6)?;
            let stored: Vec<StoredIdentity> = serde_json::from_str(&json).map_err(|_| SnapshotError::CorruptData)?;
            if stored.len() > 256 || stored.iter().any(|i| i.pid == 0 || i.role.is_empty() || i.boot_id.is_empty()) { return Err(SnapshotError::CorruptData); }
            Ok(BindingSnapshot { id:r.get(0)?,deployment_id:r.get(1)?,revision:number(r,2)?,incarnation:r.get(3)?,ownership:r.get(4)?,state:r.get(5)?,recorded_identities:stored.into_iter().map(|i| RecordedIdentity {role:i.role,pid:i.pid,boot_id:i.boot_id,start_ticks:i.start_ticks.to_string()}).collect() })
        })?;
        let reservations = budget.read(
            &tx,
            "SELECT owner_id,footprint_json FROM resource_owners ORDER BY owner_id",
            |r| {
                let json: String = r.get(1)?;
                let f = crate::resource_ledger::decode(&json)
                    .map_err(|_| SnapshotError::CorruptData)?;
                Ok(ReservationSnapshot {
                    owner_id: r.get(0)?,
                    phase: match f.phase {
                        ResourcePhase::Cold => "cold",
                        ResourcePhase::Ready => "ready",
                        ResourcePhase::Parking => "parking",
                        ResourcePhase::Parked => "parked",
                        ResourcePhase::Wake => "wake",
                    },
                    allocations: f
                        .allocations
                        .into_iter()
                        .map(|a| AllocationSnapshot {
                            domain: a.domain,
                            bytes: a.bytes.to_string(),
                            host_kv_bytes: a.host_kv_bytes.to_string(),
                        })
                        .collect(),
                    devices: f
                        .devices
                        .into_iter()
                        .map(|d| DeviceSnapshot {
                            device: d.device,
                            shared: d.sharing == Sharing::Shared,
                        })
                        .collect(),
                })
            },
        )?;
        let legacy_reservations = budget.read(&tx, "SELECT owner_id,domain_id,bytes,phase,exclusive_devices FROM reservations ORDER BY owner_id,domain_id,phase,bytes,exclusive_devices", |r| {
            let json: Option<String> = r.get(4)?;
            let devices = json.map(|j| serde_json::from_str::<Vec<String>>(&j)).transpose().map_err(|_| SnapshotError::CorruptData)?.unwrap_or_default();
            Ok(LegacyReservationSnapshot { owner_id:r.get(0)?,domain_id:r.get(1)?,bytes:number(r,2)?,phase:r.get(3)?,exclusive_devices:devices })
        })?;
        let grants = budget.read(
            &tx,
            "SELECT id,deployment_id,operation_id,committed_epoch FROM resource_grants ORDER BY id",
            |r| {
                Ok(GrantSnapshot {
                    id: r.get(0)?,
                    deployment_id: r.get(1)?,
                    operation_id: r.get(2)?,
                    committed_epoch: number(r, 3)?,
                })
            },
        )?;
        let host_policy_revisions = budget.read(
            &tx,
            "SELECT host_id,revision FROM host_resource_policies ORDER BY host_id",
            |r| {
                Ok(HostPolicyRevision {
                    host_id: r.get(0)?,
                    revision: number(r, 1)?,
                })
            },
        )?;
        let observations = budget.read(
            &tx,
            "SELECT id,kind,observed_bytes,observed_at FROM domains ORDER BY id",
            |r| {
                Ok(DomainObservation {
                    domain_id: r.get(0)?,
                    kind: r.get(1)?,
                    observed_bytes: optional_number(r, 2)?,
                    observed_at: r.get(3)?,
                })
            },
        )?;
        let snapshot = Snapshot {
            api_version: "1",
            scope: "durable_store_foundation",
            cursor,
            ledger_epoch,
            session_epoch,
            deployments,
            routes,
            operations,
            runs,
            steps,
            claims,
            bindings,
            reservations,
            legacy_reservations,
            grants,
            host_policy_revisions,
            observations,
        };
        // Bound wire size without allocating a second copy of the response.
        let mut sink = SizeLimit(0);
        serde_json::to_writer(&mut sink, &snapshot).map_err(|_| SnapshotError::TooLarge)?;
        tx.commit()?;
        Ok(snapshot)
    }
}

struct SizeLimit(usize);
impl std::io::Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_OUTPUT_BYTES - self.0 {
            return Err(std::io::Error::other("snapshot size limit"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
