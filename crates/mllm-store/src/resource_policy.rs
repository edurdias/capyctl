use crate::dispatch::{check_session, CoordinatorSession, DispatchError};
use crate::events::{append_event, EventMetadata, EventOperationId, EventWriteError};
use crate::resource_ledger::{read_snapshot, ResourceStoreError};
use crate::OpState;
use mllm_config::effective::{DomainPolicy, HostPolicy, PortRange, QueuePolicy, Sharing};
use mllm_config::resource_controls::{ResourceContext, ResourceControls};
use mllm_domain::resources::{LedgerSnapshot, MemoryObservation, ResourcePhase};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MAX_JSON_BYTES: usize = 1 << 20;
const MAX_IDENTIFIER_BYTES: usize = 256;
const UPDATE_METHOD: &str = "PUT";
const UPDATE_KIND: &str = "host_resource_policy_update";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePolicySnapshot {
    pub context: ResourceContext,
    pub controls: ResourceControls,
    pub revision: i64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePolicyImport {
    pub context: ResourceContext,
    pub controls: ResourceControls,
    pub revision: i64,
    pub epoch: u64,
    pub changed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainOvercommit {
    pub managed_bytes: i64,
    pub host_kv_bytes: i64,
    pub parked_bytes: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceOvercommit {
    pub domains: BTreeMap<String, DomainOvercommit>,
    pub parked_owners: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePolicyUpdate {
    pub operation_id: String,
    pub host_id: String,
    pub revision: i64,
    pub epoch: u64,
    pub overcommit: ResourceOvercommit,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagementOperationTarget {
    Deployment { deployment_id: String },
    HostResourcePolicy { host_id: String, revision: i64 },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementOperation {
    pub id: String,
    pub kind: String,
    pub state: OpState,
    pub error_code: Option<String>,
    pub accepted_at: String,
    pub updated_at: String,
    pub target: ManagementOperationTarget,
}

#[derive(Debug, thiserror::Error)]
pub enum ResourcePolicyError {
    #[error("stale coordinator session")]
    StaleSession,
    #[error("invalid resource policy input")]
    Invalid,
    #[error("resource policy revision conflict")]
    RevisionConflict,
    #[error("idempotency conflict")]
    IdempotencyConflict,
    #[error("corrupt stored resource policy")]
    CorruptStoredPolicy,
    #[error("legacy reservations require reconciliation")]
    NeedsReconciliation,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPolicy {
    version: u8,
    host_id: String,
    revision: i64,
    context: StoredContext,
    controls: StoredControls,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredContext {
    host_id: String,
    domain_ids: BTreeSet<String>,
    device_domains: BTreeMap<String, String>,
    port_start: u16,
    port_end: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredControls {
    domains: BTreeMap<String, StoredDomain>,
    max_parked: u32,
    observation_ttl_ms: i64,
    planner_max_states: u32,
    queue: StoredQueue,
    device_sharing: String,
    device_sharing_overrides: BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDomain {
    managed_limit: i64,
    free_reserve: i64,
    host_kv_limit: Option<i64>,
    parked_limit: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredQueue {
    max_pending_per_deployment: u32,
    max_pending_total: u32,
    max_buffered_bytes_total: i64,
    request_deadline_ms: i64,
    admission_window_ms: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    version: u8,
    method: String,
    host_id: String,
    operation_id: String,
    revision: i64,
    epoch: u64,
    overcommit: ResourceOvercommit,
}
#[derive(Serialize)]
struct HashInput<'a> {
    version: u8,
    method: &'a str,
    target: (&'a str, &'a str),
    expected_revision: i64,
    controls: &'a StoredControls,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES
}
fn update_scope(host_id: &str) -> Result<String, ResourcePolicyError> {
    Ok(format!(
        "PUT /management/v1/hosts/{}/resource-policy",
        serde_json::to_string(host_id).map_err(|_| ResourcePolicyError::Invalid)?
    ))
}
fn sharing(value: Sharing) -> String {
    match value {
        Sharing::Shared => "shared",
        Sharing::Exclusive => "exclusive",
    }
    .into()
}
fn parse_sharing(value: &str) -> Result<Sharing, ResourcePolicyError> {
    match value {
        "shared" => Ok(Sharing::Shared),
        "exclusive" => Ok(Sharing::Exclusive),
        _ => Err(ResourcePolicyError::CorruptStoredPolicy),
    }
}

impl StoredContext {
    fn from_public(value: &ResourceContext) -> Self {
        Self {
            host_id: value.host_id.clone(),
            domain_ids: value.domain_ids.clone(),
            device_domains: value.device_domains.clone(),
            port_start: value.endpoint_port_range.start,
            port_end: value.endpoint_port_range.end,
        }
    }
    fn to_public(&self) -> ResourceContext {
        ResourceContext {
            host_id: self.host_id.clone(),
            domain_ids: self.domain_ids.clone(),
            device_domains: self.device_domains.clone(),
            endpoint_port_range: PortRange {
                start: self.port_start,
                end: self.port_end,
            },
        }
    }
}
impl StoredControls {
    fn from_public(value: &ResourceControls) -> Self {
        Self {
            domains: value
                .domains
                .iter()
                .map(|(id, d)| {
                    (
                        id.clone(),
                        StoredDomain {
                            managed_limit: d.managed_limit,
                            free_reserve: d.free_reserve,
                            host_kv_limit: d.host_kv_limit,
                            parked_limit: d.parked_limit,
                        },
                    )
                })
                .collect(),
            max_parked: value.max_parked,
            observation_ttl_ms: value.observation_ttl_ms,
            planner_max_states: value.planner_max_states,
            queue: StoredQueue {
                max_pending_per_deployment: value.queue.max_pending_per_deployment,
                max_pending_total: value.queue.max_pending_total,
                max_buffered_bytes_total: value.queue.max_buffered_bytes_total,
                request_deadline_ms: value.queue.request_deadline_ms,
                admission_window_ms: value.queue.admission_window_ms,
            },
            device_sharing: sharing(value.device_sharing),
            device_sharing_overrides: value
                .device_sharing_overrides
                .iter()
                .map(|(id, s)| (id.clone(), sharing(*s)))
                .collect(),
        }
    }
    fn to_public(&self) -> Result<ResourceControls, ResourcePolicyError> {
        Ok(ResourceControls {
            domains: self
                .domains
                .iter()
                .map(|(id, d)| {
                    (
                        id.clone(),
                        DomainPolicy {
                            managed_limit: d.managed_limit,
                            free_reserve: d.free_reserve,
                            host_kv_limit: d.host_kv_limit,
                            parked_limit: d.parked_limit,
                        },
                    )
                })
                .collect(),
            max_parked: self.max_parked,
            observation_ttl_ms: self.observation_ttl_ms,
            planner_max_states: self.planner_max_states,
            queue: QueuePolicy {
                max_pending_per_deployment: self.queue.max_pending_per_deployment,
                max_pending_total: self.queue.max_pending_total,
                max_buffered_bytes_total: self.queue.max_buffered_bytes_total,
                request_deadline_ms: self.queue.request_deadline_ms,
                admission_window_ms: self.queue.admission_window_ms,
            },
            device_sharing: parse_sharing(&self.device_sharing)?,
            device_sharing_overrides: self
                .device_sharing_overrides
                .iter()
                .map(|(id, s)| Ok((id.clone(), parse_sharing(s)?)))
                .collect::<Result<_, ResourcePolicyError>>()?,
        })
    }
}

fn map_session(error: DispatchError) -> ResourcePolicyError {
    match error {
        DispatchError::StaleSession => ResourcePolicyError::StaleSession,
        DispatchError::Sql(e) => ResourcePolicyError::Sql(e),
        _ => ResourcePolicyError::Invalid,
    }
}
fn map_event(error: EventWriteError) -> ResourcePolicyError {
    match error {
        EventWriteError::Sql(e) => ResourcePolicyError::Sql(e),
        _ => ResourcePolicyError::Invalid,
    }
}
fn map_ledger(error: ResourceStoreError) -> ResourcePolicyError {
    match error {
        ResourceStoreError::Sql(e) => ResourcePolicyError::Sql(e),
        ResourceStoreError::NeedsReconciliation => ResourcePolicyError::NeedsReconciliation,
        _ => ResourcePolicyError::CorruptStoredPolicy,
    }
}
fn validate_observations(
    context: &ResourceContext,
    controls: &ResourceControls,
    observations: &[MemoryObservation],
    now_ms: i64,
    ttl_ms: i64,
) -> Result<(), ResourcePolicyError> {
    if now_ms < 0 || ttl_ms <= 0 || observations.len() != context.domain_ids.len() {
        return Err(ResourcePolicyError::Invalid);
    }
    let mut seen = BTreeSet::new();
    for observation in observations {
        if !context.domain_ids.contains(&observation.domain)
            || !seen.insert(&observation.domain)
            || observation.capacity_bytes <= 0
            || observation.available_bytes < 0
            || observation.available_bytes > observation.capacity_bytes
            || observation.sampled_at_ms < 0
            || observation.sampled_at_ms > now_ms
            || now_ms
                .checked_sub(observation.sampled_at_ms)
                .filter(|age| *age <= ttl_ms)
                .is_none()
        {
            return Err(ResourcePolicyError::Invalid);
        }
        let domain = controls
            .domains
            .get(&observation.domain)
            .ok_or(ResourcePolicyError::Invalid)?;
        if domain
            .managed_limit
            .checked_add(domain.free_reserve)
            .filter(|sum| *sum <= observation.capacity_bytes)
            .is_none()
        {
            return Err(ResourcePolicyError::Invalid);
        }
    }
    Ok(())
}
fn encode_policy(value: &StoredPolicy) -> Result<String, ResourcePolicyError> {
    let json = serde_json::to_string(value).map_err(|_| ResourcePolicyError::Invalid)?;
    if json.len() > MAX_JSON_BYTES {
        return Err(ResourcePolicyError::Invalid);
    }
    Ok(json)
}
fn decode_policy(
    host: &str,
    column_revision: i64,
    json: &str,
) -> Result<ResourcePolicySnapshot, ResourcePolicyError> {
    if json.len() > MAX_JSON_BYTES {
        return Err(ResourcePolicyError::CorruptStoredPolicy);
    }
    let value: StoredPolicy =
        serde_json::from_str(json).map_err(|_| ResourcePolicyError::CorruptStoredPolicy)?;
    if value.version != 1
        || value.revision <= 0
        || value.revision != column_revision
        || value.host_id != host
        || value.context.host_id != host
        || !valid_id(host)
    {
        return Err(ResourcePolicyError::CorruptStoredPolicy);
    }
    let context = value.context.to_public();
    let controls = value.controls.to_public()?;
    controls
        .validate(&context)
        .map_err(|_| ResourcePolicyError::CorruptStoredPolicy)?;
    Ok(ResourcePolicySnapshot {
        context,
        controls,
        revision: value.revision,
    })
}
fn read_policy(
    tx: &Transaction<'_>,
    host: &str,
) -> Result<Option<ResourcePolicySnapshot>, ResourcePolicyError> {
    let row: Option<(i64, String)> = tx
        .query_row(
            "SELECT revision,policy_json FROM host_resource_policies WHERE host_id=?1",
            [host],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(revision, json)| decode_policy(host, revision, &json))
        .transpose()
}

pub(crate) fn read_singleton_policy(
    tx: &Transaction<'_>,
    host: &str,
) -> Result<Option<ResourcePolicySnapshot>, ResourcePolicyError> {
    let stored_hosts: Vec<String> = tx
        .prepare("SELECT host_id FROM host_resource_policies ORDER BY host_id LIMIT 2")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if stored_hosts.len() > 1 { return Err(ResourcePolicyError::CorruptStoredPolicy); }
    if stored_hosts.first().is_some_and(|stored| stored != host) {
        return Err(ResourcePolicyError::RevisionConflict);
    }
    read_policy(tx, host)
}
fn next_epoch(tx: &Transaction<'_>) -> Result<u64, ResourcePolicyError> {
    let epoch: i64 = tx.query_row(
        "SELECT epoch FROM resource_ledger_meta WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let next = epoch
        .checked_add(1)
        .filter(|v| *v > 0)
        .ok_or(ResourcePolicyError::Invalid)?;
    tx.execute(
        "UPDATE resource_ledger_meta SET epoch=?1 WHERE singleton=1",
        [next],
    )?;
    u64::try_from(next).map_err(|_| ResourcePolicyError::Invalid)
}
fn overcommit(
    snapshot: &LedgerSnapshot,
    controls: &ResourceControls,
) -> Result<ResourceOvercommit, ResourcePolicyError> {
    let mut totals: BTreeMap<String, (i64, i64, i64)> = controls
        .domains
        .keys()
        .map(|id| (id.clone(), (0, 0, 0)))
        .collect();
    let mut parked_owners = 0_u32;
    for footprint in snapshot.owners.values() {
        let parked = footprint.phase == ResourcePhase::Parked;
        if parked {
            parked_owners = parked_owners
                .checked_add(1)
                .ok_or(ResourcePolicyError::Invalid)?;
        }
        for allocation in &footprint.allocations {
            let total = totals
                .get_mut(&allocation.domain)
                .ok_or(ResourcePolicyError::CorruptStoredPolicy)?;
            total.0 = total
                .0
                .checked_add(allocation.bytes)
                .ok_or(ResourcePolicyError::Invalid)?;
            total.1 = total
                .1
                .checked_add(allocation.host_kv_bytes)
                .ok_or(ResourcePolicyError::Invalid)?;
            if parked {
                total.2 = total
                    .2
                    .checked_add(allocation.bytes)
                    .ok_or(ResourcePolicyError::Invalid)?;
            }
        }
    }
    let domains = totals
        .into_iter()
        .map(|(id, (managed, host_kv, parked))| {
            let policy = &controls.domains[&id];
            (
                id,
                DomainOvercommit {
                    managed_bytes: managed.saturating_sub(policy.managed_limit).max(0),
                    host_kv_bytes: policy
                        .host_kv_limit
                        .map_or(0, |limit| host_kv.saturating_sub(limit).max(0)),
                    parked_bytes: policy
                        .parked_limit
                        .map_or(0, |limit| parked.saturating_sub(limit).max(0)),
                },
            )
        })
        .collect();
    Ok(ResourceOvercommit {
        domains,
        parked_owners: parked_owners.saturating_sub(controls.max_parked),
    })
}

impl crate::Store {
    pub fn resource_policy(
        &self,
        host_id: &str,
    ) -> Result<Option<ResourcePolicySnapshot>, ResourcePolicyError> {
        if !valid_id(host_id) {
            return Err(ResourcePolicyError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let result = read_policy(&tx, host_id)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn import_resource_policy(
        &self,
        session: &CoordinatorSession,
        host: &HostPolicy,
        observations: &[MemoryObservation],
        now_ms: i64,
    ) -> Result<ResourcePolicyImport, ResourcePolicyError> {
        let context = ResourceContext::from_host(host);
        let controls = ResourceControls::from_host(host);
        if !valid_id(&context.host_id) {
            return Err(ResourcePolicyError::Invalid);
        }
        controls
            .validate(&context)
            .map_err(|_| ResourcePolicyError::Invalid)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(map_session)?;
        if let Some(current) = read_singleton_policy(&tx, &context.host_id)? {
            if current.context != context {
                return Err(ResourcePolicyError::RevisionConflict);
            }
            validate_observations(
                &current.context,
                &current.controls,
                observations,
                now_ms,
                current.controls.observation_ttl_ms,
            )?;
            let epoch = read_snapshot(&tx).map_err(map_ledger)?.epoch;
            tx.commit()?;
            return Ok(ResourcePolicyImport {
                context: current.context,
                controls: current.controls,
                revision: current.revision,
                epoch,
                changed: false,
            });
        }
        validate_observations(
            &context,
            &controls,
            observations,
            now_ms,
            controls.observation_ttl_ms,
        )?;
        let stored = StoredPolicy {
            version: 1,
            host_id: context.host_id.clone(),
            revision: 1,
            context: StoredContext::from_public(&context),
            controls: StoredControls::from_public(&controls),
        };
        let json = encode_policy(&stored)?;
        let epoch = next_epoch(&tx)?;
        tx.execute(
            "INSERT INTO host_resource_policies(host_id,revision,policy_json) VALUES(?1,1,?2)",
            params![context.host_id, json],
        )?;
        append_event(
            &tx,
            &EventMetadata::HostResourcePolicyBootstrapped {
                revision: 1,
                ledger_epoch: epoch,
                session_epoch: session.epoch(),
            },
        )
        .map_err(map_event)?;
        tx.commit()?;
        Ok(ResourcePolicyImport {
            context,
            controls,
            revision: 1,
            epoch,
            changed: true,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn update_resource_policy(
        &self,
        session: &CoordinatorSession,
        principal_id: &str,
        host_id: &str,
        expected_revision: i64,
        idempotency_key: &str,
        controls: &ResourceControls,
        observations: &[MemoryObservation],
        now_ms: i64,
    ) -> Result<ResourcePolicyUpdate, ResourcePolicyError> {
        if !valid_id(principal_id)
            || !valid_id(host_id)
            || !valid_id(idempotency_key)
            || expected_revision <= 0
        {
            return Err(ResourcePolicyError::Invalid);
        }
        let scope = update_scope(host_id)?;
        let stored_controls = StoredControls::from_public(controls);
        let hash_json = serde_json::to_vec(&HashInput {
            version: 1,
            method: UPDATE_METHOD,
            target: ("host", host_id),
            expected_revision,
            controls: &stored_controls,
        })
        .map_err(|_| ResourcePolicyError::Invalid)?;
        let request_hash = format!("{:x}", Sha256::digest(hash_json));
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(map_session)?;
        let receipt: Option<(String, String, String)> = tx.query_row("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3", params![principal_id, scope, idempotency_key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        if let Some((prior_hash, operation_id, json)) = receipt {
            if prior_hash != request_hash {
                return Err(ResourcePolicyError::IdempotencyConflict);
            }
            let stored = decode_receipt(&json)?;
            if stored.operation_id != operation_id
                || stored.host_id != host_id
                || stored.method != UPDATE_METHOD
            {
                return Err(ResourcePolicyError::CorruptStoredPolicy);
            }
            tx.commit()?;
            return Ok(stored.into());
        }
        let current = read_policy(&tx, host_id)?.ok_or(ResourcePolicyError::RevisionConflict)?;
        if current.revision != expected_revision {
            return Err(ResourcePolicyError::RevisionConflict);
        }
        controls
            .validate(&current.context)
            .map_err(|_| ResourcePolicyError::Invalid)?;
        validate_observations(
            &current.context,
            controls,
            observations,
            now_ms,
            current.controls.observation_ttl_ms,
        )?;
        let revision = expected_revision
            .checked_add(1)
            .filter(|v| *v > 0)
            .ok_or(ResourcePolicyError::Invalid)?;
        let snapshot = read_snapshot(&tx).map_err(map_ledger)?;
        let report = overcommit(&snapshot, controls)?;
        let epoch = next_epoch(&tx)?;
        let generated_operation_id = EventOperationId::generated(ulid::Ulid::new());
        let operation_id = generated_operation_id.as_str().to_owned();
        let stored = StoredPolicy {
            version: 1,
            host_id: host_id.into(),
            revision,
            context: StoredContext::from_public(&current.context),
            controls: stored_controls,
        };
        tx.execute(
            "UPDATE host_resource_policies SET revision=?2,policy_json=?3 WHERE host_id=?1",
            params![host_id, revision, encode_policy(&stored)?],
        )?;
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state,error_code,idempotency_key) VALUES(?1,NULL,?2,'succeeded',NULL,NULL)", params![operation_id, UPDATE_KIND])?;
        let receipt = StoredReceipt {
            version: 1,
            method: UPDATE_METHOD.into(),
            host_id: host_id.into(),
            operation_id: operation_id.clone(),
            revision,
            epoch,
            overcommit: report.clone(),
        };
        let response_json =
            serde_json::to_string(&receipt).map_err(|_| ResourcePolicyError::Invalid)?;
        if response_json.len() > MAX_JSON_BYTES {
            return Err(ResourcePolicyError::Invalid);
        }
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)", params![principal_id, scope, idempotency_key, request_hash, operation_id, response_json])?;
        append_event(
            &tx,
            &EventMetadata::HostResourcePolicyUpdated {
                operation_id: generated_operation_id,
                previous_revision: expected_revision,
                current_revision: revision,
                ledger_epoch: epoch,
                session_epoch: session.epoch(),
            },
        )
        .map_err(map_event)?;
        tx.commit()?;
        Ok(ResourcePolicyUpdate {
            operation_id,
            host_id: host_id.into(),
            revision,
            epoch,
            overcommit: report,
        })
    }
    pub fn get_management_operation(
        &self,
        id: &str,
    ) -> Result<Option<ManagementOperation>, ResourcePolicyError> {
        if !valid_id(id) {
            return Err(ResourcePolicyError::Invalid);
        }
        type Row = (
            String,
            Option<String>,
            String,
            String,
            Option<String>,
            String,
            String,
        );
        let row: Option<Row> = self.conn.query_row("SELECT id,deployment_id,kind,state,error_code,accepted_at,updated_at FROM operations WHERE id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        let Some((id, deployment_id, kind, state, error_code, accepted_at, updated_at)) = row
        else {
            return Ok(None);
        };
        let state = OpState::parse(&state).map_err(|_| ResourcePolicyError::CorruptStoredPolicy)?;
        let target = if let Some(deployment_id) = deployment_id {
            if kind == UPDATE_KIND {
                return Err(ResourcePolicyError::CorruptStoredPolicy);
            }
            ManagementOperationTarget::Deployment { deployment_id }
        } else if kind == UPDATE_KIND {
            let receipts: Vec<(String, String)> = self
                .conn
                .prepare("SELECT command_scope,response_json FROM command_receipts WHERE operation_id=?1")?
                .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            if receipts.len() != 1 {
                return Err(ResourcePolicyError::CorruptStoredPolicy);
            }
            let (scope, json) = &receipts[0];
            let receipt = decode_receipt(json)?;
            if receipt.operation_id != id
                || receipt.method != UPDATE_METHOD
                || *scope != update_scope(&receipt.host_id)?
            {
                return Err(ResourcePolicyError::CorruptStoredPolicy);
            }
            ManagementOperationTarget::HostResourcePolicy {
                host_id: receipt.host_id,
                revision: receipt.revision,
            }
        } else {
            return Err(ResourcePolicyError::CorruptStoredPolicy);
        };
        Ok(Some(ManagementOperation {
            id,
            kind,
            state,
            error_code,
            accepted_at,
            updated_at,
            target,
        }))
    }
}
fn decode_receipt(json: &str) -> Result<StoredReceipt, ResourcePolicyError> {
    if json.len() > MAX_JSON_BYTES {
        return Err(ResourcePolicyError::CorruptStoredPolicy);
    }
    let value: StoredReceipt =
        serde_json::from_str(json).map_err(|_| ResourcePolicyError::CorruptStoredPolicy)?;
    if value.version != 1
        || value.method != UPDATE_METHOD
        || !valid_id(&value.host_id)
        || !valid_id(&value.operation_id)
        || value.revision <= 1
        || value.epoch == 0
        || value.epoch > i64::MAX as u64
        || ulid::Ulid::from_string(&value.operation_id).is_err()
        || value.overcommit.domains.iter().any(|(domain, overcommit)| {
            domain.is_empty()
                || overcommit.managed_bytes < 0
                || overcommit.host_kv_bytes < 0
                || overcommit.parked_bytes < 0
        })
    {
        return Err(ResourcePolicyError::CorruptStoredPolicy);
    }
    Ok(value)
}
impl From<StoredReceipt> for ResourcePolicyUpdate {
    fn from(value: StoredReceipt) -> Self {
        Self {
            operation_id: value.operation_id,
            host_id: value.host_id,
            revision: value.revision,
            epoch: value.epoch,
            overcommit: value.overcommit,
        }
    }
}

#[cfg(test)]
mod tests;
