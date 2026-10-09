//! Read-only durable-state projection, not a readiness or mutation authority.
//!
//! The cursor and every collection share one SQLite read transaction. No engine,
//! credential provider, event pruning, or coordinator action is invoked. This is
//! the Store foundation, not the complete management API snapshot: capability
//! hints, trusted live observations and missing lifecycle timestamps still need
//! coordinator integration. Recorded identities are historical facts, not fresh
//! proof of process ownership. Callers must never turn these DTOs into authority.

use crate::development_controls::{from_stored, DevelopmentControls};
use crate::{events::EventCursor, Store};
use capyctl_domain::resources::{ResourcePhase, Sharing};
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
    /// SPEC §9.1 / T21 / P4: whether this revision's launch enables vLLM
    /// development mode, derived from its effective configuration. Additive.
    pub development_controls: DevelopmentControls,
    /// Owner decision 2026-09-22 (schema v19): set when the upgrade could not
    /// carry this revision's pre-E1 engine settings forward. Everything the
    /// deployment owns is retained; the text says what the operator must do.
    /// Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_action: Option<String>,
    /// ADR 0014 §7 (WE3): the current revision's checkpoint digest record;
    /// `state: pending` is the `checkpoint_digest_pending` condition. Absent
    /// for a revision accepted before WE3 whose digest is not yet recorded.
    /// Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_digest: Option<crate::checkpoint_digests::CheckpointDigest>,
    /// ADR 0008: the current revision's declared remote model source, per
    /// host: `pending`, `downloading` with its bytes, `verified`, or `failed`
    /// with a closed reason. Absent for a local source. Additive.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_sources: Vec<crate::model_sources::ModelSourceRecord>,
    /// ADR 0013 §6: the current revision's declared instance count. Additive.
    pub desired_instances: u32,
    /// ADR 0013 §6: instances whose derived state is `ready`. Additive.
    pub ready_instances: u32,
    /// ADR 0013 §6: `degraded` when at least one instance is READY but fewer
    /// than the desired count (declared, less operator-stopped instances) are.
    /// Additive.
    pub conditions: Vec<&'static str>,
    /// ADR 0013 §6: every instance: index, placement, generation, derived state,
    /// reservation owner and its own development-control mark. Additive.
    pub instances: Vec<InstanceSnapshot>,
    /// ADR 0013 §3: every allowed host the current revision was resolved
    /// against, and the reason each refusing host refused. A refusing host is
    /// never a placement candidate. Additive.
    pub hosts: Vec<HostResolutionSnapshot>,
    /// ADR 0014 amendment A1: the current revision's timeouts and the windows
    /// a start or stop that names no deadline is given. Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<crate::lifecycle_windows::LifecycleWindows>,
    /// Owner decision 2026-09-23: the current revision's startup reservation
    /// with its provenance (`declared`, `default`, `resources`, `request`, or
    /// `measured` once a peak is recorded on its host and installation, owner
    /// decision 2026-10-07: the figure admission reserves) and every startup
    /// peak measured for it. Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup: Option<crate::ordinary_lifecycle::startup::StartupStatus>,
    /// ADR 0014 amendment A13: what a park of the current revision is
    /// charged on the memory that holds the engine's device allocations, and
    /// every parked residue measured for it. Absent for a deployment that
    /// never parks or declares its resources. Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parked: Option<crate::ordinary_lifecycle::parked_charge::ParkedStatus>,
    /// W10 (owner decision 2026-09-23): the switch in progress that names this
    /// deployment as its target or a victim: target, host, victims and phase.
    /// Additive; absent when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch: Option<crate::switch_state::SwitchStatus>,
    /// SPEC §6.5 (ADR 0013 amendment 2026-09-23): the current revision
    /// declares a warm-residency commitment. Additive; absent when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub warm: bool,
    /// SPEC §6.5, §10: `explicit` when the deployment moves only on an
    /// operator's action, by its own `lifecycle.activation` or a host's.
    /// Additive; absent when on demand.
    #[serde(skip_serializing_if = "capyctl_config::instances::Activation::is_on_demand")]
    pub activation: capyctl_config::instances::Activation,
    /// SPEC §6.4: the deployment's most recently accepted operation, with its
    /// closed error code, the recorded reason and a fixed operator hint.
    /// Additive; absent when the deployment has no operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_operation: Option<LatestOperation>,
    /// ADR 0014 §5 (owner decision 2026-09-25): the current revision's
    /// effective context, its source (`declared`, `host_fixed`, `fitted`,
    /// `fallback`, or `on_host` when a remote host fits it at launch) and why.
    /// Additive; absent when the revision does not decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<capyctl_config::context_fit::ContextFit>,
    /// ADR 0024 (owner decision 2026-10-03): the tool-call and reasoning
    /// parsers the current revision's launch passes and where they came from
    /// (`on_host` for a choice a remote host makes at launch). Additive; absent
    /// for an engine without a parser setting or a revision that does not decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parsers: Option<capyctl_config::parsers::Parsers>,
}

/// ADR 0014 §5 (owner decision 2026-09-25): the effective context of the
/// current revision. An embedded (standalone) checkpoint is read here, where
/// the launch reads it; a revision placed on enrolled remote hosts is fitted by
/// the host, so a host's path is never read on this machine.
type LaunchStatus = (
    Option<capyctl_config::context_fit::ContextFit>,
    Option<capyctl_config::parsers::Parsers>,
);

/// ADR 0024: the parsers are chosen where the context is fitted, the same way.
fn context_status(
    conn: &rusqlite::Connection,
    deployment_id: &str,
) -> rusqlite::Result<LaunchStatus> {
    let Some((effective, remote)) = current_effective(conn, deployment_id)? else {
        return Ok((None, None));
    };
    Ok(if remote {
        (
            Some(capyctl_config::context_fit::fit_on_remote_host(&effective)),
            capyctl_config::parsers::parsers_on_remote_host(&effective),
        )
    } else {
        (
            Some(capyctl_config::context_fit::fit_for_effective(&effective)),
            capyctl_config::parsers::parsers_for_effective(&effective),
        )
    })
}

/// The current revision's effective configuration, and whether it was
/// resolved on an enrolled remote host (which then fits it from its own
/// checkpoint). `None` when the revision does not decode.
fn current_effective(
    conn: &rusqlite::Connection,
    deployment_id: &str,
) -> rusqlite::Result<Option<(capyctl_config::effective::EffectiveDeployment, bool)>> {
    use rusqlite::OptionalExtension;
    let row: Option<(String, bool)> = conn
        .query_row(
            "SELECT e.effective_json,EXISTS(SELECT 1 FROM host_effective_revisions h JOIN enrolled_hosts x ON x.host_id=h.host_id WHERE h.deployment_id=d.id AND h.revision=d.revision)
               FROM deployments d JOIN effective_revisions e ON e.deployment_id=d.id AND e.revision=d.revision WHERE d.id=?1",
            [deployment_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(raw, remote)| {
        capyctl_config::effective::decode_effective_snapshot(&raw)
            .ok()
            .map(|effective| (effective, remote))
    }))
}

/// SPEC §§10, 17 (owner decision 2026-10-08): one deployment as the
/// management load read names it: its instances and the running limit
/// CapyCTL derives for its current revision. Live figures (router counts,
/// host samples) are joined by the service; nothing here is an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentCapacity {
    pub id: String,
    pub name: String,
    /// `None` when the current revision does not decode.
    pub max_running: Option<capyctl_config::context_fit::MaxRunning>,
    pub instances: Vec<InstanceCapacity>,
}

/// ADR 0013 §6: one instance of a [`DeploymentCapacity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceCapacity {
    pub index: u32,
    pub host_id: Option<String>,
    /// The generation its last activation drew; `None` until activated.
    pub generation: Option<i64>,
    /// As status derives it (`ready`, `parked`, `starting`, ...).
    pub observed_state: String,
}

impl Store {
    /// SPEC §§10, 17 (owner decision 2026-10-08): the deployments the
    /// management load read reports, every one or the one `deployment` names
    /// (empty when it names none), with their instances and derived running
    /// limit. One bounded read transaction; no engine is touched. The limit is
    /// fitted as status fits it, so only for the deployments returned.
    pub fn capacity(
        &self,
        deployment: Option<&str>,
    ) -> Result<Vec<DeploymentCapacity>, SnapshotError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let mut budget = Budget::default();
        let mut deployments: Vec<DeploymentCapacity> = budget
            .read(
                &tx,
                "SELECT d.id,d.name FROM deployments d WHERE d.kind!='deleted' ORDER BY d.id",
                |r| {
                    Ok(DeploymentCapacity {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        max_running: None,
                        instances: Vec::new(),
                    })
                },
            )?
            .into_iter()
            .filter(|d| deployment.is_none_or(|wanted| d.id == wanted))
            .collect();
        if deployments.is_empty() {
            return Ok(deployments);
        }
        let instances_sql = format!(
            "SELECT i.deployment_id,i.instance_index,i.host_id,i.generation,{INSTANCE_OBSERVED_STATE} FROM deployment_instances i ORDER BY i.deployment_id,i.instance_index"
        );
        let instances = budget.read(&tx, &instances_sql, |r| {
            let generation: Option<i64> = r.get(3)?;
            if generation.is_some_and(|g| g < 0) {
                return Err(SnapshotError::CorruptData);
            }
            Ok((
                r.get::<_, String>(0)?,
                InstanceCapacity {
                    index: r.get(1)?,
                    host_id: r.get(2)?,
                    generation,
                    observed_state: r.get(4)?,
                },
            ))
        })?;
        for (id, instance) in instances {
            if let Some(entry) = deployments.iter_mut().find(|d| d.id == id) {
                entry.instances.push(instance);
            }
        }
        for entry in &mut deployments {
            entry.max_running = current_effective(&tx, &entry.id)?.map(|(effective, remote)| {
                let fit = if remote {
                    capyctl_config::context_fit::fit_on_remote_host(&effective)
                } else {
                    capyctl_config::context_fit::fit_for_effective(&effective)
                };
                capyctl_config::context_fit::max_running_for_effective(&effective, &fit, remote)
            });
        }
        Ok(deployments)
    }
}

/// SPEC §6.4: an operation and its error as status shows them. The reason is
/// the latest journal evidence of an operation that did not succeed, bounded
/// and redacted (`capyctl_domain::diagnostics::public_reason`): one line, never an
/// engine log tail, an option value or a credential. The hint is fixed text
/// for the closed category the error belongs to. Recorded history, not proof
/// of what runs now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatestOperation {
    pub id: String,
    pub kind: String,
    pub state: String,
    /// Only a closed code (`[a-z0-9_:.]`); anything else is not shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
    /// Found live 2026-10-04: the start gave up (its retries are spent, or its
    /// failure may not be replayed) and closed its own admission; the
    /// operation stays pending until its deadline, but nothing more is
    /// attempted for it. A waiting client ends its wait on it. Additive.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub given_up: bool,
}

/// ADR 0013 §3: one allowed host's resolution of the current revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostResolutionSnapshot {
    pub host_id: String,
    /// `resolved` or `refused`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    /// ADR 0017: the release version the host declared on its latest control
    /// session. Absent for a host that has not connected since v34. Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_version: Option<String>,
    /// ADR 0017: the version skew verdict on that version: `supported`,
    /// `upgrade_recommended`, `upgrade_required` (drain-only) or `refused`.
    /// Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    /// ADR 0017: why, when not supported on the server's own line. Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_reason: Option<String>,
}

/// ADR 0013 §6: one instance of a deployment. Recorded placement and identity
/// are historical facts, not proof that a process runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstanceSnapshot {
    pub index: u32,
    /// The placed host; absent until an activation places the instance.
    pub host_id: Option<String>,
    /// The devices chosen at placement; absent until placed.
    pub devices: Option<serde_json::Value>,
    /// The generation its last activation drew; absent until activated.
    pub generation: Option<String>,
    /// Derived per instance (SPEC §6.1, ADR 0013 §6): `uncertain`, `starting`,
    /// `queued` (accepted, or waiting for a host to fit), `stopping`,
    /// `reconciling`, `ready`, `parking`, `parked`, `waking` (W5), `failed`
    /// (it gave up and closed its own admission) or `stopped`. A runtime
    /// nothing can interpret reads `uncertain`, never `stopped`.
    pub observed_state: String,
    /// The revision its current incarnation was admitted against; absent
    /// until it first starts. A count-only revision leaves it unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The closed placement or start diagnostic, when it has one (for
    /// example no allowed host fits without eviction). Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// `active`, or `retiring` while a count decrease drains it.
    pub lifecycle: String,
    /// Owner decision Q7: stopped by `stop instance`.
    pub operator_stopped: bool,
    /// The resource owner charged for this instance, when it holds a reservation.
    pub reservation_owner: Option<String>,
    /// SPEC §9.1 / T21 / P4 (W14): the mark of the revision as resolved on
    /// this instance's host, or the deployment's revision while unplaced.
    pub development_controls: DevelopmentControls,
    /// Owner decision 2026-09-23: the startup reservation its start in
    /// flight holds (armed) or will hold (queued) until Ready, with its
    /// provenance (`measured` when a first run's peak replaced the default).
    /// Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup: Option<crate::ordinary_lifecycle::startup::StartupReservation>,
    /// SPEC §6.4: this instance's most recently accepted operation and its
    /// error (see [`LatestOperation`]). Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_operation: Option<LatestOperation>,
    /// ADR 0028 §15: a group instance's plan and members, flattened into the
    /// instance. Absent for a single-host instance (T39). Additive.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupStatus>,
}

/// ADR 0028 §15: a group instance as status shows it, read from its newest
/// group plan (the unsettled one while any member is unsettled; a group
/// never records a placed host). Recorded facts, not proof that a rank runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupStatus {
    /// `vllm`, `sglang` or `tensorfold`.
    pub engine: &'static str,
    pub topology: GroupTopologyStatus,
    /// The head's peer address and the plan's rendezvous port.
    pub rendezvous: String,
    /// ADR 0028 §13: always `unauthenticated`. The engines' rendezvous,
    /// broadcast and collective ports take no credential.
    pub peer_transport: &'static str,
    /// The generation the plan was drawn for; the instance's own
    /// `generation` moves on while a stopped plan's members still settle.
    pub plan_generation: String,
    /// In node-rank order, the head first.
    pub members: Vec<GroupMemberStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupTopologyStatus {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
}

/// ADR 0028 §11, §15: one member of a group instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupMemberStatus {
    /// The member's host id.
    pub host: String,
    pub node_rank: u32,
    /// `head` or `worker`.
    pub role: &'static str,
    /// `reserved`, `dispatching`, `launched`, `uncertain` (its host has not
    /// proven it gone; it keeps its charge) or `settled`, or `failed` for the
    /// rank the group failed at once it is no longer uncertain.
    pub state: &'static str,
    /// The processes recorded for it while it is unsettled; 0 once settled.
    pub processes: u32,
    /// Its charge on its own host, `None` once released.
    pub reservation: Option<MemberReservationStatus>,
    /// Its residency as resolved on its own host.
    pub residency: Option<String>,
    /// The closed code (spec §16) that names what happened to it:
    /// the group's failure code at the failed rank, else
    /// `group_member_uncertain` while uncertain.
    pub last_error: Option<String>,
}

/// ADR 0028 §5: what one member's owner holds on its own host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberReservationStatus {
    /// `cold`, `ready`, `parking`, `parked` or `wake`.
    pub phase: &'static str,
    /// The bytes it holds across its host's domains, as a decimal string.
    pub bytes: String,
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
    /// SPEC §6.4: the error's closed code; a code of any other shape is not
    /// shown. Additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
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

/// A text scalar of the current effective revision, or NULL when the stored
/// configuration is not valid JSON or the value is absent or not text. It is
/// cut to 64 bytes, so an oversized value decodes as unrecognized instead of
/// failing the bounded read. SQLite
/// evaluates only the taken `CASE` branch, so malformed JSON never raises.
fn effective_text(path: &str) -> String {
    json_text("e.effective_json", path)
}
fn json_text(column: &str, path: &str) -> String {
    format!("CASE WHEN json_valid({column}) THEN CASE WHEN json_type({column},'{path}')='text' THEN substr(json_extract({column},'{path}'),1,64) END END")
}
/// A boolean scalar as 1/0, or 2 when present but not a boolean, else NULL.
fn effective_bool(path: &str) -> String {
    json_bool("e.effective_json", path)
}
fn json_bool(column: &str, path: &str) -> String {
    format!("CASE WHEN json_valid({column}) THEN CASE WHEN json_type({column},'{path}') IS NULL THEN NULL WHEN json_type({column},'{path}')='true' THEN 1 WHEN json_type({column},'{path}')='false' THEN 0 ELSE 2 END END")
}

/// The reported observed state (SPEC §§6.1, 6.4; Phase B follow-up). The stored
/// `observed_state` changes only on evidence, so on its own it reads `stopped`
/// while a start is queued or running and `ready` while dispatch is closed for a
/// readiness re-proof. Status derives what is actually happening, in order:
/// - `uncertain`: a launch or cleanup outcome is unresolved (G3);
/// - `parking` / `waking`: a park or restore of the instance is accepted and
///   not yet settled (W5; its stored state changes only on evidence);
/// - `queued` / `starting`: an activation is accepted and not yet sent, or sent
///   and not yet Ready;
/// - `stopping`: a stop is accepted and its cleanup not yet verified;
/// - `reconciling`: Ready by evidence, but dispatch is closed while readiness is
///   re-proven (host session loss or restart, engine exit, or leases a crashed
///   session left behind) for a coordinator-managed deployment;
/// - `failed`: stopped, and the latest operation was the verified cleanup that
///   followed an engine exit (W13, principal `system:engine_exit`); the next
///   start or on-demand activation replaces it, and an operator's Stop (SPEC
///   §6.3, `admin_stopped`) acknowledges it as `stopped`;
/// - otherwise the stored state.
///
/// ADR 0013 §6: the same derivation for one instance `i`, over its own runs,
/// steps, bindings and reservation. ADR 0013 §7: compaction moves an instance
/// into an index earlier incarnations used, so the closed-history rules (the
/// two `failed` cases) read only runs of the instance's current generation
/// (generations are drawn once per deployment, so each names one
/// incarnation). The open-run rules need no filter: an instance with a run in
/// flight is never moved.
const INSTANCE_OBSERVED_STATE: &str = "CASE \
 WHEN EXISTS(SELECT 1 FROM lifecycle_steps u JOIN runtime_bindings ub ON ub.id=u.binding_id WHERE u.deployment_id=i.deployment_id AND ub.instance_index=i.instance_index AND u.state='uncertain') THEN 'uncertain' \
 WHEN EXISTS(SELECT 1 FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND o.kind='park' AND r.state IN ('queued','running')) THEN 'parking' \
 WHEN EXISTS(SELECT 1 FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND o.kind='restore' AND r.state IN ('queued','running')) THEN 'waking' \
 WHEN i.observed_state!='ready' AND EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action='activate' AND r.state IN ('queued','running')) THEN \
   CASE WHEN EXISTS(SELECT 1 FROM lifecycle_runs r JOIN lifecycle_steps t ON t.operation_id=r.operation_id WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action='activate' AND r.state IN ('queued','running') AND t.state='armed') THEN 'starting' ELSE 'queued' END \
 WHEN i.observed_state!='stopped' AND EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action='stop' AND r.state IN ('queued','running')) THEN 'stopping' \
 WHEN i.observed_state='ready' AND i.dispatch_enabled=0 AND EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index) THEN 'reconciling' \
 WHEN i.observed_state='stopped' AND (EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index AND b.state!='released') OR EXISTS(SELECT 1 FROM resource_owners o WHERE o.deployment_id=i.deployment_id AND o.instance_index=i.instance_index)) THEN 'uncertain' \
 WHEN i.observed_state='stopped' AND i.pending_start_until_ms IS NOT NULL THEN 'queued' \
 WHEN i.observed_state='stopped' AND NOT EXISTS(SELECT 1 FROM deployments sd WHERE sd.id=i.deployment_id AND sd.admin_stopped=1) AND (SELECT c.principal_id FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id JOIN command_receipts c ON c.operation_id=o.id WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.generation=i.generation ORDER BY o.accepted_at DESC,o.id DESC LIMIT 1)='system:engine_exit' THEN 'failed' \
 WHEN i.observed_state='stopped' AND i.desired_state='ready' AND i.admission_enabled=0 AND EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.generation=i.generation AND r.action='activate') THEN 'failed' \
 ELSE i.observed_state END";

const OBSERVED_STATE: &str = "CASE \
 WHEN EXISTS(SELECT 1 FROM lifecycle_steps u WHERE u.deployment_id=d.id AND u.state='uncertain') THEN 'uncertain' \
 WHEN d.observed_state!='ready' AND EXISTS(SELECT 1 FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id WHERE r.deployment_id=d.id AND o.kind='restore' AND r.state IN ('queued','running')) THEN 'waking' \
 WHEN d.observed_state='ready' AND d.dispatch_enabled=0 AND EXISTS(SELECT 1 FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id WHERE r.deployment_id=d.id AND o.kind='park' AND r.state IN ('queued','running')) THEN 'parking' \
 WHEN d.observed_state!='ready' AND EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=d.id AND r.action='activate' AND r.state IN ('queued','running')) THEN \
   CASE WHEN EXISTS(SELECT 1 FROM lifecycle_runs r JOIN lifecycle_steps t ON t.operation_id=r.operation_id WHERE r.deployment_id=d.id AND r.action='activate' AND r.state IN ('queued','running') AND t.state='armed') THEN 'starting' ELSE 'queued' END \
 WHEN d.observed_state!='stopped' AND EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=d.id AND r.action='stop' AND r.state IN ('queued','running')) THEN 'stopping' \
 WHEN d.observed_state='ready' AND d.dispatch_enabled=0 AND EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=d.id) THEN 'reconciling' \
 WHEN d.observed_state='stopped' AND d.admin_stopped=0 AND (SELECT c.principal_id FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id JOIN command_receipts c ON c.operation_id=o.id WHERE r.deployment_id=d.id ORDER BY o.accepted_at DESC,o.id DESC LIMIT 1)='system:engine_exit' THEN 'failed' \
 ELSE d.observed_state END";

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
        // SPEC §6.3 (W6): a deleted deployment is a tombstone kept for its
        // history; it is not listed. Its operations stay listed below.
        // SPEC §§6.1, 6.4 (G3): a deployment whose launch or cleanup outcome is
        // unresolved still holds its reservation and may still run an engine. It
        // is reported uncertain, never the stored `stopped` it had before starting.
        // SPEC §9.1 / T21 / P4: only the scalars the development-control mark
        // needs are extracted, type-checked in SQL, so the raw effective
        // configuration is never read into the snapshot or reflected. Sleep
        // mode is read from `engine_config` (ADR 0014) and, for a revision frozen
        // before it, from the profile's launch settings.
        let deployments_sql = format!("SELECT d.id,d.name,d.kind,d.route_model_id,d.revision,d.current_generation,d.desired_state,{},d.admission_enabled,d.dispatch_enabled,d.suspended,e.fingerprint,{},{},{},COALESCE({},{}),{},(SELECT 'engine configuration not carried forward by the upgrade: '||substr(m.diagnostic,1,1024)||'; stop the deployment and replace its configuration with an engine_config' FROM engine_config_migrations m WHERE m.deployment_id=d.id AND m.revision=d.revision AND m.outcome='refused'),(SELECT n.instances FROM deployment_revision_instances n WHERE n.deployment_id=d.id AND n.revision=d.revision) FROM deployments d LEFT JOIN effective_revisions e ON e.deployment_id=d.id AND e.revision=d.revision WHERE d.kind!='deleted' ORDER BY d.id",
            OBSERVED_STATE, effective_text("$.profile.engine"), effective_text("$.profile.security.deep_park"), effective_text("$.profile.security.deep_park_source"), effective_bool("$.engine_config.enable_sleep_mode"), effective_bool("$.profile.launch_settings.enable_sleep_mode"), effective_text("$.residency"));
        let deployments = budget.read(&tx, &deployments_sql, |r| {
            Ok(DeploymentSnapshot {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                legacy_route: r.get(3)?,
                revision: number(r, 4)?,
                generation: number(r, 5)?,
                desired_state: r.get(6)?,
                observed_state: r.get(7)?,
                admission_enabled: boolean(r, 8)?,
                dispatch_enabled: boolean(r, 9)?,
                suspended: boolean(r, 10)?,
                effective_fingerprint: r.get(11)?,
                development_controls: from_stored(
                    r.get(12)?,
                    r.get(13)?,
                    r.get(14)?,
                    r.get(15)?,
                    r.get(16)?,
                ),
                operator_action: r.get(17)?,
                checkpoint_digest: None,
                model_sources: Vec::new(),
                desired_instances: r.get::<_, Option<u32>>(18)?.unwrap_or(0),
                ready_instances: 0,
                conditions: Vec::new(),
                instances: Vec::new(),
                hosts: Vec::new(),
                timeouts: None,
                startup: None,
                parked: None,
                switch: None,
                warm: false,
                activation: capyctl_config::instances::Activation::OnDemand,
                latest_operation: None,
                context: None,
                parsers: None,
            })
        })?;
        let mut deployments = deployments;
        let digests = budget.read(&tx, "SELECT c.deployment_id,c.state,c.host_id,c.expected,c.digest,c.weights_bytes,c.provisional,c.diagnostic,c.state_slot_bytes,c.provenance FROM checkpoint_digests c JOIN deployments d ON d.id=c.deployment_id AND d.revision=c.revision ORDER BY c.deployment_id", |r| {
            use crate::checkpoint_digests::{CheckpointDigest, DigestState};
            let state = match r.get::<_, String>(1)?.as_str() {
                "pending" => DigestState::Pending,
                "recorded" => DigestState::Recorded,
                "mismatch" => DigestState::Mismatch,
                "unusable" => DigestState::Unusable,
                _ => return Err(SnapshotError::CorruptData),
            };
            let weights: Option<i64> = r.get(5)?;
            if weights.is_some_and(|w| w < 0) { return Err(SnapshotError::CorruptData); }
            let state_slot: Option<i64> = r.get(8)?;
            if state_slot.is_some_and(|s| s <= 0) { return Err(SnapshotError::CorruptData); }
            // ADR 0014 §7 (amendment of 2026-10-08): where the digest came from.
            let provenance = match r.get::<_, Option<String>>(9)? {
                None => None,
                Some(text) => Some(
                    capyctl_config::effective::DigestProvenance::parse(&text)
                        .filter(|_| !text.is_empty())
                        .ok_or(SnapshotError::CorruptData)?,
                ),
            };
            Ok((r.get::<_, String>(0)?, CheckpointDigest {
                state, host_id: r.get(2)?, expected: r.get(3)?, digest: r.get(4)?, weights_bytes: weights,
                state_slot_bytes: state_slot, provisional: boolean(r, 6)?, diagnostic: r.get(7)?,
                provenance,
            }))
        })?;
        // ADR 0014 amendment A1: read per deployment in this transaction.
        for entry in &mut deployments {
            entry.timeouts = crate::lifecycle_windows::read(&tx, &entry.id)?;
            entry.startup = crate::ordinary_lifecycle::startup::status(&tx, &entry.id)?;
            entry.parked = crate::ordinary_lifecycle::parked_charge::status(&tx, &entry.id)?;
            (entry.context, entry.parsers) = context_status(&tx, &entry.id)?;
            entry.switch = crate::switch_state::status(&tx, &entry.id)?;
            entry.warm = crate::switch_state::is_warm(&tx, &entry.id)?;
            if crate::switch_state::is_explicit_activation(&tx, &entry.id)? {
                entry.activation = capyctl_config::instances::Activation::Explicit;
            }
            entry.latest_operation = latest_operation(
                &tx,
                &format!("SELECT {LATEST_OPERATION} FROM operations o WHERE o.deployment_id=?1 ORDER BY o.accepted_at DESC,o.rowid DESC LIMIT 1"),
                rusqlite::params![entry.id],
            )?;
        }
        // ADR 0008: the current revision's source records.
        let sources = budget.read(&tx, "SELECT s.deployment_id,s.host_id,s.source_key,s.state,s.bytes_done,s.bytes_total,s.reason,s.terminal FROM model_sources s JOIN deployments d ON d.id=s.deployment_id AND d.revision=s.revision ORDER BY s.deployment_id,s.host_id,s.source_key", |r| {
            use crate::model_sources::{ModelSourceRecord, SourceState};
            let state = match r.get::<_, String>(3)?.as_str() {
                "pending" => SourceState::Pending,
                "downloading" => SourceState::Downloading,
                "verified" => SourceState::Verified,
                "failed" => SourceState::Failed,
                _ => return Err(SnapshotError::CorruptData),
            };
            let bytes = |index| -> Result<u64, SnapshotError> {
                u64::try_from(r.get::<_, i64>(index)?).map_err(|_| SnapshotError::CorruptData)
            };
            Ok((r.get::<_, String>(0)?, ModelSourceRecord {
                host_id: r.get(1)?, source_key: r.get(2)?, state,
                bytes_done: bytes(4)?, bytes_total: bytes(5)?, reason: r.get(6)?,
                terminal: boolean(r, 7)?,
            }))
        })?;
        for (deployment, record) in sources {
            if let Some(entry) = deployments.iter_mut().find(|d| d.id == deployment) {
                entry.model_sources.push(record);
            }
        }
        for (deployment, digest) in digests {
            if let Some(entry) = deployments.iter_mut().find(|d| d.id == deployment) {
                entry.checkpoint_digest = Some(digest);
            }
        }
        // ADR 0013 §6: per-instance status. The chosen JSON is the revision as
        // resolved on the instance's host, else the deployment's revision.
        let chosen = "COALESCE(h.effective_json,e.effective_json)";
        // The instance's own revision when it has one (a count-only revision
        // leaves a running instance on its predecessor), else the deployment's.
        // ADR 0028 §5: an instance's reservation owner is its instance-form
        // owner; a group instance's members are listed as reservations each.
        let instances_sql = format!("SELECT i.deployment_id,i.instance_index,i.host_id,i.device_json,i.generation,i.state,i.operator_stopped,{},(SELECT o.owner_id FROM resource_owners o WHERE o.deployment_id=i.deployment_id AND o.instance_index=i.instance_index AND o.member_rank IS NULL),{},{},{},COALESCE({},{}),{},i.revision,substr(i.last_error,1,1024) FROM deployment_instances i JOIN deployments d ON d.id=i.deployment_id LEFT JOIN effective_revisions e ON e.deployment_id=d.id AND e.revision=COALESCE(i.revision,d.revision) LEFT JOIN host_effective_revisions h ON h.deployment_id=d.id AND h.revision=COALESCE(i.revision,d.revision) AND h.host_id=i.host_id AND h.outcome='resolved' ORDER BY i.deployment_id,i.instance_index",
            INSTANCE_OBSERVED_STATE, json_text(chosen, "$.profile.engine"), json_text(chosen, "$.profile.security.deep_park"), json_text(chosen, "$.profile.security.deep_park_source"), json_bool(chosen, "$.engine_config.enable_sleep_mode"), json_bool(chosen, "$.profile.launch_settings.enable_sleep_mode"), json_text(chosen, "$.residency"));
        let instances = budget.read(&tx, &instances_sql, |r| {
            let devices: Option<String> = r.get(3)?;
            let devices = devices
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(|_| SnapshotError::CorruptData)?;
            Ok((
                r.get::<_, String>(0)?,
                InstanceSnapshot {
                    index: r.get(1)?,
                    host_id: r.get(2)?,
                    devices,
                    generation: optional_number(r, 4)?,
                    lifecycle: r.get(5)?,
                    operator_stopped: boolean(r, 6)?,
                    observed_state: r.get(7)?,
                    reservation_owner: r.get(8)?,
                    development_controls: from_stored(
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                        r.get(12)?,
                        r.get(13)?,
                    ),
                    revision: optional_number(r, 14)?,
                    last_error: r.get(15)?,
                    startup: None,
                    latest_operation: None,
                    group: None,
                },
            ))
        })?;
        for (deployment, mut instance) in instances {
            if let Some(entry) = deployments.iter_mut().find(|d| d.id == deployment) {
                instance.latest_operation = latest_operation(
                    &tx,
                    // ADR 0013 §7: only the current incarnation's runs; the
                    // index may have been used by an earlier, retired one.
                    &format!("SELECT {LATEST_OPERATION} FROM operations o JOIN lifecycle_runs r ON r.operation_id=o.id WHERE r.deployment_id=?1 AND r.instance_index=?2 AND r.generation=(SELECT i.generation FROM deployment_instances i WHERE i.deployment_id=?1 AND i.instance_index=?2) ORDER BY o.accepted_at DESC,o.rowid DESC LIMIT 1"),
                    rusqlite::params![deployment, instance.index],
                )?;
                // ADR 0028 §15: a group instance is shown from its plan,
                // each member's residency as resolved on its own host for
                // the revision the instance runs.
                let revision = instance
                    .revision
                    .clone()
                    .unwrap_or_else(|| entry.revision.clone());
                instance.group = group_status(&tx, &deployment, instance.index, &revision)?;
                entry.instances.push(instance);
            }
        }
        let starting = crate::ordinary_lifecycle::startup::instance_reservations(&tx)
            .map_err(|_| SnapshotError::CorruptData)?;
        for (deployment, index, reservation) in starting {
            if let Some(instance) = deployments
                .iter_mut()
                .find(|d| d.id == deployment)
                .and_then(|d| d.instances.iter_mut().find(|i| i.index == index))
            {
                instance.startup = Some(reservation);
            }
        }
        let hosts = budget.read(&tx, "SELECT h.deployment_id,h.host_id,h.outcome,substr(h.diagnostic,1,256),v.binary_version,v.compatibility,NULLIF(v.reason,'') FROM host_effective_revisions h JOIN deployments d ON d.id=h.deployment_id AND d.revision=h.revision LEFT JOIN host_versions v ON v.host_id=h.host_id WHERE d.kind!='deleted' ORDER BY h.deployment_id,h.host_id", |r| {
            Ok((r.get::<_, String>(0)?, HostResolutionSnapshot {
                host_id: r.get(1)?, outcome: r.get(2)?, diagnostic: r.get(3)?,
                binary_version: r.get(4)?, compatibility: r.get(5)?, compatibility_reason: r.get(6)?,
            }))
        })?;
        for (deployment, host) in hosts {
            if let Some(entry) = deployments.iter_mut().find(|d| d.id == deployment) {
                entry.hosts.push(host);
            }
        }
        for entry in &mut deployments {
            if entry.desired_instances == 0 {
                entry.desired_instances =
                    u32::try_from(entry.instances.len()).map_err(|_| SnapshotError::CorruptData)?;
            }
            entry.ready_instances = u32::try_from(
                entry
                    .instances
                    .iter()
                    .filter(|i| i.observed_state == "ready")
                    .count(),
            )
            .map_err(|_| SnapshotError::CorruptData)?;
            let stopped = u32::try_from(
                entry
                    .instances
                    .iter()
                    .filter(|i| i.operator_stopped && i.lifecycle == "active")
                    .count(),
            )
            .map_err(|_| SnapshotError::CorruptData)?;
            let wanted = entry.desired_instances.saturating_sub(stopped);
            if entry.ready_instances >= 1 && entry.ready_instances < wanted {
                entry.conditions.push("degraded");
            }
            if let Some(state) = aggregate_state(&entry.instances) {
                entry.observed_state = state.into();
            }
        }
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
            "SELECT id,deployment_id,kind,state,error_code IS NOT NULL,accepted_at,substr(error_code,1,65)
             FROM operations ORDER BY id",
            |r| {
                Ok(OperationSnapshot {
                    id: r.get(0)?,
                    deployment_id: r.get(1)?,
                    action: r.get(2)?,
                    state: r.get(3)?,
                    has_error: boolean(r, 4)?,
                    accepted_at: r.get(5)?,
                    error_code: r
                        .get::<_, Option<String>>(6)?
                        .filter(|code| capyctl_domain::diagnostics::is_closed_code(code)),
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
                    phase: phase_name(f.phase),
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

/// SPEC §6.4: the columns [`latest_operation`] reads, the operation aliased
/// `o`: its latest journal evidence is cut in SQL before it is read.
const LATEST_OPERATION: &str = "o.id,o.kind,o.state,o.error_code,(SELECT substr(j.evidence,1,4096) FROM journal_entries j WHERE j.operation_id=o.id ORDER BY j.rowid DESC LIMIT 1),(SELECT j.state='given_up' FROM journal_entries j WHERE j.operation_id=o.id ORDER BY j.rowid DESC LIMIT 1)";

/// SPEC §6.4: one latest operation, with its closed error code and, when it did
/// not succeed, its bounded reason and hint.
fn latest_operation(
    tx: &Transaction<'_>,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Option<LatestOperation>, SnapshotError> {
    use capyctl_domain::diagnostics::{classify, is_closed_code, operator_hint, public_reason};
    let mut statement = tx.prepare_cached(sql)?;
    let mut rows = statement.query(params)?;
    let Some(r) = rows.next()? else {
        return Ok(None);
    };
    for column in 0..5 {
        if let ValueRef::Text(value) | ValueRef::Blob(value) = r.get_ref(column)? {
            if value.len() > MAX_FIELD_BYTES {
                return Err(SnapshotError::TooLarge);
            }
        }
    }
    let state: String = r.get(2)?;
    let error_code = r
        .get::<_, Option<String>>(3)?
        .filter(|code| is_closed_code(code));
    let failed = state != "succeeded";
    let reason = failed
        .then(|| r.get::<_, Option<String>>(4))
        .transpose()?
        .flatten()
        .and_then(|evidence| public_reason(&evidence));
    let hint = failed
        .then(|| classify(error_code.as_deref(), reason.as_deref().unwrap_or("")))
        .flatten()
        .and_then(operator_hint);
    Ok(Some(LatestOperation {
        id: r.get(0)?,
        kind: r.get(1)?,
        state,
        error_code,
        reason,
        hint,
        given_up: failed && r.get::<_, Option<bool>>(5)?.unwrap_or(false),
    }))
}

/// ADR 0028 §15: the group status of instance `index` of `deployment_id`, or
/// `None` when it never ran as a group. Read from its newest plan: while a
/// member is unsettled that is the only unsettled plan (one per instance),
/// since a new generation never starts before every member settled.
fn group_status(
    tx: &Transaction<'_>,
    deployment_id: &str,
    index: u32,
    revision: &str,
) -> Result<Option<GroupStatus>, SnapshotError> {
    use crate::groups::MemberState;
    use capyctl_domain::group::MemberRole;
    use rusqlite::OptionalExtension;
    let newest: Option<(i64, Option<u32>, Option<String>)> = tx
        .prepare_cached(
            "SELECT generation,failed_rank,failure_code FROM group_plans
              WHERE deployment_id=?1 AND instance_index=?2 ORDER BY generation DESC LIMIT 1",
        )?
        .query_row(rusqlite::params![deployment_id, index], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let Some((generation, failed_rank, failure_code)) = newest else {
        return Ok(None);
    };
    let (plan, rows) = crate::groups::plan_at(tx, deployment_id, index, generation)
        .map_err(|_| SnapshotError::CorruptData)?
        .ok_or(SnapshotError::CorruptData)?;
    let revision: i64 = revision.parse().map_err(|_| SnapshotError::CorruptData)?;
    let residency_sql = format!(
        "SELECT {} FROM host_effective_revisions h
          WHERE h.deployment_id=?1 AND h.revision=?2 AND h.host_id=?3 AND h.outcome='resolved'",
        json_text("h.effective_json", "$.residency")
    );
    let mut members = Vec::with_capacity(rows.len());
    for (member, row) in plan.members().iter().zip(&rows) {
        if member.rank != row.rank || member.member.host_id != row.host_id {
            return Err(SnapshotError::CorruptData);
        }
        let charge: Option<String> = tx
            .prepare_cached("SELECT footprint_json FROM resource_owners WHERE owner_id=?1")?
            .query_row([&row.owner_id], |r| r.get(0))
            .optional()?;
        let reservation = charge
            .map(|json| {
                let footprint = crate::resource_ledger::decode(&json)
                    .map_err(|_| SnapshotError::CorruptData)?;
                let bytes = footprint
                    .allocations
                    .iter()
                    .try_fold(0_i64, |sum, a| sum.checked_add(a.bytes))
                    .ok_or(SnapshotError::CorruptData)?;
                Ok::<_, SnapshotError>(MemberReservationStatus {
                    phase: phase_name(footprint.phase),
                    bytes: bytes.to_string(),
                })
            })
            .transpose()?;
        let residency: Option<String> = tx
            .prepare_cached(&residency_sql)?
            .query_row(
                rusqlite::params![deployment_id, revision, row.host_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let failed = failed_rank == Some(row.rank);
        let uncertain = row.state == MemberState::Uncertain;
        let settled = row.state == MemberState::Settled;
        let processes = if settled {
            0
        } else {
            u32::try_from(row.identities.as_ref().map_or(0, Vec::len))
                .map_err(|_| SnapshotError::CorruptData)?
        };
        members.push(GroupMemberStatus {
            host: row.host_id.clone(),
            node_rank: row.rank,
            role: match member.role {
                MemberRole::Head => "head",
                MemberRole::Worker => "worker",
            },
            // ADR 0028 §11: an uncertain member keeps its charge, whatever
            // else is known of it; the failed rank reads failed otherwise.
            state: if uncertain {
                MemberState::Uncertain.as_str()
            } else if failed {
                "failed"
            } else {
                row.state.as_str()
            },
            processes,
            reservation,
            residency,
            last_error: if failed {
                Some(
                    failure_code
                        .clone()
                        .unwrap_or_else(|| "group_member_failed".into()),
                )
            } else if uncertain {
                Some("group_member_uncertain".into())
            } else {
                None
            },
        });
    }
    if members.len() != plan.members().len() {
        return Err(SnapshotError::CorruptData);
    }
    let topology = plan.topology();
    Ok(Some(GroupStatus {
        engine: plan.engine().as_str(),
        topology: GroupTopologyStatus {
            tensor_parallel: topology.tensor_parallel,
            pipeline_parallel: topology.pipeline_parallel,
        },
        rendezvous: std::net::SocketAddr::new(plan.head().peer_address, plan.rendezvous_port())
            .to_string(),
        peer_transport: "unauthenticated",
        plan_generation: plan.generation().to_string(),
        members,
    }))
}

fn phase_name(phase: ResourcePhase) -> &'static str {
    match phase {
        ResourcePhase::Cold => "cold",
        ResourcePhase::Ready => "ready",
        ResourcePhase::Parking => "parking",
        ResourcePhase::Parked => "parked",
        ResourcePhase::Wake => "wake",
    }
}

/// ADR 0013 §6: a deployment's observed state from its instances. `ready`
/// while any instance is Ready (`degraded` says the rest); `failed` only when
/// every active instance failed; otherwise the most pressing state any active
/// instance is in. `None` leaves the deployment's own derivation (no instance
/// has anything more to say than `stopped` or `parked`).
fn aggregate_state(instances: &[InstanceSnapshot]) -> Option<&'static str> {
    let states: Vec<&str> = instances
        .iter()
        .filter(|i| i.lifecycle == "active" || i.observed_state != "stopped")
        .map(|i| i.observed_state.as_str())
        .collect();
    if states.is_empty() {
        return None;
    }
    if states.contains(&"ready") {
        return Some("ready");
    }
    // W5: a wake or a park in flight is reported like a start or a stop.
    for state in [
        "uncertain",
        "waking",
        "starting",
        "queued",
        "parking",
        "stopping",
        "reconciling",
    ] {
        if states.contains(&state) {
            return Some(state);
        }
    }
    if states.iter().all(|state| *state == "failed") {
        return Some("failed");
    }
    None
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

#[cfg(test)]
mod capacity_tests {
    use crate::Store;

    // SPEC §§10, 17 (owner decision 2026-10-08): the load read lists every
    // deployment or the one named, each with its instances as status derives
    // them; an ID that names none reads empty; a revision that does not decode
    // has no derived limit rather than a guess.
    #[test]
    fn the_load_read_names_deployments_and_their_instances() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                r#"INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES('d','chat','model','ready','ready',1,1,0,5,1,1);
                INSERT INTO effective_revisions VALUES('d',1,'{}','f');
                INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json) VALUES('d',1,2,'{"hosts":null,"selector":{},"strategy":"spread","max_per_host":null}');
                INSERT INTO deployment_instances(deployment_id,instance_index) VALUES('d',1);
                UPDATE deployment_instances SET revision=1,generation=4,desired_state='ready',observed_state='ready',admission_enabled=1,dispatch_enabled=1 WHERE deployment_id='d' AND instance_index=0;
                INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('e','other','model','stopped',0,0,1,1);"#,
            )
            .unwrap();
        let all = store.capacity(None).unwrap();
        assert_eq!(
            all.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["d", "e"]
        );
        let one = store.capacity(Some("d")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "chat");
        assert_eq!(one[0].max_running, None);
        let instances: Vec<_> = one[0]
            .instances
            .iter()
            .map(|i| (i.index, i.generation, i.observed_state.as_str()))
            .collect();
        assert_eq!(instances, [(0, Some(4), "ready"), (1, None, "stopped")]);
        assert!(store.capacity(Some("missing")).unwrap().is_empty());
    }
}
