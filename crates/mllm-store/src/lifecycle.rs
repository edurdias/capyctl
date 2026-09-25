use std::net::TcpListener;

pub(crate) mod completion;

use mllm_domain::completion::ProcessIdentity;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::dispatch::CoordinatorSession;
use crate::instances::fence_instance;

const MAX_DTO_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAction {
    Activate,
    Park,
    Stop,
    Prepare,
    Reconcile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedRun {
    pub operation_id: String,
    pub joined: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SequencePlan {
    version: u32,
    steps: Vec<PlanAction>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanAction {
    deployment_id: String,
    action: RunAction,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPlan {
    version: u32,
    steps: Vec<PlanAction>,
    cleanup_target: Option<String>,
    handoffs: Vec<HandoffHistory>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffHistory {
    predecessor_operation_id: String,
    claims: Vec<ClaimLink>,
    steps: Vec<StepLink>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimLink {
    deployment_id: String,
    revision: i64,
    generation: i64,
    current_generation: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepLink {
    id: String,
    deployment_id: String,
    state: String,
}

fn bounded_json<T: Serialize>(value: &T) -> Result<String, LifecycleError> {
    let json = serde_json::to_string(value).map_err(|_| LifecycleError::Invalid)?;
    if json.len() > MAX_DTO_BYTES {
        return Err(LifecycleError::Invalid);
    }
    Ok(json)
}

fn sorted_members(members: &[DeploymentFence]) -> Result<Vec<DeploymentFence>, LifecycleError> {
    if members.is_empty() || members.len() > 1024 {
        return Err(LifecycleError::Invalid);
    }
    let mut sorted = members.to_vec();
    sorted.sort_by(|a, b| a.deployment_id.cmp(&b.deployment_id));
    for pair in sorted.windows(2) {
        if pair[0].deployment_id == pair[1].deployment_id && pair[0] != pair[1] {
            return Err(LifecycleError::Stale);
        }
    }
    sorted.dedup();
    Ok(sorted)
}

struct RunRecord {
    target: DeploymentFence,
    session_id: String,
    action: String,
    state: String,
    plan_json: String,
}

fn run_record(tx: &Transaction<'_>, id: &str) -> Result<RunRecord, LifecycleError> {
    tx.query_row("SELECT deployment_id,revision,generation,session_id,action,state,plan_json FROM lifecycle_runs WHERE operation_id=?1",[id],|r|Ok(RunRecord {
        target: DeploymentFence { deployment_id:r.get(0)?,revision:r.get(1)?,generation:r.get(2)? },session_id:r.get(3)?,action:r.get(4)?,state:r.get(5)?,plan_json:r.get(6)?
    })).optional()?.ok_or(LifecycleError::Conflict)
}

fn current_run(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    id: &str,
) -> Result<RunRecord, LifecycleError> {
    let run = run_record(tx, id)?;
    fenced(tx, session, &run.target)?;
    if run.session_id != session.id() {
        return Err(LifecycleError::Stale);
    }
    if !matches!(run.state.as_str(), "queued" | "running" | "uncertain") {
        return Err(LifecycleError::Conflict);
    }
    Ok(run)
}

fn insert_run(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    target: &DeploymentFence,
    deadline_ms: i64,
    action: &str,
) -> Result<AcceptedRun, LifecycleError> {
    if deadline_ms <= 0 {
        return Err(LifecycleError::Invalid);
    }
    let id = ulid::Ulid::new().to_string();
    tx.execute(
        "INSERT INTO operations(id,deployment_id,kind,state) VALUES (?1,?2,?3,'pending')",
        params![id, target.deployment_id, action],
    )?;
    let plan = bounded_json(&StoredPlan {
        version: 1,
        steps: vec![],
        cleanup_target: if action == "stop" {
            Some(target.deployment_id.clone())
        } else {
            None
        },
        handoffs: vec![],
    })?;
    tx.execute("INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES (?1,?2,?3,?4,?5,?6,'queued',?7,?8,?9)", params![id,target.deployment_id,target.revision,target.generation,session.id(),action,deadline_ms,plan,fence_instance(tx,target)?])?;
    Ok(AcceptedRun {
        operation_id: id,
        joined: false,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentFence {
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
}

/// What a persisted arm returned. Only `New` permits a send; `AlreadyRecorded` is a
/// replay and carries no authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArmResult {
    New { step_id: String },
    AlreadyRecorded,
}

pub(crate) fn insert_initialize_run(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    target: &DeploymentFence,
    operation_id: &str,
    deadline_ms: i64,
) -> Result<(), LifecycleError> {
    fenced(tx, session, target)?;
    let plan = bounded_json(&StoredPlan {
        version: 1,
        steps: vec![PlanAction {
            deployment_id: target.deployment_id.clone(),
            action: RunAction::Activate,
        }],
        cleanup_target: None,
        handoffs: vec![],
    })?;
    tx.execute("INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES(?1,?2,?3,?4,?5,'activate','queued',?6,?7,?8)",
        params![operation_id,target.deployment_id,target.revision,target.generation,session.id(),deadline_ms,plan,fence_instance(tx,target)?])?;
    Ok(())
}

pub(crate) fn insert_owned_cleanup_run(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    target: &DeploymentFence,
    operation: &str,
    deadline: i64,
    kind: &str,
) -> Result<(), LifecycleError> {
    if !matches!(kind, "ordinary_cleanup" | "ordinary_unarmed_stop") {
        return Err(LifecycleError::Invalid);
    }
    fenced(tx, session, target)?;
    let plan = bounded_json(&StoredPlan {
        version: 1,
        steps: vec![PlanAction {
            deployment_id: target.deployment_id.clone(),
            action: RunAction::Stop,
        }],
        cleanup_target: Some(target.deployment_id.clone()),
        handoffs: vec![],
    })?;
    tx.execute(
        "INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,?3,'pending')",
        params![operation, target.deployment_id, kind],
    )?;
    tx.execute("INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES(?1,?2,?3,?4,?5,'stop','queued',?6,?7,?8)",params![operation,target.deployment_id,target.revision,target.generation,session.id(),deadline,plan,fence_instance(tx,target)?])?;
    Ok(())
}

pub(crate) fn validate_cleanup_run(
    tx: &Transaction<'_>,
    target: &DeploymentFence,
    operation: &str,
    session: &str,
    deadline: i64,
    predecessor: Option<&str>,
) -> Result<String, LifecycleError> {
    let run = run_record(tx, operation)?;
    let plan: StoredPlan = completion::decode(&run.plan_json)?;
    let bound: i64 = tx.query_row(
        "SELECT deadline_ms FROM lifecycle_runs WHERE operation_id=?1",
        [operation],
        |r| r.get(0),
    )?;
    if run.target != *target
        || run.session_id != session
        || run.action != "stop"
        || bound != deadline
        || plan.version != 1
        || plan.steps.len() != 1
        || plan.steps[0].deployment_id != target.deployment_id
        || plan.steps[0].action != RunAction::Stop
        || plan.cleanup_target.as_deref() != Some(&target.deployment_id)
        || plan.handoffs.len() != usize::from(predecessor.is_some())
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    if let Some(id) = predecessor {
        let h = &plan.handoffs[0];
        let previous = run_record(tx, id)?;
        if h.predecessor_operation_id != id
            || h.claims.len() != 1
            || h.claims[0].deployment_id != target.deployment_id
            || h.claims[0].revision != target.revision
            || h.claims[0].generation >= target.generation
            || h.claims[0].generation != previous.target.generation
            || previous.target.deployment_id != target.deployment_id
            || previous.target.revision != target.revision
            || h.claims[0].current_generation != target.generation
            || h.steps.len() != 1
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        let rows:Vec<(String,String)>=tx.prepare("SELECT id,deployment_id FROM lifecycle_steps WHERE operation_id=?1 ORDER BY ordinal")?.query_map([id],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
        if rows.len() != h.steps.len() {
            return Err(LifecycleError::CorruptStoredData);
        }
        for ((step, dep), history) in rows.iter().zip(&h.steps) {
            if step != &history.id
                || dep != &target.deployment_id
                || dep != &history.deployment_id
                || !matches!(history.state.as_str(), "planned" | "armed" | "uncertain")
            {
                return Err(LifecycleError::CorruptStoredData);
            }
        }
    }
    Ok(run.state)
}

/// Only the immutable single-member plan; caller fences current session separately.
pub(crate) fn validate_initialize_run(
    tx: &Transaction<'_>,
    target: &DeploymentFence,
    operation_id: &str,
    session_id: &str,
    deadline_ms: i64,
) -> Result<String, LifecycleError> {
    let run = run_record(tx, operation_id)?;
    if run.plan_json.len() > MAX_DTO_BYTES {
        return Err(LifecycleError::Invalid);
    }
    let plan: StoredPlan =
        serde_json::from_str(&run.plan_json).map_err(|_| LifecycleError::Invalid)?;
    let deadline: i64 = tx.query_row(
        "SELECT deadline_ms FROM lifecycle_runs WHERE operation_id=?1",
        [operation_id],
        |r| r.get(0),
    )?;
    if run.target != *target
        || run.session_id != session_id
        || deadline != deadline_ms
        || run.action != "activate"
        || !matches!(
            run.state.as_str(),
            "queued" | "running" | "uncertain" | "succeeded" | "failed"
        )
        || plan.version != 1
        || plan.steps.len() != 1
        || plan.steps[0].deployment_id != target.deployment_id
        || plan.steps[0].action != RunAction::Activate
        || plan.cleanup_target.is_some()
        || !plan.handoffs.is_empty()
    {
        return Err(LifecycleError::Invalid);
    }
    Ok(run.state)
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("deployment not found")]
    NotFound,
    #[error("expected revision does not match")]
    RevisionConflict,
    #[error("idempotency key identifies a different command")]
    IdempotencyConflict,
    #[error("retained runtime requires cleanup")]
    RuntimeRetained,
    #[error("host policy denies lifecycle action")]
    HostPolicyDenied,
    #[error("endpoint capacity exhausted")]
    CapacityBlocked,
    #[error("lifecycle command queue full")]
    QueueFull,
    #[error("resource policy requires reconciliation")]
    ReconciliationRequired,
    #[error("unsupported lifecycle step or backend validation")]
    Unsupported,
    #[error("corrupt stored lifecycle data")]
    CorruptStoredData,
    #[error("store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("stale coordinator or deployment fence")]
    Stale,
    #[error("lifecycle conflict")]
    Conflict,
    #[error("activation disabled")]
    Disabled,
    #[error("invalid lifecycle input")]
    Invalid,
    #[error("resource or evidence check failed: {0}")]
    Rejected(String),
    /// ADR 0014 §7 (WE3): the revision's resources derive from a checkpoint
    /// digest a host has not measured yet; activation waits for it.
    #[error("checkpoint digest pending")]
    CheckpointDigestPending,
    /// ADR 0014 §7: the checkpoint measured to a digest other than the declared
    /// or recorded one, or its weights do not resolve the revision.
    #[error("checkpoint does not match its recorded digest")]
    CheckpointMismatch,
    /// ADR 0008: the revision's declared remote model source is not yet
    /// materialized on any host; activation waits for it.
    #[error("model source pending")]
    ModelSourcePending,
    /// ADR 0008: every host's materialization failed with a reason the same
    /// declaration will meet again (hash mismatch, size over the host's
    /// limit, policy refusal); a new revision is needed.
    #[error("model source failed")]
    ModelSourceFailed,
    /// Owner decision 2026-09-23: an unmeasured model whose startup estimate
    /// exceeds the host's managed limit starts only alone on its host; another
    /// engine holds a charge there. `start --evict` (or a request, through the
    /// switching rules) empties the host first.
    #[error("startup requires an empty host")]
    StartupRequiresEmptyHost,
    /// Owner decision 2026-09-25: nothing could be placed because every
    /// allowed host that resolved the revision is ineligible for placement now
    /// (drain-only after version skew, draining, revoked, offline or not yet
    /// reconciled). Not a capacity refusal: releasing memory would not help.
    #[error("every allowed host is ineligible for placement")]
    HostIneligible,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveBinding {
    pub id: String,
    pub fence: DeploymentFence,
    pub incarnation: String,
    pub identity_id: String,
    pub ownership: String,
    pub endpoint_host: String,
    pub endpoint_port: u16,
    pub credential_ref: String,
    pub binding_payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRuntimeBinding {
    pub id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub incarnation: String,
    pub identity_id: String,
    pub ownership: String,
    pub endpoint: String,
    pub credential_ref: String,
    pub identities: Vec<ProcessIdentity>,
    pub state: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingDto {
    pub(crate) version: u32,
    pub(crate) identity_id: String,
    pub(crate) endpoint: String,
    pub(crate) credential_ref: String,
    pub(crate) payload: String,
}

pub(crate) enum DecodedBinding {
    V1,
}

/// ADR 0011: the only binding an mllm store writes is version 1. Anything else is
/// not a binding this store produced.
pub(crate) fn decode_binding(json: &str) -> Result<DecodedBinding, LifecycleError> {
    if json.len() > MAX_DTO_BYTES {
        return Err(LifecycleError::Invalid);
    }
    let binding: BindingDto = serde_json::from_str(json).map_err(|_| LifecycleError::Invalid)?;
    if binding.version != 1 {
        return Err(LifecycleError::Invalid);
    }
    Ok(DecodedBinding::V1)
}

pub(crate) struct PreparedBinding {
    /// Held while the reservation commits so no local process takes the port
    /// meanwhile. `None` for an endpoint on a remote host: the controller's own
    /// ports say nothing about that host's, and its agent checks its port itself.
    _listener: Option<TcpListener>,
    id: String,
    fence: DeploymentFence,
    incarnation: String,
    ownership: String,
    host: String,
    port: u16,
    json: String,
}

impl PreparedBinding {
    /// Prepare a binding for the host its instance is placed on. SPEC §3 / T24:
    /// the endpoint is test-bound here only when that host is this machine; a
    /// remote host's port is leased from its own range and checked by its agent,
    /// so a port busy on the controller must not refuse it.
    pub(crate) fn prepare_for_host(
        tx: &Transaction<'_>,
        request: &ReserveBinding,
    ) -> Result<Self, LifecycleError> {
        let local = endpoint_is_local(tx, &request.fence)?;
        Self::prepare(request, local)
    }

    fn prepare(request: &ReserveBinding, local: bool) -> Result<Self, LifecycleError> {
        if !valid_text(&request.id)
            || !valid_text(&request.fence.deployment_id)
            || request.fence.revision < 1
            || request.fence.generation < 1
            || !valid_text(&request.incarnation)
            || !valid_text(&request.identity_id)
            || !matches!(request.ownership.as_str(), "managed" | "attached")
            || request.endpoint_host != "127.0.0.1"
            || request.endpoint_port == 0
            || !valid_text(&request.credential_ref)
            || !valid_text(&request.binding_payload)
        {
            return Err(LifecycleError::Invalid);
        }
        let listener = if local {
            Some(
                TcpListener::bind((&*request.endpoint_host, request.endpoint_port))
                    .map_err(|_| LifecycleError::Conflict)?,
            )
        } else {
            None
        };
        let json = serde_json::to_string(&BindingDto {
            version: 1,
            identity_id: request.identity_id.clone(),
            endpoint: format!("{}:{}", request.endpoint_host, request.endpoint_port),
            credential_ref: request.credential_ref.clone(),
            payload: request.binding_payload.clone(),
        })
        .map_err(|_| LifecycleError::Invalid)?;
        if json.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::Invalid);
        }
        Ok(Self {
            _listener: listener,
            id: request.id.clone(),
            fence: request.fence.clone(),
            incarnation: request.incarnation.clone(),
            ownership: request.ownership.clone(),
            host: request.endpoint_host.clone(),
            port: request.endpoint_port,
            json,
        })
    }
}

pub(crate) fn insert_prepared_binding(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    prepared: &PreparedBinding,
) -> Result<(), LifecycleError> {
    crate::dispatch::check_session(tx, session).map_err(|error| match error {
        crate::dispatch::DispatchError::Sql(error) => LifecycleError::Sql(error),
        _ => LifecycleError::Stale,
    })?;
    // ADR 0013 §5: the binding belongs to the instance whose current fence it is.
    let instance = fence_instance(tx, &prepared.fence)?;
    tx.execute("INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state,instance_index) VALUES(?1,?2,?3,?4,?5,?6,'[]','reserved',?7)",
        params![prepared.id, prepared.fence.deployment_id, prepared.fence.revision, prepared.incarnation, prepared.ownership, prepared.json, instance])?;
    // SPEC §3 (v21): a port lease belongs to the host the instance is placed on.
    let host_id = endpoint_host_key(tx, &prepared.fence)?;
    tx.execute(
        "INSERT INTO endpoint_leases(host_id,host,port,binding_id) VALUES(?1,?2,?3,?4)",
        params![host_id, prepared.host, prepared.port, prepared.id],
    )?;
    Ok(())
}

/// Whether this incarnation's endpoint is on the controller's machine: true for
/// the embedded host (and the F1 path, which has none), false for an enrolled
/// remote host.
pub(crate) fn endpoint_is_local(
    tx: &Transaction<'_>,
    fence: &DeploymentFence,
) -> Result<bool, LifecycleError> {
    let host = endpoint_host_key(tx, fence)?;
    if host.is_empty() {
        return Ok(true);
    }
    let remote: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1)
             OR EXISTS(SELECT 1 FROM host_resource_namespaces WHERE host_id=?1 AND kind='remote')",
        [&host],
        |r| r.get(0),
    )?;
    Ok(!remote)
}

/// The host an endpoint lease of this incarnation is keyed under: the host the
/// scheduler placed its instance on (ADR 0013 §4), else the effective
/// revision's host (`$.host.name`), or the empty key for a deployment with no
/// effective revision (the F1 path). Schema v21 derives existing leases the
/// same way, so allocation and migration agree.
pub(crate) fn endpoint_host_key(
    tx: &Transaction<'_>,
    fence: &DeploymentFence,
) -> Result<String, LifecycleError> {
    Ok(tx.query_row(
        "SELECT COALESCE((SELECT host_id FROM deployment_instances WHERE deployment_id=?1 AND generation=?3),
                         (SELECT json_extract(effective_json,'$.host.name') FROM effective_revisions WHERE deployment_id=?1 AND revision=?2),'')",
        params![fence.deployment_id, fence.revision, fence.generation],
        |r| r.get(0),
    )?)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdentityDto {
    role: String,
    pid: u32,
    boot_id: String,
    start_ticks: u64,
}

fn valid_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_DTO_BYTES
}

fn fenced(
    transaction: &Transaction<'_>,
    session: &CoordinatorSession,
    fence: &DeploymentFence,
) -> Result<(), LifecycleError> {
    crate::dispatch::check_session(transaction, session).map_err(|_| LifecycleError::Stale)?;
    // ADR 0013 §5: a fence is current while its instance still carries it.
    fence_instance(transaction, fence)?;
    Ok(())
}

impl crate::Store {
    /// Acquires every member atomically. Input describes intent, never evidence or authority.
    pub fn claim_sequence(
        &self,
        session: &CoordinatorSession,
        operation_id: &str,
        members: &[DeploymentFence],
        plan_json: &str,
    ) -> Result<(), LifecycleError> {
        if plan_json.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::Invalid);
        }
        let plan: SequencePlan =
            serde_json::from_str(plan_json).map_err(|_| LifecycleError::Invalid)?;
        if plan.version != 1 || plan.steps.len() > 1024 {
            return Err(LifecycleError::Invalid);
        }
        let members = sorted_members(members)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let run = current_run(&tx, session, operation_id)?;
        if run.state != "queued" {
            return Err(LifecycleError::Conflict);
        }
        for member in &members {
            fenced(&tx, session, member)?;
        }
        if !members.contains(&run.target) {
            return Err(LifecycleError::Invalid);
        }
        for step in &plan.steps {
            if !members
                .iter()
                .any(|m| m.deployment_id == step.deployment_id)
                || (step.action == RunAction::Stop
                    && (run.action != "stop" || step.deployment_id != run.target.deployment_id))
            {
                return Err(LifecycleError::Invalid);
            }
        }
        let mut stored: StoredPlan =
            serde_json::from_str(&run.plan_json).map_err(|_| LifecycleError::Invalid)?;
        if stored.version != 1 || !stored.handoffs.is_empty() {
            return Err(LifecycleError::Conflict);
        }
        let existing: i64 = tx.query_row(
            "SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?1",
            [operation_id],
            |r| r.get(0),
        )?;
        if existing != 0 {
            return Err(LifecycleError::Conflict);
        }
        stored.steps = plan.steps;
        let json = bounded_json(&stored)?;
        for member in &members {
            tx.execute("INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation) VALUES (?1,?2,?3,?4)",params![member.deployment_id,operation_id,member.revision,member.generation]).map_err(|error|match error {
                rusqlite::Error::SqliteFailure(ref e,_) if e.code==rusqlite::ErrorCode::ConstraintViolation => LifecycleError::Conflict,
                other=>LifecycleError::Sql(other),
            })?;
        }
        tx.execute(
            "UPDATE lifecycle_runs SET plan_json=?1 WHERE operation_id=?2",
            params![json, operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Fences admission immediately; retained work requires a separate claim handoff.
    pub fn fence_stop(
        &self,
        session: &CoordinatorSession,
        target: &DeploymentFence,
        deadline_ms: i64,
    ) -> Result<AcceptedRun, LifecycleError> {
        self.fence_lifecycle(session, target, deadline_ms, false)
    }

    /// Suspension creates reconciliation intent with no cleanup authority.
    pub fn fence_suspend(
        &self,
        session: &CoordinatorSession,
        target: &DeploymentFence,
        deadline_ms: i64,
    ) -> Result<AcceptedRun, LifecycleError> {
        self.fence_lifecycle(session, target, deadline_ms, true)
    }

    fn fence_lifecycle(
        &self,
        session: &CoordinatorSession,
        target: &DeploymentFence,
        deadline_ms: i64,
        suspend: bool,
    ) -> Result<AcceptedRun, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let next = Self::fence_lifecycle_in_transaction(&tx, session, target, suspend)?;
        let run = insert_run(
            &tx,
            session,
            &next,
            deadline_ms,
            if suspend { "reconcile" } else { "stop" },
        )?;
        tx.commit()?;
        Ok(run)
    }

    pub(crate) fn fence_lifecycle_in_transaction(
        tx: &Transaction<'_>,
        session: &CoordinatorSession,
        target: &DeploymentFence,
        suspend: bool,
    ) -> Result<DeploymentFence, LifecycleError> {
        fenced(tx, session, target)?;
        // ADR 0013 §5: the next generation comes from the deployment's one
        // counter and moves only the fenced instance; its siblings keep theirs.
        let generation = crate::instances::draw_generation(tx, &target.deployment_id)?;
        let sql = if suspend {
            "UPDATE deployment_instances SET generation=?1,dispatch_enabled=0 WHERE deployment_id=?2 AND generation IN (?3,?1)"
        } else {
            "UPDATE deployment_instances SET generation=?1,dispatch_enabled=0,desired_state='stopped' WHERE deployment_id=?2 AND generation IN (?3,?1)"
        };
        if tx.execute(sql, params![generation, target.deployment_id, target.generation])? != 1 {
            return Err(LifecycleError::Stale);
        }
        if suspend {
            tx.execute(
                "UPDATE deployments SET suspended=1 WHERE id=?1",
                [&target.deployment_id],
            )?;
        }
        tx.execute(
            "INSERT INTO generation_history(deployment_id,generation) VALUES (?1,?2)",
            params![target.deployment_id, generation],
        )?;
        // W10 gap (a): the replaced incarnation's closure reasons are dead.
        crate::switch_state::prune_closures(tx, &target.deployment_id)?;
        let next = DeploymentFence {
            generation,
            ..target.clone()
        };
        Ok(next)
    }

    /// Transfers reconciliation responsibility, preserving predecessor rows and effects.
    /// This grants no send/release permission. Workers must await old command task exit
    /// (and hold the lifetime lock after restart) before considering any cleanup.
    pub fn handoff_claims(
        &self,
        session: &CoordinatorSession,
        successor_operation_id: &str,
        predecessor_operation_id: &str,
        members: &[DeploymentFence],
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        Self::handoff_claims_in_transaction(
            &tx,
            session,
            successor_operation_id,
            predecessor_operation_id,
            members,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn handoff_claims_in_transaction(
        tx: &Transaction<'_>,
        session: &CoordinatorSession,
        successor_operation_id: &str,
        predecessor_operation_id: &str,
        members: &[DeploymentFence],
    ) -> Result<(), LifecycleError> {
        if successor_operation_id == predecessor_operation_id {
            return Err(LifecycleError::Invalid);
        }
        let members = sorted_members(members)?;
        let successor = current_run(tx, session, successor_operation_id)?;
        if !matches!(successor.action.as_str(), "stop" | "reconcile") {
            return Err(LifecycleError::Conflict);
        }
        for member in &members {
            fenced(tx, session, member)?;
        }
        if !members.contains(&successor.target) {
            return Err(LifecycleError::Conflict);
        }
        let predecessor = run_record(tx, predecessor_operation_id)?;
        if !matches!(
            predecessor.state.as_str(),
            "queued" | "running" | "uncertain"
        ) {
            return Err(LifecycleError::Conflict);
        }
        let mut claims:Vec<ClaimLink>=tx.prepare("SELECT deployment_id,revision,generation FROM lifecycle_claims WHERE operation_id=?1 ORDER BY deployment_id")?.query_map([predecessor_operation_id],|r|Ok(ClaimLink { deployment_id:r.get(0)?,revision:r.get(1)?,generation:r.get(2)?,current_generation:r.get(2)? }))?.collect::<Result<_,_>>()?;
        if claims.len() != members.len()
            || claims.iter().zip(&members).any(|(c, m)| {
                c.deployment_id != m.deployment_id
                    || c.revision != m.revision
                    || c.generation > m.generation
            })
        {
            return Err(LifecycleError::Conflict);
        }
        let steps:Vec<StepLink>=tx.prepare("SELECT id,deployment_id,state FROM lifecycle_steps WHERE operation_id=?1 ORDER BY ordinal")?.query_map([predecessor_operation_id],|r|Ok(StepLink {id:r.get(0)?,deployment_id:r.get(1)?,state:r.get(2)?}))?.collect::<Result<_,_>>()?;
        let mut plan: StoredPlan =
            serde_json::from_str(&successor.plan_json).map_err(|_| LifecycleError::Invalid)?;
        if plan.version != 1 {
            return Err(LifecycleError::Invalid);
        }
        for (claim, member) in claims.iter_mut().zip(&members) {
            // ADR 0013 §5: a new generation is drawn from the deployment's
            // counter and moves only the member's own instance.
            let generation = if claim.generation == member.generation {
                crate::instances::draw_generation(tx, &member.deployment_id)?
            } else {
                member.generation
            };
            claim.current_generation = generation;
            if tx.execute(
                "UPDATE deployment_instances SET generation=?1,dispatch_enabled=0 WHERE deployment_id=?2 AND generation IN (?3,?1)",
                params![generation, member.deployment_id, member.generation],
            )? != 1
            {
                return Err(LifecycleError::Stale);
            }
            if generation != member.generation {
                tx.execute(
                    "INSERT INTO generation_history(deployment_id,generation) VALUES (?1,?2)",
                    params![member.deployment_id, generation],
                )?;
            }
            if member.deployment_id == successor.target.deployment_id {
                tx.execute(
                    "UPDATE lifecycle_runs SET generation=?1 WHERE operation_id=?2",
                    params![generation, successor_operation_id],
                )?;
            }
            tx.execute("UPDATE lifecycle_claims SET operation_id=?1,revision=?2,generation=?3 WHERE deployment_id=?4 AND operation_id=?5",params![successor_operation_id,member.revision,generation,member.deployment_id,predecessor_operation_id])?;
        }
        plan.handoffs.push(HandoffHistory {
            predecessor_operation_id: predecessor_operation_id.into(),
            claims,
            steps,
        });
        let json = bounded_json(&plan)?;
        tx.execute(
            "UPDATE lifecycle_runs SET plan_json=?1 WHERE operation_id=?2",
            params![json, successor_operation_id],
        )?;
        Ok(())
    }
    /// Joins retained work without extending its deadline or granting dispatch authority.
    pub fn accept_activation(
        &self,
        session: &CoordinatorSession,
        target: &DeploymentFence,
        deadline_ms: i64,
    ) -> Result<AcceptedRun, LifecycleError> {
        self.accept_activation_inner(session, target, deadline_ms, false)
    }

    /// Administrative start: callers must authorize administration before invoking this method.
    pub fn accept_administrative_start(
        &self,
        session: &CoordinatorSession,
        target: &DeploymentFence,
        deadline_ms: i64,
    ) -> Result<AcceptedRun, LifecycleError> {
        self.accept_activation_inner(session, target, deadline_ms, true)
    }

    fn accept_activation_inner(
        &self,
        session: &CoordinatorSession,
        target: &DeploymentFence,
        deadline_ms: i64,
        start: bool,
    ) -> Result<AcceptedRun, LifecycleError> {
        if deadline_ms <= 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        fenced(&tx, session, target)?;
        let (admission, suspended, desired): (bool, bool, String) = tx.query_row(
            "SELECT admission_enabled,suspended,desired_state FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3",
            params![target.deployment_id, target.revision, target.generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if !admission || suspended || (!start && desired == "stopped") {
            return Err(LifecycleError::Disabled);
        }
        if start {
            tx.execute(
                "UPDATE deployment_instances SET desired_state='ready' WHERE deployment_id=?1 AND generation=?2",
                params![target.deployment_id, target.generation],
            )?;
        }
        let existing: Option<String> = tx.query_row("SELECT operation_id FROM lifecycle_runs WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND action='activate' AND state IN ('queued','running','uncertain')",params![target.deployment_id,target.revision,target.generation], |r| r.get(0)).optional()?;
        let run = match existing {
            Some(operation_id) => AcceptedRun {
                operation_id,
                joined: true,
            },
            None => insert_run(&tx, session, target, deadline_ms, "activate")?,
        };
        tx.commit()?;
        Ok(run)
    }

    /// Durably consumes the one spawn attempt before process construction.
    pub fn arm_runtime_spawn(
        &self,
        session: &CoordinatorSession,
        fence: &DeploymentFence,
        binding_id: &str,
        incarnation: &str,
    ) -> Result<(), LifecycleError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        fenced(&transaction, session, fence)?;
        let changed = transaction.execute(
            "UPDATE runtime_bindings SET state='uncertain'
             WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND incarnation=?4 AND state='reserved'",
            params![binding_id, fence.deployment_id, fence.revision, incarnation],
        )?;
        if changed != 1 {
            return Err(LifecycleError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomically retains immutable binding metadata and its endpoint accounting lease.
    pub fn reserve_runtime_binding(
        &self,
        session: &CoordinatorSession,
        request: &ReserveBinding,
    ) -> Result<(), LifecycleError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let prepared = PreparedBinding::prepare_for_host(&transaction, request)?;
        insert_prepared_binding(&transaction, session, &prepared).map_err(|error| match error {
            LifecycleError::Sql(rusqlite::Error::SqliteFailure(_, _)) => LifecycleError::Conflict,
            other => other,
        })?;
        transaction.commit()?;
        drop(prepared);
        Ok(())
    }

    /// Records API membership only; it never establishes complete worker ownership.
    pub fn record_api_identity(
        &self,
        session: &CoordinatorSession,
        fence: &DeploymentFence,
        binding_id: &str,
        identity: &ProcessIdentity,
    ) -> Result<(), LifecycleError> {
        if identity.role != "api"
            || identity.pid == 0
            || identity.boot_id.is_empty()
            || identity.start_ticks == 0
        {
            return Err(LifecycleError::Invalid);
        }
        let dto = [IdentityDto {
            role: identity.role.clone(),
            pid: identity.pid,
            boot_id: identity.boot_id.clone(),
            start_ticks: identity.start_ticks,
        }];
        let json = serde_json::to_string(&dto).map_err(|_| LifecycleError::Invalid)?;
        if json.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::Invalid);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        fenced(&transaction, session, fence)?;
        let row:Option<(String,String,String)>=transaction.query_row("SELECT ownership,binding_json,identities_json FROM runtime_bindings WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND state!='released'",params![binding_id,fence.deployment_id,fence.revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (ownership, binding, existing) = row.ok_or(LifecycleError::Conflict)?;
        if ownership != "managed" {
            return Err(LifecycleError::Conflict);
        }
        decode_binding(&binding).map_err(|_| LifecycleError::CorruptStoredData)?;
        if existing.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::CorruptStoredData);
        }
        let members: Vec<IdentityDto> =
            serde_json::from_str(&existing).map_err(|_| LifecycleError::CorruptStoredData)?;
        let mut roles = std::collections::BTreeSet::new();
        let mut processes = std::collections::BTreeSet::new();
        if members.iter().any(|m| {
            m.role.trim().is_empty()
                || m.pid == 0
                || m.start_ticks == 0
                || m.boot_id.trim().is_empty()
                || m.boot_id != members[0].boot_id
                || !roles.insert(&m.role)
                || !processes.insert(m.pid)
        }) {
            return Err(LifecycleError::CorruptStoredData);
        }
        if !members.is_empty() {
            if !members.iter().any(|m| m.role == "api") {
                return Err(LifecycleError::CorruptStoredData);
            }
            return if members.contains(&dto[0]) {
                Ok(())
            } else {
                Err(LifecycleError::Conflict)
            };
        }
        let changed = transaction.execute(
            "UPDATE runtime_bindings SET identities_json=?1,state='uncertain'
             WHERE id=?2 AND deployment_id=?3 AND revision=?4 AND state!='released'",
            params![json, binding_id, fence.deployment_id, fence.revision],
        )?;
        if changed != 1 {
            return Err(LifecycleError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }

    /// The retained binding of the deployment's lowest-index instance that
    /// holds one. ADR 0013 §5: a deployment with several running instances has
    /// one binding each; callers acting on one launch use
    /// [`Self::retained_binding`] with its binding id.
    pub fn runtime_binding(
        &self,
        deployment_id: &str,
    ) -> Result<Option<StoredRuntimeBinding>, LifecycleError> {
        self.retained_binding_where("deployment_id=?1 ORDER BY instance_index LIMIT 1", deployment_id)
    }

    /// One retained (not released) binding, by its id.
    pub fn retained_binding(
        &self,
        binding_id: &str,
    ) -> Result<Option<StoredRuntimeBinding>, LifecycleError> {
        self.retained_binding_where("id=?1", binding_id)
    }

    /// ADR 0013 §10 (the router's instance choice is I3): the binding the
    /// router forwards to, which is the one a request lease without a fence is
    /// charged to — the lowest-index instance whose gate is open, else the
    /// lowest-index instance holding a binding (whose closed gate the caller
    /// reports). Returns the binding with its instance's dispatch gate.
    pub fn serving_binding(
        &self,
        deployment_id: &str,
    ) -> Result<Option<(StoredRuntimeBinding, bool)>, LifecycleError> {
        let chosen: Option<(String, bool)> = self
            .conn
            .query_row(
                "SELECT b.id,(i.observed_state='ready' AND i.admission_enabled=1 AND i.dispatch_enabled=1)
                   FROM runtime_bindings b JOIN deployment_instances i
                     ON i.deployment_id=b.deployment_id AND i.instance_index=b.instance_index
                  WHERE b.deployment_id=?1 AND b.state!='released'
                  ORDER BY (i.observed_state='ready' AND i.admission_enabled=1 AND i.dispatch_enabled=1) DESC, b.instance_index
                  LIMIT 1",
                [deployment_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, open)) = chosen else {
            return Ok(None);
        };
        Ok(self.retained_binding(&id)?.map(|binding| (binding, open)))
    }

    fn retained_binding_where(
        &self,
        predicate: &str,
        key: &str,
    ) -> Result<Option<StoredRuntimeBinding>, LifecycleError> {
        type Row = (String, String, i64, String, String, String, String, String);
        let row: Option<Row> = self
            .conn
            .query_row(
                &format!(
                    "SELECT id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state
                 FROM runtime_bindings WHERE state!='released' AND {predicate}"
                ),
                [key],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            id,
            deployment_id,
            revision,
            incarnation,
            ownership,
            binding_json,
            identities_json,
            state,
        )) = row
        else {
            return Ok(None);
        };
        if binding_json.len() > MAX_DTO_BYTES || identities_json.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::Invalid);
        }
        if !matches!(decode_binding(&binding_json)?, DecodedBinding::V1) {
            return Err(LifecycleError::Invalid);
        }
        let binding: BindingDto =
            serde_json::from_str(&binding_json).map_err(|_| LifecycleError::Invalid)?;
        if binding.version != 1 {
            return Err(LifecycleError::Invalid);
        }
        let identity_dtos: Vec<IdentityDto> =
            serde_json::from_str(&identities_json).map_err(|_| LifecycleError::Invalid)?;
        let identities = identity_dtos
            .into_iter()
            .map(|identity| ProcessIdentity {
                role: identity.role,
                pid: identity.pid,
                boot_id: identity.boot_id,
                start_ticks: identity.start_ticks,
            })
            .collect();
        Ok(Some(StoredRuntimeBinding {
            id,
            deployment_id,
            revision,
            incarnation,
            identity_id: binding.identity_id,
            ownership,
            endpoint: binding.endpoint,
            credential_ref: binding.credential_ref,
            identities,
            state,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcceptDeployment, Store};
    use mllm_domain::{DeploymentId, LifecycleState, OperationId};

    fn fence(store: &Store, name: &str) -> DeploymentFence {
        DeploymentFence {
            deployment_id: accepted(store, name),
            revision: 1,
            generation: 1,
        }
    }

    const EMPTY_PLAN: &str = r#"{"version":1,"steps":[]}"#;

    #[test]
    fn sequence_uncertain_join_cannot_acquire_fresh_execution_plan() {
        let store = Store::open_in_memory().unwrap();
        let target = fence(&store, "uncertain");
        let session = store.begin_coordinator_session().unwrap();
        let run = store
            .accept_administrative_start(&session, &target, 100)
            .unwrap();
        store
            .conn
            .execute("UPDATE lifecycle_runs SET state='uncertain'", [])
            .unwrap();
        let joined = store.accept_activation(&session, &target, 200).unwrap();
        assert!(joined.joined);
        assert!(matches!(
            store.claim_sequence(&session, &run.operation_id, &[target], EMPTY_PLAN),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM lifecycle_claims", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn sequence_conflict_rolls_back_every_claim_and_rejects_stale_or_forged_plan() {
        let store = Store::open_in_memory().unwrap();
        let a = fence(&store, "a");
        let b = fence(&store, "b");
        let c = fence(&store, "c");
        let session = store.begin_coordinator_session().unwrap();
        let first = store
            .accept_administrative_start(&session, &a, 100)
            .unwrap();
        let second = store
            .accept_administrative_start(&session, &b, 100)
            .unwrap();
        store
            .claim_sequence(
                &session,
                &first.operation_id,
                &[a.clone(), c.clone(), a.clone()],
                EMPTY_PLAN,
            )
            .unwrap();
        assert!(matches!(
            store.claim_sequence(
                &session,
                &second.operation_id,
                &[b.clone(), c.clone()],
                EMPTY_PLAN
            ),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?1",
                    [&second.operation_id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let mut stale = b.clone();
        stale.revision += 1;
        assert!(matches!(
            store.claim_sequence(&session, &second.operation_id, &[stale], EMPTY_PLAN),
            Err(LifecycleError::Stale)
        ));
        for plan in [
            r#"{"version":2,"steps":[]}"#.to_string(),
            r#"{"version":1,"steps":[],"evidence":"owned"}"#.to_string(),
            format!(
                r#"{{"version":1,"steps":[{{"deployment_id":"{}","action":"stop"}}]}}"#,
                b.deployment_id
            ),
            " ".repeat(MAX_DTO_BYTES + 1),
        ] {
            assert!(matches!(
                store.claim_sequence(
                    &session,
                    &second.operation_id,
                    std::slice::from_ref(&b),
                    &plan
                ),
                Err(LifecycleError::Invalid)
            ));
        }
        let _new = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.claim_sequence(&session, &second.operation_id, &[b], EMPTY_PLAN),
            Err(LifecycleError::Stale)
        ));
    }

    #[test]
    fn handoff_stop_during_restore_retains_effects_and_restart_fences() {
        let store = Store::open_in_memory().unwrap();
        let a = fence(&store, "restore");
        let b = fence(&store, "victim");
        let session = store.begin_coordinator_session().unwrap();
        let old = store
            .accept_administrative_start(&session, &a, 100)
            .unwrap();
        store
            .claim_sequence(
                &session,
                &old.operation_id,
                &[a.clone(), b.clone()],
                EMPTY_PLAN,
            )
            .unwrap();
        store.conn.execute("INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding',?1,1,'incarnation','managed','{}','[]','uncertain')",[&a.deployment_id]).unwrap();
        store.conn.execute("INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('other-binding',?1,1,'other-incarnation','managed','{}','[]','reserved')",[&b.deployment_id]).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO endpoint_leases(host_id,host,port,binding_id) VALUES ('','127.0.0.1',1,'binding')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('lease',?1,1,1,?2,'inflight')",
                params![a.deployment_id, session.id()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO resource_grants VALUES ('grant',?1,?2,'{}',1)",
                params![a.deployment_id, old.operation_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO owners VALUES ('owner','deployment',?1)",
                [&a.deployment_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO reservations VALUES ('owner','memory',4096,'activation','[]')",
                [],
            )
            .unwrap();
        store.conn.execute("INSERT INTO lifecycle_steps VALUES ('restore-step',?1,0,?2,'binding',?3,'armed','{\"action\":\"restore\"}','grant')",params![old.operation_id,a.deployment_id,session.id()]).unwrap();
        let stop = store.fence_stop(&session, &a, 200).unwrap();
        let mut current = a.clone();
        current.generation += 1;
        let forged = format!(
            r#"{{"version":1,"steps":[{{"deployment_id":"{}","action":"stop"}}]}}"#,
            b.deployment_id
        );
        assert!(matches!(
            store.claim_sequence(
                &session,
                &stop.operation_id,
                &[current.clone(), b.clone()],
                &forged
            ),
            Err(LifecycleError::Invalid)
        ));
        let fake_identity = r#"{"version":1,"steps":[],"cleanup_target":"victim","identities":[{"role":"worker","pid":42}]}"#;
        assert!(matches!(
            store.claim_sequence(
                &session,
                &stop.operation_id,
                &[current.clone(), b.clone()],
                fake_identity
            ),
            Err(LifecycleError::Invalid)
        ));
        assert!(matches!(
            store.handoff_claims(
                &session,
                &stop.operation_id,
                &old.operation_id,
                &[a.clone(), b.clone()]
            ),
            Err(LifecycleError::Stale)
        ));
        assert!(matches!(
            store.handoff_claims(
                &session,
                &stop.operation_id,
                &old.operation_id,
                std::slice::from_ref(&current)
            ),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?1",
                    [&old.operation_id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        store
            .handoff_claims(
                &session,
                &stop.operation_id,
                &old.operation_id,
                &[current.clone(), b.clone()],
            )
            .unwrap();
        assert!(matches!(
            store.record_api_identity(
                &session,
                &b,
                "other-binding",
                &ProcessIdentity {
                    role: "api".into(),
                    pid: 42,
                    boot_id: "boot".into(),
                    start_ticks: 1
                }
            ),
            Err(LifecycleError::Stale)
        ));
        assert!(matches!(
            store.arm_runtime_spawn(&session, &b, "other-binding", "other-incarnation"),
            Err(LifecycleError::Stale)
        ));
        let b = DeploymentFence { generation: 2, ..b };
        let json: String = store
            .conn
            .query_row(
                "SELECT plan_json FROM lifecycle_runs WHERE operation_id=?1",
                [&stop.operation_id],
                |r| r.get(0),
            )
            .unwrap();
        let history: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            history["handoffs"][0]["predecessor_operation_id"],
            old.operation_id
        );
        assert_eq!(history["handoffs"][0]["steps"][0]["id"], "restore-step");
        assert_eq!(history["cleanup_target"], a.deployment_id);
        assert!(matches!(
            store.arm_runtime_spawn(&session, &a, "binding", "incarnation"),
            Err(LifecycleError::Stale)
        ));
        assert!(matches!(
            store.claim_sequence(
                &session,
                &old.operation_id,
                std::slice::from_ref(&b),
                EMPTY_PLAN
            ),
            Err(LifecycleError::Stale)
        ));
        let restart = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.handoff_claims(
                &session,
                &stop.operation_id,
                &old.operation_id,
                &[current.clone(), b.clone()]
            ),
            Err(LifecycleError::Stale)
        ));
        let reconcile = store.fence_suspend(&restart, &current, 300).unwrap();
        current.generation += 1;
        store
            .handoff_claims(
                &restart,
                &reconcile.operation_id,
                &stop.operation_id,
                &[current, b],
            )
            .unwrap();
        assert_eq!(store.generation_history(&a.deployment_id).unwrap().len(), 2);
        for table in [
            "endpoint_leases",
            "request_leases",
            "lifecycle_steps",
            "resource_grants",
            "reservations",
        ] {
            assert_eq!(
                store
                    .conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM runtime_bindings", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT state FROM lifecycle_steps", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "uncertain"
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT state FROM runtime_bindings WHERE id='binding'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "uncertain"
        );
        let restart_plan: String = store
            .conn
            .query_row(
                "SELECT plan_json FROM lifecycle_runs WHERE operation_id=?1",
                [&reconcile.operation_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&restart_plan).unwrap()["cleanup_target"]
                .is_null()
        );
        assert_eq!(store.conn.query_row("SELECT desired_state,dispatch_enabled,suspended,current_generation FROM deployments WHERE id=?1",[&a.deployment_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?))).unwrap(),("stopped".into(),0,1,3));
    }

    #[test]
    fn sequence_opposing_connections_have_one_winner_and_no_partial_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        let store = Store::open(&path).unwrap();
        let a = fence(&store, "a");
        let b = fence(&store, "b");
        let session = store.begin_coordinator_session().unwrap();
        let first = store
            .accept_administrative_start(&session, &a, 100)
            .unwrap();
        let second = store
            .accept_administrative_start(&session, &b, 100)
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [(first, [a.clone(), b.clone()]), (second, [b, a])]
            .into_iter()
            .map(|(run, members)| {
                let path = path.clone();
                let session = session.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = Store::open(&path).unwrap();
                    barrier.wait();
                    (
                        run.operation_id.clone(),
                        store.claim_sequence(&session, &run.operation_id, &members, EMPTY_PLAN),
                    )
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.1.is_ok()).count(), 1);
        for (id, result) in results {
            let count = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?1",
                    [id],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap();
            if result.is_ok() {
                assert_eq!(count, 2);
            } else {
                assert!(matches!(result, Err(LifecycleError::Conflict)));
                assert_eq!(count, 0);
            }
        }
    }

    #[test]
    fn activation_concurrent_acceptance_joins_without_extending_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        let store = Store::open(&path).unwrap();
        let target = fence(&store, "concurrent");
        store
            .conn
            .execute("UPDATE deployments SET desired_state='ready'", [])
            .unwrap();
        let session = store.begin_coordinator_session().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [100, 200]
            .into_iter()
            .map(|deadline| {
                let path = path.clone();
                let target = target.clone();
                let session = session.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = Store::open(&path).unwrap();
                    barrier.wait();
                    (
                        deadline,
                        store
                            .accept_activation(&session, &target, deadline)
                            .unwrap(),
                    )
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results[0].1.operation_id, results[1].1.operation_id);
        assert_eq!(results.iter().filter(|r| !r.1.joined).count(), 1);
        let original = results.iter().find(|r| !r.1.joined).unwrap().0;
        assert_eq!(
            store
                .conn
                .query_row("SELECT deadline_ms FROM lifecycle_runs", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            original
        );
    }

    #[test]
    fn activation_admission_start_and_uncertain_join_are_fenced() {
        let store = Store::open_in_memory().unwrap();
        let target = fence(&store, "activation");
        let session = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.accept_activation(&session, &target, 100),
            Err(LifecycleError::Disabled)
        ));
        let run = store
            .accept_administrative_start(&session, &target, 100)
            .unwrap();
        store
            .conn
            .execute("UPDATE lifecycle_runs SET state='uncertain'", [])
            .unwrap();
        assert_eq!(
            store.accept_activation(&session, &target, 200).unwrap(),
            AcceptedRun {
                operation_id: run.operation_id,
                joined: true
            }
        );
        store
            .conn
            .execute("UPDATE deployments SET suspended=1", [])
            .unwrap();
        assert!(matches!(
            store.accept_administrative_start(&session, &target, 100),
            Err(LifecycleError::Disabled)
        ));
        store
            .conn
            .execute(
                "UPDATE deployments SET suspended=0, admission_enabled=0",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.accept_administrative_start(&session, &target, 100),
            Err(LifecycleError::Disabled)
        ));
        let _next = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.accept_administrative_start(&session, &target, 100),
            Err(LifecycleError::Stale)
        ));
    }

    fn accepted(store: &Store, name: &str) -> String {
        let id = DeploymentId::new();
        store
            .accept_deployment(AcceptDeployment {
                id,
                name: name.into(),
                kind: "model".into(),
                route_model_id: None,
                desired_state: LifecycleState::Stopped,
                schema_version: 1,
                initial_operation_id: OperationId(format!("operation-{name}")),
                idempotency_key: format!("idempotency-{name}"),
            })
            .unwrap();
        id.to_string()
    }

    #[test]
    fn binding_and_endpoint_are_reserved_atomically_under_fences() {
        let store = Store::open_in_memory().unwrap();
        let deployment_a = accepted(&store, "deployment-a");
        let deployment_b = accepted(&store, "deployment-b");
        let session = store.begin_coordinator_session().unwrap();
        let reserve = |deployment: &str, port| ReserveBinding {
            id: format!("binding-{deployment}"),
            fence: DeploymentFence {
                deployment_id: deployment.into(),
                revision: 1,
                generation: 1,
            },
            incarnation: format!("incarnation-{deployment}"),
            identity_id: "qualified".into(),
            ownership: "managed".into(),
            endpoint_host: "127.0.0.1".into(),
            endpoint_port: port,
            credential_ref: format!("credential-{deployment}"),
            binding_payload: "recipe-reference".into(),
        };
        let listeners: Vec<_> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let ports: Vec<_> = listeners
            .iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect();
        drop(listeners);
        store
            .reserve_runtime_binding(&session, &reserve(&deployment_a, ports[0]))
            .unwrap();
        store
            .reserve_runtime_binding(&session, &reserve(&deployment_b, ports[1]))
            .unwrap();
        let a = store.runtime_binding(&deployment_a).unwrap().unwrap();
        let b = store.runtime_binding(&deployment_b).unwrap().unwrap();
        assert_ne!(a.id, b.id);
        assert_ne!(a.endpoint, b.endpoint);
        assert_ne!(a.credential_ref, b.credential_ref);

        let mut stale = reserve(&deployment_a, ports[2]);
        stale.fence.revision = 2;
        assert!(store.reserve_runtime_binding(&session, &stale).is_err());
        assert_eq!(store.runtime_binding(&deployment_a).unwrap().unwrap(), a);
    }

    // T24: a remote host's endpoint port is that host's; a port busy on the
    // controller's machine must not refuse it, while a local endpoint still
    // proves its port is free before the reservation commits.
    #[test]
    fn a_remote_endpoint_is_not_test_bound_on_the_controller() {
        let store = Store::open_in_memory().unwrap();
        let remote = accepted(&store, "remote-endpoint");
        let local = accepted(&store, "local-endpoint");
        store
            .conn
            .execute_batch(&format!(
                "INSERT INTO enrolled_hosts VALUES('spark-remote','spark','key',0);
                 INSERT OR IGNORE INTO deployment_instances(deployment_id,instance_index) VALUES('{remote}',0);
                 UPDATE deployment_instances SET host_id='spark-remote',generation=1 WHERE deployment_id='{remote}';"
            ))
            .unwrap();
        let session = store.begin_coordinator_session().unwrap();
        // Held for the whole test: the port is busy on this machine.
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let reserve = |deployment: &str| ReserveBinding {
            id: format!("binding-{deployment}"),
            fence: DeploymentFence {
                deployment_id: deployment.into(),
                revision: 1,
                generation: 1,
            },
            incarnation: format!("incarnation-{deployment}"),
            identity_id: "qualified".into(),
            ownership: "managed".into(),
            endpoint_host: "127.0.0.1".into(),
            endpoint_port: port,
            credential_ref: format!("credential-{deployment}"),
            binding_payload: "recipe-reference".into(),
        };
        assert!(matches!(
            store.reserve_runtime_binding(&session, &reserve(&local)),
            Err(LifecycleError::Conflict)
        ));
        store
            .reserve_runtime_binding(&session, &reserve(&remote))
            .unwrap();
        let binding = store.runtime_binding(&remote).unwrap().unwrap();
        assert_eq!(binding.endpoint, format!("127.0.0.1:{port}"));
        let lease_host: String = store
            .conn
            .query_row(
                "SELECT host_id FROM endpoint_leases WHERE binding_id=?1",
                [&binding.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(lease_host, "spark-remote");
        drop(busy);
    }

    #[test]
    fn incomplete_identity_and_consumed_spawn_attempt_retain_accounting() {
        let store = Store::open_in_memory().unwrap();
        let deployment = accepted(&store, "uncertain-runtime");
        let session = store.begin_coordinator_session().unwrap();
        let fence = DeploymentFence {
            deployment_id: deployment.clone(),
            revision: 1,
            generation: 1,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        store
            .reserve_runtime_binding(
                &session,
                &ReserveBinding {
                    id: "binding-uncertain".into(),
                    fence: fence.clone(),
                    incarnation: "incarnation-uncertain".into(),
                    identity_id: "qualified".into(),
                    ownership: "managed".into(),
                    endpoint_host: "127.0.0.1".into(),
                    endpoint_port: port,
                    credential_ref: "credential-reference".into(),
                    binding_payload: "recipe-reference".into(),
                },
            )
            .unwrap();
        store
            .arm_runtime_spawn(
                &session,
                &fence,
                "binding-uncertain",
                "incarnation-uncertain",
            )
            .unwrap();
        assert!(matches!(
            store.arm_runtime_spawn(
                &session,
                &fence,
                "binding-uncertain",
                "incarnation-uncertain",
            ),
            Err(LifecycleError::Conflict)
        ));
        store
            .record_api_identity(
                &session,
                &fence,
                "binding-uncertain",
                &ProcessIdentity {
                    role: "api".into(),
                    pid: 42,
                    boot_id: "boot".into(),
                    start_ticks: 7,
                },
            )
            .unwrap();
        let retained = store.runtime_binding(&deployment).unwrap().unwrap();
        assert_eq!(retained.state, "uncertain");
        assert_eq!(retained.identities.len(), 1);
        let api = retained.identities[0].clone();
        let mut changed_api = api.clone();
        changed_api.start_ticks += 1;
        assert!(matches!(
            store.record_api_identity(&session, &fence, "binding-uncertain", &changed_api),
            Err(LifecycleError::Conflict)
        ));
        store.conn.execute("UPDATE runtime_bindings SET identities_json='[{\"role\":\"api\",\"pid\":42,\"boot_id\":\"boot\",\"start_ticks\":7},{\"role\":\"worker-0\",\"pid\":43,\"boot_id\":\"boot\",\"start_ticks\":8}]' WHERE id='binding-uncertain'",[]).unwrap();
        store
            .record_api_identity(&session, &fence, "binding-uncertain", &api)
            .unwrap();
        assert_eq!(
            store
                .runtime_binding(&deployment)
                .unwrap()
                .unwrap()
                .identities
                .len(),
            2
        );
        let leases: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM endpoint_leases WHERE binding_id='binding-uncertain'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leases, 1);

        let _new_session = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.record_api_identity(
                &session,
                &fence,
                "binding-uncertain",
                &ProcessIdentity {
                    role: "api".into(),
                    pid: 43,
                    boot_id: "boot".into(),
                    start_ticks: 8,
                },
            ),
            Err(LifecycleError::Stale)
        ));
        let still_retained = store.runtime_binding(&deployment).unwrap().unwrap();
        assert_eq!(still_retained.endpoint, format!("127.0.0.1:{port}"));
        assert_eq!(still_retained.state, "uncertain");
    }
}
