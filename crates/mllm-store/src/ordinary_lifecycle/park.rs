//! SPEC §§6.1, 6.3, 6.5, 7.3, 9.1, 10, 13 (plan W5): park, restore and
//! preinitialize of one instance's retained launch, durably.
//!
//! A park or a restore is its own operation (`park` or `restore`) with one
//! lifecycle run, one claim and one persisted step on the instance's retained
//! binding. The instance keeps its placement, binding, generation and resource
//! owner throughout: a parked instance wakes on the host it parked on, under
//! the same fence, because it is the same engine (ADR 0013 §4, sticky parked
//! placement). Every transition is budgeted before resources increase
//! (SPEC §6.5, §7.3):
//!
//! | step    | arm (grant)                          | completion (evidence)          |
//! |---------|--------------------------------------|--------------------------------|
//! | park    | Ready → Parking peak (ready ∪ parking) | → Parked (`memory_released`)   |
//! | restore | Parked → Wake peak (parked ∪ wake)     | → Ready (four restore facts, model usable) |
//!
//! A park is accepted only from Ready; acceptance closes the instance's
//! dispatch (SPEC §10 drain), and the step arms only once no request lease of
//! the instance remains. A restore is accepted only from Parked; the instance's
//! dispatch opens again only on completed restore evidence that includes a
//! usable model (SPEC §6.1). A refusal before any effect (`Unsupported`, a typed
//! host refusal) settles the step at once and returns the footprint to where it
//! was; an effect whose outcome is unknown leaves the step `uncertain` with the
//! peak reservation, the claim and the closed gate retained until an operator
//! Stop proves the group gone (AGENTS.md: uncertainty retains accounting). An
//! uncertain step is never re-armed, so a collective is never repeated blindly
//! (SPEC §13.2, T20).
//!
//! The parked set is bounded (SPEC §6.5): a park that would exceed the host's
//! `max_parked` or parked residual budget, and a restore or cold start that
//! does not fit, first reclaims least-recently-parked instances on the same
//! host with ordinary stops (verified cleanup, on-demand eligibility kept).
//! Ready work is never evicted here; that is automatic switching (W10).
use super::cleanup::{instance_scope, StopCommand};
use super::reconcile::SCHEDULER_PRINCIPAL;
use super::*;
use crate::events::ResidencyTransition;
use mllm_config::effective::Residency;
use mllm_domain::completion::{EffectObservation, Milestone, ProcessIdentity};
use mllm_domain::resources::LedgerSnapshot;
use sha2::{Digest, Sha256};

/// Which residency change a step performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidencyKind {
    Park,
    Restore,
}

impl ResidencyKind {
    /// The `operations.kind` of this change.
    pub fn operation_kind(self) -> &'static str {
        match self {
            Self::Park => "park",
            Self::Restore => "restore",
        }
    }
    /// The lifecycle run action: a restore is an activation of the instance.
    fn run_action(self) -> &'static str {
        match self {
            Self::Park => "park",
            Self::Restore => "activate",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Self::Park => "park",
            Self::Restore => "wake",
        }
    }
    /// SPEC §§6.1, 9.1: exactly the facts completed evidence must carry.
    pub fn facts(self) -> &'static [Milestone] {
        match self {
            Self::Park => &[Milestone::MemoryReleased],
            Self::Restore => &[
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable,
            ],
        }
    }
    fn transition(self, stage: Stage) -> ResidencyTransition {
        use ResidencyTransition as T;
        match (self, stage) {
            (Self::Park, Stage::Accepted) => T::ParkAccepted,
            (Self::Park, Stage::Armed) => T::ParkArmed,
            (Self::Park, Stage::Completed) => T::Parked,
            (Self::Park, Stage::Refused) => T::ParkRefused,
            (Self::Park, Stage::Uncertain) => T::ParkUncertain,
            (Self::Restore, Stage::Accepted) => T::RestoreAccepted,
            (Self::Restore, Stage::Armed) => T::RestoreArmed,
            (Self::Restore, Stage::Completed) => T::Restored,
            (Self::Restore, Stage::Refused) => T::RestoreRefused,
            (Self::Restore, Stage::Uncertain) => T::RestoreUncertain,
            (_, Stage::Cancelled) => T::Cancelled,
        }
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Accepted,
    Armed,
    Completed,
    Refused,
    Uncertain,
    Cancelled,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResidencyPlan {
    version: u8,
    kind: ResidencyKind,
    operation_id: String,
    step_id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    instance_index: u32,
    session_id: String,
    binding_id: String,
    incarnation: String,
    /// The completed Initialize step whose launch this parks or restores.
    source_step_id: String,
    principal: String,
    accepted_at_ms: i64,
    deadline_ms: i64,
    execution: Option<ResidencyExecution>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResidencyExecution {
    issued_at_ms: i64,
    grant_id: String,
    expected_epoch: u64,
}

impl ResidencyPlan {
    fn fence(&self) -> DeploymentFence {
        DeploymentFence {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            generation: self.generation,
        }
    }
    fn owner(&self) -> String {
        crate::instances::instance_owner_id(&self.deployment_id, self.instance_index)
    }
    fn token(&self) -> TransitionToken {
        TransitionToken {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            generation: self.generation,
            operation_id: self.operation_id.clone(),
            step_id: self.step_id.clone(),
        }
    }
}

/// What a caller observes of an accepted park or restore. Observation only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidencyReceipt {
    pub operation_id: String,
    pub step_id: String,
    pub deployment_id: String,
    pub instance: u32,
    pub kind: ResidencyKind,
    pub revision: i64,
    pub generation: i64,
    pub accepted_at_ms: i64,
    pub deadline_ms: i64,
    pub joined: bool,
}

impl ResidencyReceipt {
    fn of(p: &ResidencyPlan, joined: bool) -> Self {
        Self {
            operation_id: p.operation_id.clone(),
            step_id: p.step_id.clone(),
            deployment_id: p.deployment_id.clone(),
            instance: p.instance_index,
            kind: p.kind,
            revision: p.revision,
            generation: p.generation,
            accepted_at_ms: p.accepted_at_ms,
            deadline_ms: p.deadline_ms,
            joined,
        }
    }
}

/// Which parked instances a wake addresses (owner decision Q5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeScope {
    /// On-demand activation: one parked instance the operator did not stop.
    OnDemand,
    /// `start deployment`: every parked instance.
    All,
    /// `start instance <n>`.
    Instance(u32),
}

/// One planned park or restore of this session, for the worker to arm.
/// Frozen input only; the arm alone grants permission to send.
#[derive(Clone, Debug)]
pub struct ResidencyWork {
    pub step_id: String,
    pub operation_id: String,
    pub deployment_id: String,
    pub instance: u32,
    pub kind: ResidencyKind,
    pub binding_id: String,
    pub incarnation: String,
    /// The host the instance is placed on, whose observations admit the arm.
    pub host: String,
    pub deadline_ms: i64,
    pub limits: Vec<MemoryLimit>,
    pub observation_ttl_ms: i64,
    pub max_parked: usize,
}

/// The outcome of arming a planned park or restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResidencyArm {
    /// Send this command exactly once.
    New(Box<StepExecutionContext>),
    /// SPEC §10: requests admitted before the park still hold leases; the step
    /// stays planned until they close or its deadline passes.
    Draining,
    /// SPEC §6.5: least-recently-parked instances on the host are being
    /// stopped first; these stop operations were accepted and the step stays
    /// planned.
    Reclaiming(Vec<String>),
    /// It does not fit now and nothing may be reclaimed; the step waits for
    /// capacity or its deadline.
    Blocked(String),
    /// Refused before any effect; the operation failed with this code.
    Refused(&'static str),
}

/// SPEC §6.5: the controller-owned idle policy. `None` disables a timer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdlePolicy {
    /// After this long Ready with nothing in flight, park (or stop under
    /// restart-only).
    pub ready_idle_ms: Option<i64>,
    /// After this long parked, stop to reclaim the residual state.
    pub parked_idle_ms: Option<i64>,
}

/// One idle transition the policy accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdleAction {
    Parked {
        deployment_id: String,
        instance: u32,
        operation_id: String,
    },
    Stopped {
        deployment_id: String,
        instance: u32,
        operation_id: String,
        reason: &'static str,
    },
}

/// One step a preinitialize operation took (SPEC §6.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreinitializeProgress {
    Started {
        operation_id: String,
        deployment_id: String,
        instance: u32,
    },
    Parking {
        operation_id: String,
        deployment_id: String,
        instance: u32,
    },
    Finished {
        operation_id: String,
        deployment_id: String,
        outcome: &'static str,
    },
}

/// What a caller observes of an accepted preinitialize.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreinitializeReceipt {
    pub operation_id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub accepted_at_ms: i64,
    pub deadline_ms: i64,
    pub joined: bool,
}

// --- footprints --------------------------------------------------------------

/// The transition peak of `base` into `next`: every domain at its maximum and
/// every device claimed by either, exclusive if either is. The reservation
/// never shrinks while an effect is in flight (SPEC §7.3).
fn peak(base: &PhaseFootprint, next: &PhaseFootprint, at: ResourcePhase) -> PhaseFootprint {
    let mut out = base.clone();
    out.phase = at;
    for allocation in &next.allocations {
        match out
            .allocations
            .iter_mut()
            .find(|a| a.domain == allocation.domain)
        {
            Some(held) => {
                held.bytes = held.bytes.max(allocation.bytes);
                held.host_kv_bytes = held.host_kv_bytes.max(allocation.host_kv_bytes);
            }
            None => out.allocations.push(allocation.clone()),
        }
    }
    for device in &next.devices {
        match out.devices.iter_mut().find(|d| d.device == device.device) {
            Some(held) if device.sharing == Sharing::Exclusive => held.sharing = Sharing::Exclusive,
            Some(_) => {}
            None => out.devices.push(device.clone()),
        }
    }
    out
}

/// The four footprints a Ready launch's owner may hold (SPEC §7.3).
pub(super) struct Footprints {
    ready: PhaseFootprint,
    parking: PhaseFootprint,
    pub(super) parked: PhaseFootprint,
    pub(super) wake: PhaseFootprint,
}

pub(super) fn footprints(e: &EffectiveDeployment) -> Footprints {
    let ready = phase(&e.resources.ready, ResourcePhase::Ready);
    let parked = phase(&e.resources.parked, ResourcePhase::Parked);
    Footprints {
        parking: peak(
            &ready,
            &phase(&e.resources.parking, ResourcePhase::Parking),
            ResourcePhase::Parking,
        ),
        wake: peak(
            &parked,
            &phase(&e.resources.wake, ResourcePhase::Wake),
            ResourcePhase::Wake,
        ),
        ready,
        parked,
    }
}

/// Footprints compared as the ledger stores them (sorted, versioned).
fn same(a: &PhaseFootprint, b: &PhaseFootprint) -> bool {
    matches!(
        (resource_ledger::encode(a), resource_ledger::encode(b)),
        (Ok(a), Ok(b)) if a == b
    )
}

/// ADR 0011, SPEC §7.3: whether a completed launch's owner holds one of the
/// footprints its residency transitions leave it with. A Ready launch holds
/// the Ready footprint; a parked one the Parked; one in a park or restore
/// (armed or uncertain) the transition peak.
pub(super) fn retained_footprint(e: &EffectiveDeployment, held: Option<&PhaseFootprint>) -> bool {
    let Some(held) = held else { return false };
    let f = footprints(e);
    same(&f.ready, held) || same(&f.parking, held) || same(&f.parked, held) || same(&f.wake, held)
}

// --- small helpers -----------------------------------------------------------

pub(super) fn journal(
    tx: &Transaction<'_>,
    operation: &str,
    state: &str,
    evidence: &str,
) -> Result<(), LifecycleError> {
    tx.execute(
        "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,NULL,?2,?3,?4)",
        params![ulid::Ulid::new().to_string(), operation, state, evidence],
    )?;
    Ok(())
}

fn record(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &ResidencyPlan,
    stage: Stage,
    epoch: Option<u64>,
) -> Result<(), LifecycleError> {
    use crate::events::{append_event, EventMetadata, EventOperationId};
    let id = |v: &str| {
        v.parse()
            .map(EventOperationId::generated)
            .map_err(|_| LifecycleError::CorruptStoredData)
    };
    append_event(
        tx,
        &EventMetadata::ResidencyRecorded {
            transition: p.kind.transition(stage),
            operation_id: id(&p.operation_id)?,
            deployment_id: id(&p.deployment_id)?,
            step_id: id(&p.step_id)?,
            session_epoch: s.epoch(),
            committed_epoch: epoch,
        },
    )
    .map(|_| ())
    .map_err(|error| match error {
        crate::events::EventWriteError::Sql(error) => LifecycleError::Sql(error),
        _ => LifecycleError::CorruptStoredData,
    })
}

/// A residency command's receipt scope: the deployment's (or one instance's)
/// action scope, qualified by the change so no other action's receipt under
/// the same key is ever read as one of these.
fn residency_scope(deployment: &str, instance: Option<u32>, verb: &str) -> String {
    let base = match instance {
        None => super::cleanup::scope(deployment),
        Some(k) => instance_scope(deployment, k),
    };
    format!("{base}#{verb}")
}

fn request_hash(
    principal: &str,
    scope: &str,
    revision: i64,
    verb: &str,
    deadline: i64,
) -> Result<String, LifecycleError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(encode(&(1, principal, scope, revision, verb, deadline))?.as_bytes())
    ))
}

/// An exact earlier receipt under `(principal, scope, key)`, or `None`.
fn lookup<T: serde::de::DeserializeOwned>(
    tx: &Transaction<'_>,
    principal: &str,
    scope: &str,
    key: &str,
    hash: &str,
) -> Result<Option<T>, LifecycleError> {
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT request_hash,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",
            params![principal, scope, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match prior {
        None => Ok(None),
        Some((old, _)) if old != hash => Err(LifecycleError::IdempotencyConflict),
        Some((_, raw)) => decode(&raw).map(Some),
    }
}

fn store_receipt(
    tx: &Transaction<'_>,
    principal: &str,
    scope: &str,
    key: &str,
    hash: &str,
    operation: &str,
    response: &impl Serialize,
) -> Result<(), LifecycleError> {
    tx.execute(
        "INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",
        params![principal, scope, key, hash, operation, encode(response)?],
    )?;
    Ok(())
}

/// Whether the instance's binding is on an enrolled remote host, whose ingress
/// gate a sent park closes (W4) and only a fresh probe reopens.
fn remote(tx: &Transaction<'_>, binding: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_binding_ingress WHERE binding_id=?1)",
        [binding],
        |r| r.get(0),
    )?)
}

/// The completed launch an instance runs, with its associated group.
pub(super) fn launch(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
) -> Result<(Plan, EffectiveDeployment, Vec<ProcessIdentity>), LifecycleError> {
    let id: String = tx
        .query_row(
            "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
               JOIN runtime_bindings b ON b.id=s.binding_id
              WHERE s.deployment_id=?1 AND b.instance_index=?2 AND o.kind='initialize'
                AND s.state='completed' AND b.state='live'",
            params![deployment, instance],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(LifecycleError::Conflict)?;
    let (p, e, state) = load(tx, &id)?;
    if state != "completed" {
        return Err(LifecycleError::Conflict);
    }
    let association = association(tx, &p)?.ok_or(LifecycleError::CorruptStoredData)?;
    let identities = members(&association.identities)?;
    Ok((p, e, identities))
}

/// SPEC §6.2, ADR 0010, ADR 0012: whether this revision parks at all. A
/// restart-only deployment never parks (a park is refused, never a stop in
/// disguise), and a host that opted out of deep parking refuses it too.
pub(super) fn parks(e: &EffectiveDeployment) -> bool {
    e.residency != Residency::RestartOnly && e.profile.security.deep_park.is_enabled()
}

/// The instance's stored observed state and whether it may take new work.
fn instance_state(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
) -> Result<Option<(String, String, bool, bool)>, LifecycleError> {
    Ok(tx
        .query_row(
            "SELECT i.observed_state,i.state,i.operator_stopped=1,
                    (i.desired_state='ready' AND i.admission_enabled=1 AND d.suspended=0)
               FROM deployment_instances i JOIN deployments d ON d.id=i.deployment_id
              WHERE i.deployment_id=?1 AND i.instance_index=?2",
            params![deployment, instance],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?)
}

/// Any lifecycle run of the instance still open, with its operation kind.
fn open_run(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
) -> Result<Option<(String, String)>, LifecycleError> {
    Ok(tx
        .query_row(
            "SELECT r.operation_id,o.kind FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id
              WHERE r.deployment_id=?1 AND r.instance_index=?2 AND r.state IN ('queued','running','uncertain')
              ORDER BY o.accepted_at,o.id LIMIT 1",
            params![deployment, instance],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn step_of(tx: &Transaction<'_>, operation: &str) -> Result<String, LifecycleError> {
    tx.query_row(
        "SELECT id FROM lifecycle_steps WHERE operation_id=?1",
        [operation],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(LifecycleError::CorruptStoredData)
}

// --- loading and validation --------------------------------------------------

/// A residency step with its plan and durable state, validated against its
/// run and operation. History only: authority needs `current_residency` too.
fn read(tx: &Transaction<'_>, id: &str) -> Result<(ResidencyPlan, String), LifecycleError> {
    let row: Option<(String, String, String, String, String)> = tx
        .query_row(
            "SELECT s.step_json,s.state,o.kind,r.state,o.state FROM lifecycle_steps s
               JOIN operations o ON o.id=s.operation_id JOIN lifecycle_runs r ON r.operation_id=s.operation_id
              WHERE s.id=?1 AND o.kind IN ('park','restore')",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (raw, state, kind, run, operation) = row.ok_or(LifecycleError::Unsupported)?;
    let p: ResidencyPlan = decode(&raw)?;
    if p.version != 1
        || p.step_id != id
        || p.kind.operation_kind() != kind
        || p.accepted_at_ms < 0
        || p.deadline_ms <= p.accepted_at_ms
        || p.revision < 1
        || p.generation < 1
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    for value in [
        &p.operation_id,
        &p.step_id,
        &p.deployment_id,
        &p.binding_id,
        &p.incarnation,
        &p.session_id,
        &p.source_step_id,
    ] {
        if ulid::Ulid::from_string(value).is_err() {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    let consistent = match state.as_str() {
        "planned" => run == "queued" && operation == "pending" && p.execution.is_none(),
        "armed" => run == "running" && operation == "running" && p.execution.is_some(),
        "uncertain" => run == "uncertain" && operation == "running" && p.execution.is_some(),
        "completed" => run == "succeeded" && operation == "succeeded" && p.execution.is_some(),
        "cancelled" => run == "failed" && operation == "failed",
        _ => false,
    };
    if !consistent {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok((p, state))
}

/// The plan is this session's current work on the instance it names: the
/// fence still names the instance, the claim is the plan's, and the source
/// launch is the one retained.
fn current_residency(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &ResidencyPlan,
) -> Result<(Plan, EffectiveDeployment, Vec<ProcessIdentity>), LifecycleError> {
    check_session(tx, s)?;
    if p.session_id != s.id() {
        return Err(LifecycleError::Stale);
    }
    if crate::instances::fence_instance(tx, &p.fence())? != p.instance_index {
        return Err(LifecycleError::Stale);
    }
    let claimed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND instance_index=?2 AND operation_id=?3 AND revision=?4 AND generation=?5)",
        params![p.deployment_id, p.instance_index, p.operation_id, p.revision, p.generation],
        |r| r.get(0),
    )?;
    if !claimed {
        return Err(LifecycleError::Stale);
    }
    let (source, e, identities) = launch(tx, &p.deployment_id, p.instance_index)?;
    if source.step_id != p.source_step_id
        || source.binding_id != p.binding_id
        || source.incarnation != p.incarnation
        || source.fence() != p.fence()
    {
        return Err(LifecycleError::Conflict);
    }
    Ok((source, e, identities))
}

// --- acceptance --------------------------------------------------------------

/// Accept one instance's park or restore, or join the one of that kind already
/// open. Nothing is sent; the worker arms it.
#[allow(clippy::too_many_arguments)]
fn accept_instance(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    kind: ResidencyKind,
    deployment: &str,
    instance: u32,
    principal: &str,
    now: i64,
    deadline: i64,
) -> Result<ResidencyReceipt, LifecycleError> {
    // T15: a second request for the same change joins the open one.
    if let Some((operation, open_kind)) = open_run(tx, deployment, instance)? {
        if open_kind == kind.operation_kind() {
            let (p, _) = read(tx, &step_of(tx, &operation)?)?;
            return Ok(ResidencyReceipt::of(&p, true));
        }
        return Err(LifecycleError::Conflict);
    }
    let (source, e, _) = launch(tx, deployment, instance)?;
    current(tx, s, &source, true)?;
    if kind == ResidencyKind::Park && !parks(&e) {
        // SPEC §6.3: fail clearly if explicit parking is unsupported.
        return Err(LifecycleError::Unsupported);
    }
    let (observed, lifecycle, _, admitting) =
        instance_state(tx, deployment, instance)?.ok_or(LifecycleError::NotFound)?;
    let expected = match kind {
        ResidencyKind::Park => "ready",
        ResidencyKind::Restore => "parked",
    };
    if observed != expected || lifecycle != "active" || !admitting {
        return Err(LifecycleError::Conflict);
    }
    if deadline <= now
        || now < source.accepted_at_ms
        || deadline
            .checked_sub(now)
            .is_none_or(|window| window > e.request_deadline_ms)
    {
        return Err(LifecycleError::Invalid);
    }
    let claimed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND instance_index=?2)",
        params![deployment, instance],
        |r| r.get(0),
    )?;
    if claimed {
        return Err(LifecycleError::Conflict);
    }
    let p = ResidencyPlan {
        version: 1,
        kind,
        operation_id: ulid::Ulid::new().to_string(),
        step_id: ulid::Ulid::new().to_string(),
        deployment_id: deployment.into(),
        revision: source.revision,
        generation: source.generation,
        instance_index: instance,
        session_id: s.id().into(),
        binding_id: source.binding_id.clone(),
        incarnation: source.incarnation.clone(),
        source_step_id: source.step_id.clone(),
        principal: principal.into(),
        accepted_at_ms: now,
        deadline_ms: deadline,
        execution: None,
    };
    let run_plan = serde_json::json!({
        "version": 1,
        "steps": [{"deployment_id": deployment, "action": if kind == ResidencyKind::Park { "park" } else { "activate" }}],
        "cleanup_target": null,
        "handoffs": [],
    })
    .to_string();
    tx.execute(
        "INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,?3,'pending')",
        params![p.operation_id, deployment, kind.operation_kind()],
    )?;
    tx.execute(
        "INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES(?1,?2,?3,?4,?5,?6,'queued',?7,?8,?9)",
        params![p.operation_id, deployment, p.revision, p.generation, s.id(), kind.run_action(), deadline, run_plan, instance],
    )?;
    tx.execute(
        "INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation,instance_index) VALUES(?1,?2,?3,?4,?5)",
        params![deployment, p.operation_id, p.revision, p.generation, instance],
    )?;
    tx.execute(
        "INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",
        params![p.step_id, p.operation_id, deployment, p.binding_id, s.id(), encode(&p)?],
    )?;
    if kind == ResidencyKind::Park {
        // SPEC §10 steps 1–2: admission to the engine closes before the drain;
        // the instance stays eligible for on-demand activation.
        tx.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id=?1 AND instance_index=?2",
            params![deployment, instance],
        )?;
    }
    journal(
        tx,
        &p.operation_id,
        &format!("{}_accepted", kind.operation_kind()),
        &format!(
            "deployment {deployment}: instance {instance} {} accepted by {principal}; nothing is sent before it arms",
            kind.verb()
        ),
    )?;
    record(tx, s, &p, Stage::Accepted, None)?;
    Ok(ResidencyReceipt::of(&p, false))
}

/// The current revision's effective configuration, for command-level refusals.
fn declared(tx: &Transaction<'_>, deployment: &str) -> Result<EffectiveDeployment, LifecycleError> {
    let raw: String = tx
        .query_row(
            "SELECT e.effective_json FROM effective_revisions e JOIN deployments d ON d.id=e.deployment_id AND d.revision=e.revision WHERE d.id=?1",
            [deployment],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(LifecycleError::NotFound)?;
    decode_effective_snapshot(&raw).map_err(|_| LifecycleError::CorruptStoredData)
}

impl crate::Store {
    /// SPEC §6.3 `park deployment`: drain and park every READY instance at the
    /// declared tier, leaving the deployment eligible for on-demand
    /// activation. Refused (`Unsupported`) for a restart-only deployment or a
    /// host that opted out of deep parking. The first instance's park is
    /// answered under the deployment's scope; each sibling is its own
    /// operation under its instance's scope with the same key, accepted in the
    /// same transaction, so a retry replays them all.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_park_command(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        expected_revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<ResidencyReceipt, LifecycleError> {
        super::receipt::check_request(principal, deployment, expected_revision, key, deadline)?;
        if now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let scope = residency_scope(deployment, None, "park");
        let hash = request_hash(principal, &scope, expected_revision, "park", deadline)?;
        if let Some(receipt) = lookup(&tx, principal, &scope, key, &hash)? {
            return Ok(receipt);
        }
        super::receipt::command_revision(&tx, deployment, expected_revision)?;
        super::check_managed_command_target(&tx, deployment)?;
        if !parks(&declared(&tx, deployment)?) {
            return Err(LifecycleError::Unsupported);
        }
        let targets: Vec<u32> = tx
            .prepare(
                "SELECT i.instance_index FROM deployment_instances i WHERE i.deployment_id=?1 AND i.state='active'
                   AND (i.observed_state='ready' OR EXISTS(SELECT 1 FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id
                        WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND o.kind='park'
                          AND r.state IN ('queued','running','uncertain')))
                 ORDER BY i.instance_index",
            )?
            .query_map([deployment], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut first: Option<ResidencyReceipt> = None;
        for instance in targets {
            match accept_instance(
                &tx,
                s,
                ResidencyKind::Park,
                deployment,
                instance,
                principal,
                now,
                deadline,
            ) {
                Ok(receipt) => {
                    if first.is_some() {
                        let scope = residency_scope(deployment, Some(instance), "park");
                        let hash =
                            request_hash(principal, &scope, expected_revision, "park", deadline)?;
                        if lookup::<ResidencyReceipt>(&tx, principal, &scope, key, &hash)?.is_none()
                        {
                            store_receipt(
                                &tx,
                                principal,
                                &scope,
                                key,
                                &hash,
                                &receipt.operation_id,
                                &receipt,
                            )?;
                        }
                    }
                    first.get_or_insert(receipt);
                }
                // An instance whose own launch is busy (a start, a stop) is
                // not READY for this command; its siblings still park.
                Err(LifecycleError::Conflict | LifecycleError::Stale) => {}
                Err(error) => return Err(error),
            }
        }
        // SPEC §6.3: a deployment with no READY instance has nothing to park.
        let receipt = first.ok_or(LifecycleError::Conflict)?;
        store_receipt(
            &tx,
            principal,
            &scope,
            key,
            &hash,
            &receipt.operation_id,
            &receipt,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// SPEC §6.5: park one READY instance (idle policy, preinitialize). Under
    /// the instance's own scope; an exact retry replays the receipt.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_instance_park(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        instance: u32,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<ResidencyReceipt, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let receipt = Self::instance_park_in_transaction(
            &tx, s, principal, deployment, instance, key, now, deadline,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn instance_park_in_transaction(
        tx: &Transaction<'_>,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        instance: u32,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<ResidencyReceipt, LifecycleError> {
        let revision: i64 = tx
            .query_row(
                "SELECT revision FROM deployments WHERE id=?1",
                [deployment],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LifecycleError::NotFound)?;
        super::receipt::check_request(principal, deployment, revision, key, deadline)?;
        let scope = residency_scope(deployment, Some(instance), "park");
        let hash = request_hash(principal, &scope, revision, "park", deadline)?;
        if let Some(receipt) = lookup(tx, principal, &scope, key, &hash)? {
            return Ok(receipt);
        }
        let receipt = accept_instance(
            tx,
            s,
            ResidencyKind::Park,
            deployment,
            instance,
            principal,
            now,
            deadline,
        )?;
        store_receipt(
            tx,
            principal,
            &scope,
            key,
            &hash,
            &receipt.operation_id,
            &receipt,
        )?;
        Ok(receipt)
    }

    /// SPEC §6.4: the exact receipt an earlier wake under `key` recorded, read
    /// only, so a caller can tell a retry from a new command before it changes
    /// anything. `Err(IdempotencyConflict)` when the key answered another
    /// request. Grants nothing and accepts nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn restore_command_receipt(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        scope: WakeScope,
        expected_revision: i64,
        key: &str,
        deadline: i64,
    ) -> Result<Option<ResidencyReceipt>, LifecycleError> {
        super::receipt::check_request(principal, deployment, expected_revision, key, deadline)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let receipt_scope = residency_scope(
            deployment,
            match scope {
                WakeScope::Instance(k) => Some(k),
                _ => None,
            },
            "wake",
        );
        let hash = request_hash(
            principal,
            &receipt_scope,
            expected_revision,
            "wake",
            deadline,
        )?;
        lookup(&tx, principal, &receipt_scope, key, &hash)
    }

    /// SPEC §6.3 `start deployment` and on-demand activation of a parked
    /// deployment: restore parked instances in place, on the host each parked
    /// on (ADR 0013 §4). `None` when the scope names no parked instance, so
    /// the caller starts cold instead; nothing is recorded then.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_restore_command(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        scope: WakeScope,
        expected_revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<Option<ResidencyReceipt>, LifecycleError> {
        super::receipt::check_request(principal, deployment, expected_revision, key, deadline)?;
        if now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let receipt_scope = residency_scope(
            deployment,
            match scope {
                WakeScope::Instance(k) => Some(k),
                _ => None,
            },
            "wake",
        );
        let hash = request_hash(
            principal,
            &receipt_scope,
            expected_revision,
            "wake",
            deadline,
        )?;
        if let Some(receipt) = lookup(&tx, principal, &receipt_scope, key, &hash)? {
            return Ok(Some(receipt));
        }
        super::receipt::command_revision(&tx, deployment, expected_revision)?;
        super::check_managed_command_target(&tx, deployment)?;
        // (instance, restoring now, parked, operator stopped)
        let rows: Vec<(u32, bool, bool, bool)> = tx
            .prepare(
                "SELECT i.instance_index,
                        EXISTS(SELECT 1 FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id
                                WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND o.kind='restore'
                                  AND r.state IN ('queued','running','uncertain')),
                        i.observed_state='parked', i.operator_stopped=1
                   FROM deployment_instances i WHERE i.deployment_id=?1 AND i.state='active'
                  ORDER BY i.instance_index",
            )?
            .query_map([deployment], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        let targets: Vec<u32> = match scope {
            // Q5, T15: a restore in flight is joined; otherwise the lowest
            // parked instance the operator did not stop is woken.
            WakeScope::OnDemand => rows
                .iter()
                .find(|r| r.1)
                .or_else(|| rows.iter().find(|r| r.2 && !r.3))
                .map(|r| vec![r.0])
                .unwrap_or_default(),
            WakeScope::All => rows.iter().filter(|r| r.1 || r.2).map(|r| r.0).collect(),
            WakeScope::Instance(k) => rows
                .iter()
                .filter(|r| r.0 == k && (r.1 || r.2))
                .map(|r| r.0)
                .collect(),
        };
        let mut first: Option<ResidencyReceipt> = None;
        for instance in targets {
            match accept_instance(
                &tx,
                s,
                ResidencyKind::Restore,
                deployment,
                instance,
                principal,
                now,
                deadline,
            ) {
                Ok(receipt) => {
                    first.get_or_insert(receipt);
                }
                Err(LifecycleError::Conflict | LifecycleError::Stale)
                    if scope == WakeScope::All => {}
                Err(error) => return Err(error),
            }
        }
        let Some(receipt) = first else {
            return Ok(None);
        };
        store_receipt(
            &tx,
            principal,
            &receipt_scope,
            key,
            &hash,
            &receipt.operation_id,
            &receipt,
        )?;
        tx.commit()?;
        Ok(Some(receipt))
    }

    /// This session's planned parks and restores in acceptance order, after
    /// closing every one whose deadline passed before it armed (no effect was
    /// sent, so nothing is retained for it).
    pub fn next_residency_work(
        &self,
        s: &CoordinatorSession,
        now: i64,
    ) -> Result<Vec<ResidencyWork>, LifecycleError> {
        // The worker asks on every pass: stay a plain read unless there is
        // planned residency work at all.
        let any: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              WHERE o.kind IN ('park','restore') AND s.state='planned' AND s.session_id=?1)",
            [s.id()],
            |r| r.get(0),
        )?;
        if !any {
            return Ok(Vec::new());
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let expired: Vec<String> = tx
            .prepare(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN lifecycle_runs r ON r.operation_id=s.operation_id
                  WHERE o.kind IN ('park','restore') AND s.state='planned' AND s.session_id=?1 AND r.deadline_ms<=?2",
            )?
            .query_map(params![s.id(), now], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in expired {
            let (p, _) = read(&tx, &id)?;
            cancel(
                &tx,
                s,
                &p,
                "deadline",
                "the deadline passed before it armed; nothing was sent",
            )?;
        }
        let planned: Vec<String> = tx
            .prepare(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                  WHERE o.kind IN ('park','restore') AND s.state='planned' AND s.session_id=?1
                  ORDER BY o.accepted_at,o.id LIMIT 16",
            )?
            .query_map([s.id()], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut work = Vec::new();
        for id in planned {
            let (p, _) = read(&tx, &id)?;
            let Ok((_, e, _)) = current_residency(&tx, s, &p) else {
                continue;
            };
            let policy = policy(&tx, &e)?;
            work.push(ResidencyWork {
                step_id: p.step_id.clone(),
                operation_id: p.operation_id.clone(),
                deployment_id: p.deployment_id.clone(),
                instance: p.instance_index,
                kind: p.kind,
                binding_id: p.binding_id.clone(),
                incarnation: p.incarnation.clone(),
                host: e.host.name.clone(),
                deadline_ms: p.deadline_ms,
                limits: limits(&policy),
                observation_ttl_ms: policy.controls.observation_ttl_ms,
                max_parked: policy.controls.max_parked as usize,
            });
        }
        tx.commit()?;
        Ok(work)
    }

    /// Arm one planned park or restore against fresh host observations.
    /// Only `ResidencyArm::New` permits a send, exactly once.
    pub fn arm_residency(
        &self,
        s: &CoordinatorSession,
        id: &str,
        context: AdmissionContext<'_>,
    ) -> Result<ResidencyArm, LifecycleError> {
        self.arm_residency_with_residents(s, id, context, &[])
    }

    /// As [`Self::arm_residency`], crediting Ready engines on the host with
    /// the memory `residents` (sampled beside the observations) attributes to
    /// their own processes (ADR 0007).
    pub fn arm_residency_with_residents(
        &self,
        s: &CoordinatorSession,
        id: &str,
        context: AdmissionContext<'_>,
        residents: &[mllm_domain::resources::ProcessResident],
    ) -> Result<ResidencyArm, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let outcome = arm(&tx, s, id, context, residents)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Commit a park's or restore's completed evidence: the launch's exact
    /// group, observed fresh within the step, with exactly the step's facts.
    pub fn complete_residency(
        &self,
        s: &CoordinatorSession,
        id: &str,
        observation: &EffectObservation,
        now: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        complete(&tx, s, id, observation, now)?;
        tx.commit()?;
        Ok(())
    }

    /// SPEC §13 (W4): the engine or host refused the armed step before any
    /// effect. The operation fails with `code`, the grant's peak is returned
    /// to the footprint the launch held, and the launch keeps its state. A
    /// refused park of an embedded launch reopens its dispatch at once; a
    /// remote host's gate stays closed until a fresh probe reopens it.
    pub fn refuse_residency(
        &self,
        s: &CoordinatorSession,
        id: &str,
        reason: &str,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        refuse(&tx, s, id, reason)?;
        tx.commit()?;
        Ok(())
    }

    /// SPEC §13.2, T20: the armed step's outcome is unknown. Everything is
    /// retained (peak reservation, claim, closed dispatch) and the step is
    /// never re-armed; an operator Stop resolves it on gone evidence.
    pub fn mark_residency_uncertain(
        &self,
        s: &CoordinatorSession,
        id: &str,
        reason: &str,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, state) = read(&tx, id)?;
        if state == "uncertain" {
            return Ok(false);
        }
        if state != "armed" || p.session_id != s.id() {
            return Err(LifecycleError::Conflict);
        }
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='uncertain' WHERE id=?1 AND state='armed'",
            [id],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='uncertain' WHERE operation_id=?1 AND state='running'",
            [&p.operation_id],
        )?)?;
        tx.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id=?1 AND instance_index=?2",
            params![p.deployment_id, p.instance_index],
        )?;
        journal(
            &tx,
            &p.operation_id,
            &format!("{}_uncertain", p.kind.operation_kind()),
            &format!(
                "deployment {}: instance {} {} outcome unknown ({}); its reservation, claim and closed gate are retained until a stop proves the group gone",
                p.deployment_id,
                p.instance_index,
                p.kind.verb(),
                redact_reason(reason)
            ),
        )?;
        record(&tx, s, &p, Stage::Uncertain, None)?;
        tx.commit()?;
        Ok(true)
    }

    /// The residency plan's operation state for a step, for observers.
    pub fn residency_step_state(&self, id: &str) -> Result<Option<String>, LifecycleError> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind IN ('park','restore')",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }
}

fn redact_reason(reason: &str) -> String {
    reason.chars().take(512).collect()
}

fn limits(policy: &ResourcePolicySnapshot) -> Vec<MemoryLimit> {
    policy
        .controls
        .domains
        .iter()
        .map(|(id, d)| MemoryLimit {
            domain: id.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect()
}

/// Close a step that never armed (deadline, superseded): nothing was sent.
fn cancel(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &ResidencyPlan,
    code: &str,
    why: &str,
) -> Result<(), LifecycleError> {
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='planned'",
        [&p.step_id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state='queued'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='failed',error_code=?2 WHERE id=?1 AND state='pending'",
        params![
            p.operation_id,
            format!("{}_{code}", p.kind.operation_kind())
        ],
    )?)?;
    tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.operation_id],
    )?;
    if p.kind == ResidencyKind::Park {
        // Nothing reached the engine or its host's gate: the launch is still
        // Ready and serves again.
        reopen(tx, p)?;
    }
    journal(
        tx,
        &p.operation_id,
        &format!("{}_cancelled", p.kind.operation_kind()),
        &format!(
            "deployment {}: instance {} {} closed: {why}",
            p.deployment_id,
            p.instance_index,
            p.kind.verb()
        ),
    )?;
    record(tx, s, p, Stage::Cancelled, None)
}

/// Reopen a Ready instance's dispatch after a park that had no effect on it.
///
/// ADR 0015 amendment (closure reasons), SPEC §13.2: the park reopens only the
/// gate it closed. A host-session, engine-exit or switch closure recorded for
/// the incarnation while the park was in flight keeps it closed until its own
/// evidence clears it.
fn reopen(tx: &Transaction<'_>, p: &ResidencyPlan) -> Result<(), LifecycleError> {
    tx.execute(
        &format!(
            "UPDATE deployment_instances AS i SET dispatch_enabled=1 WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.generation=?3
                AND i.observed_state='ready' AND i.desired_state='ready' AND i.admission_enabled=1
                AND NOT EXISTS(SELECT 1 FROM deployments d WHERE d.id=?1 AND d.suspended=1)
                AND {}",
            crate::switch_state::no_closure_clause("i")
        ),
        params![p.deployment_id, p.instance_index, p.generation],
    )?;
    Ok(())
}

/// One parked instance that capacity may reclaim.
struct Parked {
    deployment: String,
    instance: u32,
    owner: String,
    /// Already being stopped (a reclamation in progress): it will leave the
    /// ledger on its own gone evidence.
    stopping: bool,
}

/// Parked instances on the host `scoped` covers that may be reclaimed by a
/// stop: those already stopping first, then least recently parked first
/// (SPEC §6.5), by the epoch their park evidence committed. An instance with
/// any other work in flight is not a candidate.
fn parked_lru(
    tx: &Transaction<'_>,
    scoped: &LedgerSnapshot,
) -> Result<Vec<Parked>, LifecycleError> {
    // SPEC §6.5: a warm-residency commitment is never reclaimed by capacity
    // (one already being stopped still leaves on its own evidence).
    let warm = crate::switch_state::warm_clause("i");
    // SPEC §6.5, §10: nor is the parked target of a switch in progress; it is
    // waking (see `switch_waking`).
    let waking = crate::switch_state::switch_target_clause("i");
    let rows: Vec<(String, u32, bool)> = tx
        .prepare(&format!(
            "SELECT i.deployment_id,i.instance_index,
                    EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action='stop' AND r.state IN ('queued','running','uncertain')) AS stopping
               FROM deployment_instances i
               JOIN runtime_bindings b ON b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index AND b.state!='released'
              WHERE i.observed_state='parked'
                AND NOT EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action!='stop' AND r.state IN ('queued','running','uncertain'))
                AND (NOT ({warm} OR {waking}) OR EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action='stop' AND r.state IN ('queued','running','uncertain')))
              ORDER BY stopping DESC,
                       COALESCE((SELECT MAX(e.committed_epoch) FROM lifecycle_evidence e JOIN lifecycle_steps s ON s.id=e.step_id
                                  JOIN operations o ON o.id=s.operation_id
                                 WHERE s.binding_id=b.id AND o.kind='park' AND s.state='completed'),0),
                       i.deployment_id,i.instance_index",
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(deployment, instance, stopping)| {
            let owner = crate::instances::instance_owner_id(&deployment, instance);
            Parked {
                deployment,
                instance,
                owner,
                stopping,
            }
        })
        .filter(|parked| scoped.owners.contains_key(&parked.owner))
        .collect())
}

/// SPEC §6.5, §10: the owners of the parked instances a switch in progress is
/// waking.
///
/// Found live 2026-09-23 (matrix M31, `max_parked: 1`): switching back to a
/// parked deployment released its victim by parking it, the victim's park
/// found the parked set full of the switch's own target, and reclaimed it, so
/// every switch was a cold restart. The target leaves the parked set when the
/// switch wakes it, so it is counted as waking, never as reclaimable. Its
/// memory stays charged exactly as the ledger holds it: the wake peak is
/// reserved only when the wake itself is admitted, after the victims release.
fn switch_waking(tx: &Transaction<'_>) -> Result<Vec<String>, LifecycleError> {
    let rows: Vec<(String, u32)> = tx
        .prepare(&format!(
            "SELECT i.deployment_id,i.instance_index FROM deployment_instances i
              WHERE i.observed_state='parked' AND {}
              ORDER BY i.deployment_id,i.instance_index",
            crate::switch_state::switch_target_clause("i")
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(deployment, instance)| crate::instances::instance_owner_id(&deployment, instance))
        .collect())
}

/// SPEC §6.5 (W5): the owners of every parked instance capacity may reclaim,
/// for placement to treat their residual reservations as available.
pub(super) fn reclaimable_parked_owners(
    tx: &Transaction<'_>,
) -> Result<Vec<String>, LifecycleError> {
    let everyone = resource_ledger::read_snapshot(tx).map_err(resource)?;
    Ok(parked_lru(tx, &everyone)?
        .into_iter()
        .map(|p| p.owner)
        .collect())
}

/// What fitting a candidate's next footprints on its host needs.
enum Fit {
    /// It fits now.
    Fits,
    /// It fits once these parked instances are stopped.
    Reclaim(Vec<(String, u32)>),
    /// It fits once the parked instances already stopping are gone.
    Waiting,
    /// It does not fit even with every parked instance reclaimed.
    Impossible(String),
}

/// SPEC §6.5, §7.3: whether `targets` (in order) can be admitted for `owner`
/// on the host `scoped` covers, reclaiming least recently parked instances
/// first. Ready work is never a candidate.
fn fit(
    tx: &Transaction<'_>,
    scoped: &LedgerSnapshot,
    owner: &str,
    targets: &[PhaseFootprint],
    context: AdmissionContext<'_>,
) -> Result<Fit, LifecycleError> {
    let lru = parked_lru(tx, scoped)?;
    let order: Vec<String> = lru.iter().map(|p| p.owner.clone()).collect();
    let mut state = scoped.clone();
    // SPEC §6.5, §10: a switch's parked target is waking: it leaves the
    // parked count, and keeps the memory it holds (never for its own
    // admission, which the wake itself is).
    for waking in switch_waking(tx)? {
        if waking == owner {
            continue;
        }
        if let Some(current) = state.owners.get_mut(&waking) {
            if current.phase == ResourcePhase::Parked {
                current.phase = ResourcePhase::Wake;
            }
        }
    }
    let mut victims: Vec<String> = Vec::new();
    for target in targets {
        // Found live 2026-09-23 (matrix M27): a transition that allocates
        // nothing beyond what the owner already holds (a park's parking and
        // parked phases) cannot need free memory. Its own charge is already in
        // use, so the host's published free memory never covers it again, and
        // every park of a large Ready model was held until its deadline. It is
        // still judged on the parked-set rules (count and category limits).
        if state
            .owners
            .get(owner)
            .is_some_and(|current| !resource_ledger::allocates_more(current, target))
            && matches!(
                mllm_scheduler::residency::admit_phase(&state, owner, target, context),
                Ok(()) | Err(mllm_domain::resources::ResourceError::Insufficient)
            )
        {
            continue;
        }
        match mllm_scheduler::sequence::lru_parked_victims(&state, owner, target, &order, context) {
            Some(more) => {
                for victim in more {
                    state.owners.remove(&victim);
                    victims.push(victim);
                }
            }
            None => {
                return Ok(Fit::Impossible(
                    match mllm_scheduler::residency::admit_phase(&state, owner, target, context) {
                        Err(error) => error.to_string(),
                        Ok(()) => "no fit".into(),
                    },
                ))
            }
        }
    }
    if victims.is_empty() {
        return Ok(Fit::Fits);
    }
    let chosen: Vec<(String, u32)> = lru
        .iter()
        .filter(|p| victims.contains(&p.owner) && !p.stopping)
        .map(|p| (p.deployment.clone(), p.instance))
        .collect();
    Ok(if chosen.is_empty() {
        Fit::Waiting
    } else {
        Fit::Reclaim(chosen)
    })
}

/// SPEC §6.5: stop the chosen parked victims, ordinarily (on-demand
/// eligibility is kept), each completing only on gone evidence.
fn reclaim(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    victims: &[(String, u32)],
    cause: &str,
    now: i64,
) -> Result<Vec<String>, LifecycleError> {
    let mut stops = Vec::new();
    for (deployment, instance) in victims {
        let (source, e, _) = launch(tx, deployment, *instance)?;
        let revision: i64 = tx.query_row(
            "SELECT revision FROM deployments WHERE id=?1",
            [deployment],
            |r| r.get(0),
        )?;
        let key = format!("reclaim:{cause}:{}:{}", source.generation, source.step_id);
        let command = StopCommand {
            scope: Some(instance_scope(deployment, *instance)),
            revision: Some(revision),
        };
        let receipt = crate::Store::accept_instance_stop_in_transaction(
            tx,
            s,
            SCHEDULER_PRINCIPAL,
            &source.fence(),
            &key,
            now,
            now.saturating_add(e.request_deadline_ms),
            &command,
        )?;
        journal(
            tx,
            &receipt.operation_id,
            "parked_reclaimed",
            &format!(
                "deployment {deployment}: parked instance {instance} is stopped to reclaim its residual state for {cause} (least recently parked first); cleanup completes only on gone evidence"
            ),
        )?;
        stops.push(receipt.operation_id);
    }
    Ok(stops)
}

fn arm(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    context: AdmissionContext<'_>,
    residents: &[mllm_domain::resources::ProcessResident],
) -> Result<ResidencyArm, LifecycleError> {
    let (mut p, state) = read(tx, id)?;
    if state != "planned" {
        return Err(LifecycleError::Conflict);
    }
    let (_, e, identities) = current_residency(tx, s, &p)?;
    if context.now_ms < p.accepted_at_ms || context.now_ms >= p.deadline_ms {
        return Err(LifecycleError::Conflict);
    }
    let (observed, _, _, admitting) =
        instance_state(tx, &p.deployment_id, p.instance_index)?.ok_or(LifecycleError::NotFound)?;
    let expected = match p.kind {
        ResidencyKind::Park => "ready",
        ResidencyKind::Restore => "parked",
    };
    if observed != expected || !admitting {
        return Err(LifecycleError::Conflict);
    }
    if p.kind == ResidencyKind::Park {
        // SPEC §10 step 4: in-flight work drains before the park is sent.
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2)",
            params![p.deployment_id, p.instance_index],
            |r| r.get(0),
        )?;
        if leased {
            return Ok(ResidencyArm::Draining);
        }
    }
    let policy = policy(tx, &e)?;
    let mut supplied = context.limits.to_vec();
    supplied.sort_by(|a, b| a.domain.cmp(&b.domain));
    if supplied != limits(&policy)
        || context.ttl_ms != policy.controls.observation_ttl_ms
        || context.max_parked != policy.controls.max_parked as usize
        || !context.resident_floors.is_empty()
    {
        return Err(LifecycleError::Conflict);
    }
    let owner = p.owner();
    let f = footprints(&e);
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    // ADR 0007 (found live 2026-09-23, matrix M33): a wake beside a Ready
    // engine was refused because the memory that engine holds, already out of
    // the host's availability, was charged again. Ready engines are credited
    // what the host sampled for their own processes.
    let floors = crate::resident_floors::resident_floors(
        tx,
        &ledger,
        &owner,
        context.observations,
        residents,
        &crate::resident_floors::domain_kinds(&policy.controls),
    )?;
    let context = AdmissionContext {
        resident_floors: &floors,
        ..context
    };
    let (held, next, targets) = match p.kind {
        ResidencyKind::Park => (
            &f.ready,
            f.parking.clone(),
            vec![f.parking.clone(), f.parked.clone()],
        ),
        ResidencyKind::Restore => (&f.parked, f.wake.clone(), vec![f.wake.clone()]),
    };
    if !ledger
        .owners
        .get(&owner)
        .is_some_and(|current| same(current, held))
    {
        return Err(LifecycleError::Conflict);
    }
    // SPEC §6.5, §7.3: budget the transition, and a park's parked result
    // (the parked count and residual budget), before anything increases.
    let scoped = resource_ledger::scoped_to_domain_hosts(
        tx,
        &ledger,
        context.limits.iter().map(|l| l.domain.as_str()),
    )
    .map_err(resource)?;
    match fit(tx, &scoped, &owner, &targets, context)? {
        Fit::Fits => {}
        Fit::Reclaim(chosen) => {
            let cause = format!(
                "{} of deployment {} instance {}",
                p.kind.verb(),
                p.deployment_id,
                p.instance_index
            );
            let stops = reclaim(tx, s, &chosen, &cause, context.now_ms)?;
            return Ok(ResidencyArm::Reclaiming(stops));
        }
        Fit::Waiting => {
            return Ok(ResidencyArm::Blocked(
                "waiting for reclaimed parked instances to stop".into(),
            ))
        }
        // A park that can never fit the parked set is refused; the launch
        // stays Ready and serves again. Anything else waits for capacity or
        // its deadline (it never evicts Ready work).
        Fit::Impossible(why) => {
            let parked_set = p.kind == ResidencyKind::Park
                && mllm_scheduler::residency::admit_phase(&scoped, &owner, &f.parking, context)
                    .is_ok();
            return Ok(if parked_set {
                fail(tx, s, &p, "parked_capacity", &why)?;
                ResidencyArm::Refused("parked_capacity")
            } else {
                ResidencyArm::Blocked(why)
            });
        }
    }
    let execution = ResidencyExecution {
        issued_at_ms: context.now_ms,
        grant_id: ulid::Ulid::new().to_string(),
        expected_epoch: ledger.epoch,
    };
    match reserve_increase_in_transaction(
        tx,
        &GrantRequest {
            id: execution.grant_id.clone(),
            owner_id: owner,
            deployment_id: p.deployment_id.clone(),
            operation_id: p.operation_id.clone(),
            revision: p.revision,
            generation: p.generation,
            expected_epoch: ledger.epoch,
            next,
        },
        context,
    ) {
        Ok(_) => {}
        // Stale observation, a racing epoch: nothing was written; retry later.
        Err(error) => return Ok(ResidencyArm::Blocked(error.to_string())),
    }
    one(tx.execute(
        "UPDATE operations SET state='running' WHERE id=?1 AND state='pending'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1 AND session_id=?2 AND state='queued'",
        params![p.operation_id, s.id()],
    )?)?;
    p.execution = Some(execution);
    let grant = p.execution.as_ref().map(|x| x.grant_id.clone());
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='armed',step_json=?2,grant_id=?3 WHERE id=?1 AND session_id=?4 AND state='planned' AND grant_id IS NULL",
        params![id, encode(&p)?, grant, s.id()],
    )?)?;
    record(tx, s, &p, Stage::Armed, None)?;
    Ok(ResidencyArm::New(Box::new(StepExecutionContext {
        token: p.token(),
        binding_id: p.binding_id.clone(),
        incarnation: p.incarnation.clone(),
        issued_at_ms: context.now_ms,
        deadline_ms: p.deadline_ms,
        identities: ExecutionIdentities::Retained(identities),
        completion_target: None,
        grant_id: grant,
        launch_settings: None,
    })))
}

/// Fail a planned step before it armed (a refusal decided at arm).
fn fail(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &ResidencyPlan,
    code: &str,
    why: &str,
) -> Result<(), LifecycleError> {
    cancel(tx, s, p, code, why)
}

fn complete(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    o: &EffectObservation,
    now: i64,
) -> Result<(), LifecycleError> {
    let (p, state) = read(tx, id)?;
    if state != "armed" {
        return Err(LifecycleError::Conflict);
    }
    let (_, e, identities) = current_residency(tx, s, &p)?;
    let execution = p
        .execution
        .clone()
        .ok_or(LifecycleError::CorruptStoredData)?;
    let ttl = policy(tx, &e)?.controls.observation_ttl_ms;
    let mut observed = o.identities.clone();
    observed.sort();
    if o.token != p.token()
        || o.binding_id != p.binding_id
        || o.incarnation != p.incarnation
        || observed != identities
        || o.facts != p.kind.facts()
    {
        return Err(LifecycleError::Rejected(
            "residency evidence does not match the retained launch".into(),
        ));
    }
    fresh(
        execution.issued_at_ms,
        p.deadline_ms,
        o.observed_at_ms,
        now,
        ttl,
    )?;
    let f = footprints(&e);
    let (held, next) = match p.kind {
        ResidencyKind::Park => (&f.parking, &f.parked),
        ResidencyKind::Restore => (&f.wake, &f.ready),
    };
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    if !ledger
        .owners
        .get(&p.owner())
        .is_some_and(|current| same(current, held))
        || ledger.epoch <= execution.expected_epoch
    {
        return Err(LifecycleError::Conflict);
    }
    if p.kind == ResidencyKind::Park {
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2)",
            params![p.deployment_id, p.instance_index],
            |r| r.get(0),
        )?;
        if leased {
            return Err(LifecycleError::Conflict);
        }
    }
    let evidence = encode(&completion_value(&CompletionEvidence {
        token: p.token(),
        identities: identities.clone(),
        observed_at_ms: o.observed_at_ms,
        control_receipt: Some(o.receipt.clone()),
        milestones: o.facts.clone(),
    })?)?;
    let epoch = resource_ledger::advance_completion_epoch(tx)?;
    one(tx.execute(
        "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
        params![p.owner(), resource_ledger::encode(next).map_err(resource)?],
    )?)?;
    match p.kind {
        // SPEC §6.1 PARKED: initialized runtime retained, release verified.
        ResidencyKind::Park => one(tx.execute(
            "UPDATE deployment_instances SET observed_state='parked',dispatch_enabled=0 WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND observed_state='ready'",
            params![p.deployment_id, p.instance_index, p.generation],
        )?)?,
        // SPEC §6.1: only a usable model reopens dispatch. SPEC §13.2, ADR 0015
        // amendment: and only while no closure reason remains for the
        // incarnation (a host-session or engine-exit closure made during the
        // restore holds).
        ResidencyKind::Restore => one(tx.execute(
            &format!(
                "UPDATE deployment_instances AS i SET observed_state='ready',
                        dispatch_enabled=CASE WHEN i.desired_state='ready' AND i.admission_enabled=1 AND NOT EXISTS(SELECT 1 FROM deployments d WHERE d.id=?1 AND d.suspended=1) AND {} THEN 1 ELSE 0 END
                  WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.generation=?3 AND i.observed_state='parked'",
                crate::switch_state::no_closure_clause("i")
            ),
            params![p.deployment_id, p.instance_index, p.generation],
        )?)?,
    }
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
        [id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    tx.execute(
        "INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch) VALUES(?1,?2,?3)",
        params![id, evidence, epoch],
    )?;
    one(tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.operation_id],
    )?)?;
    journal(
        tx,
        &p.operation_id,
        match p.kind {
            ResidencyKind::Park => "parked",
            ResidencyKind::Restore => "restored",
        },
        &format!(
            "deployment {}: instance {} {} on its own host with its recorded group ({}) at {}",
            p.deployment_id,
            p.instance_index,
            match p.kind {
                ResidencyKind::Park => "parked",
                ResidencyKind::Restore => "restored and probed",
            },
            redact_reason(&o.receipt),
            o.observed_at_ms
        ),
    )?;
    record(tx, s, &p, Stage::Completed, Some(epoch))
}

fn refuse(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    reason: &str,
) -> Result<(), LifecycleError> {
    let (p, state) = read(tx, id)?;
    if state != "armed" {
        return Err(LifecycleError::Conflict);
    }
    let (_, e, _) = current_residency(tx, s, &p)?;
    let f = footprints(&e);
    let (held, back) = match p.kind {
        ResidencyKind::Park => (&f.parking, &f.ready),
        ResidencyKind::Restore => (&f.wake, &f.parked),
    };
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    if !ledger
        .owners
        .get(&p.owner())
        .is_some_and(|current| same(current, held))
    {
        return Err(LifecycleError::Conflict);
    }
    let epoch = resource_ledger::advance_completion_epoch(tx)?;
    one(tx.execute(
        "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
        params![p.owner(), resource_ledger::encode(back).map_err(resource)?],
    )?)?;
    let code = format!("{}_refused", p.kind.operation_kind());
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='armed'",
        [id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='failed',error_code=?2 WHERE id=?1 AND state='running'",
        params![p.operation_id, code],
    )?)?;
    one(tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.operation_id],
    )?)?;
    let gate = if p.kind == ResidencyKind::Park && !remote(tx, &p.binding_id)? {
        reopen(tx, &p)?;
        "dispatch reopened"
    } else if p.kind == ResidencyKind::Park {
        "the host's gate stays closed until a fresh model probe reopens it"
    } else {
        "the launch stays parked"
    };
    journal(
        tx,
        &p.operation_id,
        &code,
        &format!(
            "deployment {}: instance {} {} refused before any effect ({}); {gate}",
            p.deployment_id,
            p.instance_index,
            p.kind.verb(),
            redact_reason(reason)
        ),
    )?;
    record(tx, s, &p, Stage::Refused, Some(epoch))
}

/// SPEC §6.3, §13.2: a stop takes over an instance's residency work. A park or
/// restore that never armed is closed (nothing was sent); an uncertain one
/// stays uncertain and its claim moves to the stop, whose gone evidence
/// settles it (`settle_after_cleanup`). An armed one is still its effect's,
/// so the stop is refused until it settles.
pub(super) fn yield_to_stop(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    deployment: &str,
    instance: u32,
) -> Result<(), LifecycleError> {
    let open: Vec<(String, String)> = tx
        .prepare(
            "SELECT s.id,s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN lifecycle_runs r ON r.operation_id=s.operation_id
              WHERE o.kind IN ('park','restore') AND r.deployment_id=?1 AND r.instance_index=?2 AND s.state IN ('planned','armed','uncertain')",
        )?
        .query_map(params![deployment, instance], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, state) in open {
        let (p, _) = read(tx, &id)?;
        match state.as_str() {
            "planned" => {
                one(tx.execute(
                    "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='planned'",
                    [&id],
                )?)?;
                one(tx.execute("UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state='queued'", [&p.operation_id])?)?;
                one(tx.execute(
                    "UPDATE operations SET state='failed',error_code=?2 WHERE id=?1 AND state='pending'",
                    params![p.operation_id, format!("{}_superseded_by_stop", p.kind.operation_kind())],
                )?)?;
                tx.execute(
                    "DELETE FROM lifecycle_claims WHERE operation_id=?1",
                    [&p.operation_id],
                )?;
                record(tx, s, &p, Stage::Cancelled, None)?;
            }
            "uncertain" => {
                tx.execute(
                    "DELETE FROM lifecycle_claims WHERE operation_id=?1",
                    [&p.operation_id],
                )?;
                journal(
                    tx,
                    &p.operation_id,
                    "residency_handed_to_stop",
                    &format!(
                        "deployment {deployment}: instance {instance} {} is uncertain; a stop takes it over and settles it only on gone evidence",
                        p.kind.verb()
                    ),
                )?;
            }
            _ => return Err(LifecycleError::Conflict),
        }
    }
    Ok(())
}

/// After a stop's verified cleanup of `binding`, an uncertain park or restore
/// it took over is settled: the group it acted on is proven gone.
pub(super) fn settle_after_cleanup(
    tx: &Transaction<'_>,
    binding: &str,
) -> Result<(), LifecycleError> {
    let open: Vec<(String, String)> = tx
        .prepare(
            "SELECT s.id,s.operation_id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              WHERE o.kind IN ('park','restore') AND s.binding_id=?1 AND s.state='uncertain'",
        )?
        .query_map([binding], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, operation) in open {
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='uncertain'",
            [&id],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state='uncertain'",
            [&operation],
        )?)?;
        one(tx.execute(
            "UPDATE operations SET state='failed',error_code='resolved_by_owned_cleanup' WHERE id=?1 AND state='running'",
            [&operation],
        )?)?;
    }
    Ok(())
}

/// SPEC §13.2 (W5): a restarted coordinator adopting an instance's launch
/// takes its residency work too. A park or restore that never armed is closed
/// (nothing was sent). One that was armed when the session ended is already
/// `uncertain` (session start); it moves to this session unchanged, with its
/// reservation and claim, and is never re-armed: a stop settles it.
pub(super) fn adopt_residency(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    deployment: &str,
    instance: u32,
) -> Result<(), LifecycleError> {
    let open: Vec<(String, String)> = tx
        .prepare(
            "SELECT s.id,s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN lifecycle_runs r ON r.operation_id=s.operation_id
              WHERE o.kind IN ('park','restore') AND r.deployment_id=?1 AND r.instance_index=?2
                AND s.state IN ('planned','uncertain') AND s.session_id!=?3",
        )?
        .query_map(params![deployment, instance, s.id()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, state) in open {
        let (mut p, _) = read(tx, &id)?;
        let retired = std::mem::replace(&mut p.session_id, s.id().into());
        if state == "planned" {
            one(tx.execute(
                "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='planned'",
                [&id],
            )?)?;
            one(tx.execute(
                "UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state='queued'",
                [&p.operation_id],
            )?)?;
            one(tx.execute(
                "UPDATE operations SET state='failed',error_code=?2 WHERE id=?1 AND state='pending'",
                params![p.operation_id, format!("{}_superseded_by_restart", p.kind.operation_kind())],
            )?)?;
            tx.execute(
                "DELETE FROM lifecycle_claims WHERE operation_id=?1",
                [&p.operation_id],
            )?;
            continue;
        }
        one(tx.execute(
            "UPDATE lifecycle_steps SET session_id=?2,step_json=?3 WHERE id=?1 AND session_id=?4",
            params![id, s.id(), encode(&p)?, retired],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET session_id=?2 WHERE operation_id=?1 AND session_id=?3",
            params![p.operation_id, s.id(), retired],
        )?)?;
        journal(
            tx,
            &p.operation_id,
            "residency_adopted",
            &format!(
                "deployment {deployment}: instance {instance} {} was in flight when the controller stopped; it stays uncertain with its accounting until a stop proves the group gone",
                p.kind.verb()
            ),
        )?;
    }
    Ok(())
}

impl crate::Store {
    /// SPEC §6.5 (W5): before a planned cold start arms, reclaim least
    /// recently parked instances on its host if that is what it takes to
    /// fit. `Some(reason)` when the start must wait (stops were accepted, or
    /// earlier ones are still running); `None` when it fits now or cannot fit
    /// by reclamation at all (the arm then refuses with its own reason).
    pub fn reclaim_for_start(
        &self,
        s: &CoordinatorSession,
        step_id: &str,
        context: AdmissionContext<'_>,
    ) -> Result<Option<String>, LifecycleError> {
        self.reclaim_for_start_with_residents(s, step_id, context, &[])
    }

    /// As [`Self::reclaim_for_start`], crediting Ready engines on the host
    /// with the memory `residents` attributes to their own processes, so a
    /// start that fits beside them reclaims nothing (ADR 0007).
    pub fn reclaim_for_start_with_residents(
        &self,
        s: &CoordinatorSession,
        step_id: &str,
        context: AdmissionContext<'_>,
        residents: &[mllm_domain::resources::ProcessResident],
    ) -> Result<Option<String>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, e, state) = load(&tx, step_id)?;
        if state != "planned" {
            return Ok(None);
        }
        current(&tx, s, &p, false)?;
        let ledger = resource_ledger::read_snapshot(&tx).map_err(resource)?;
        let scoped = resource_ledger::scoped_to_domain_hosts(
            &tx,
            &ledger,
            context.limits.iter().map(|l| l.domain.as_str()),
        )
        .map_err(resource)?;
        let cold = super::startup::cold(&p, &e);
        // ADR 0019: each domain credited by its kind. A policy that does not
        // read here credits nothing; the start's own arm judges the policy.
        let kinds = policy(&tx, &e)
            .map(|policy| crate::resident_floors::domain_kinds(&policy.controls))
            .unwrap_or_default();
        let floors = crate::resident_floors::resident_floors(
            &tx,
            &scoped,
            &p.owner(),
            context.observations,
            residents,
            &kinds,
        )?;
        let context = AdmissionContext {
            resident_floors: &floors,
            ..context
        };
        let outcome = match fit(&tx, &scoped, &p.owner(), &[cold], context)? {
            Fit::Fits | Fit::Impossible(_) => None,
            Fit::Waiting => Some("waiting for reclaimed parked instances to stop".to_string()),
            Fit::Reclaim(chosen) => {
                let cause = format!(
                    "the start of deployment {} instance {}",
                    p.deployment_id, p.instance_index
                );
                let stops = reclaim(&tx, s, &chosen, &cause, context.now_ms)?;
                Some(format!(
                    "reclaiming {} parked instance(s) first",
                    stops.len()
                ))
            }
        };
        tx.commit()?;
        Ok(outcome)
    }
}

// --- idle policy -------------------------------------------------------------

/// When the instance last became Ready or parked, by committed evidence.
fn evidence_since(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
    kinds: &str,
) -> Result<Option<i64>, LifecycleError> {
    Ok(tx.query_row(
        &format!(
            "SELECT MAX(CAST(json_extract(e.evidence_json,'$.observed_at_ms') AS INTEGER)) FROM lifecycle_evidence e
               JOIN lifecycle_steps s ON s.id=e.step_id JOIN operations o ON o.id=s.operation_id
               JOIN runtime_bindings b ON b.id=s.binding_id
              WHERE b.deployment_id=?1 AND b.instance_index=?2 AND b.state='live' AND s.state='completed' AND o.kind IN ({kinds})"
        ),
        params![deployment, instance],
        |r| r.get(0),
    )?)
}

impl crate::Store {
    /// SPEC §6.5: the controller-owned idle policy, one pass. A Ready
    /// instance with nothing in flight whose last activity is older than
    /// `ready_idle_ms` parks at its declared tier (a restart-only one, or one
    /// whose park the engine refused, stops instead); a parked instance older
    /// than `parked_idle_ms` stops to reclaim its residual state. Every stop
    /// is ordinary, so automatic activation stays enabled. `activity` is the
    /// router's last request time for an instance generation; `floor` is the
    /// earliest time idleness may be counted from (the worker's start).
    pub fn apply_idle_policy(
        &self,
        s: &CoordinatorSession,
        now: i64,
        policy: IdlePolicy,
        activity: &dyn Fn(&str, i64) -> Option<i64>,
        floor: i64,
    ) -> Result<Vec<IdleAction>, LifecycleError> {
        let mut done = Vec::new();
        if policy.ready_idle_ms.is_none() && policy.parked_idle_ms.is_none() {
            return Ok(done);
        }
        let candidates: Vec<(String, u32, i64, String)> = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, s)?;
            // SPEC §6.5: a warm-residency commitment is never idled.
            let rows = tx.prepare(&format!(
                "SELECT i.deployment_id,i.instance_index,i.generation,i.observed_state FROM deployment_instances i
                   JOIN deployments d ON d.id=i.deployment_id
                  WHERE d.kind='model' AND d.suspended=0 AND i.state='active' AND i.generation IS NOT NULL
                    AND ((i.observed_state='ready' AND i.dispatch_enabled=1) OR i.observed_state='parked')
                    AND NOT EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.state IN ('queued','running','uncertain'))
                    AND NOT EXISTS(SELECT 1 FROM lifecycle_claims c WHERE c.deployment_id=i.deployment_id AND c.instance_index=i.instance_index)
                    AND NOT EXISTS(SELECT 1 FROM request_leases l WHERE l.deployment_id=i.deployment_id AND l.instance_index=i.instance_index)
                    AND NOT {}
                  ORDER BY i.deployment_id,i.instance_index",
                crate::switch_state::warm_clause("i")
            ))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
            rows
        };
        for (deployment, instance, generation, observed) in candidates {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            let outcome = idle_one(
                &tx,
                s,
                &deployment,
                instance,
                generation,
                &observed,
                now,
                policy,
                activity,
                floor,
            );
            match outcome {
                Ok(Some(action)) => {
                    tx.commit()?;
                    done.push(action);
                }
                Ok(None) => {}
                Err(LifecycleError::Sql(error)) => return Err(LifecycleError::Sql(error)),
                Err(LifecycleError::CorruptStoredData) => {
                    return Err(LifecycleError::CorruptStoredData)
                }
                // Refused now (a racing start or stop): a later pass decides.
                Err(_) => {}
            }
        }
        Ok(done)
    }
}

#[allow(clippy::too_many_arguments)]
fn idle_one(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    deployment: &str,
    instance: u32,
    generation: i64,
    observed: &str,
    now: i64,
    policy: IdlePolicy,
    activity: &dyn Fn(&str, i64) -> Option<i64>,
    floor: i64,
) -> Result<Option<IdleAction>, LifecycleError> {
    let (source, e, _) = launch(tx, deployment, instance)?;
    if source.generation != generation {
        return Ok(None);
    }
    let parked = observed == "parked";
    let (limit, kinds) = if parked {
        (policy.parked_idle_ms, "'park'")
    } else {
        (policy.ready_idle_ms, "'initialize','restore'")
    };
    let Some(limit) = limit else {
        return Ok(None);
    };
    let since = evidence_since(tx, deployment, instance, kinds)?
        .unwrap_or(floor)
        .max(floor)
        .max(if parked {
            i64::MIN
        } else {
            activity(deployment, generation).unwrap_or(i64::MIN)
        });
    if now.saturating_sub(since) < limit {
        return Ok(None);
    }
    let deadline = now.saturating_add(e.request_deadline_ms);
    let refused_park: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations o JOIN lifecycle_runs r ON r.operation_id=o.id
          WHERE o.deployment_id=?1 AND r.instance_index=?2 AND r.generation=?3 AND o.kind='park'
            AND o.state='failed' AND o.error_code IN ('park_refused','park_parked_capacity'))",
        params![deployment, instance, generation],
        |r| r.get(0),
    )?;
    if !parked && parks(&e) && !refused_park {
        let key = format!("idle-park:{generation}:{since}");
        let receipt = crate::Store::instance_park_in_transaction(
            tx, s, "idle", deployment, instance, &key, now, deadline,
        )?;
        journal(
            tx,
            &receipt.operation_id,
            "idle_park",
            &format!("deployment {deployment}: instance {instance} was ready and idle since {since}; it parks and stays eligible for on-demand activation"),
        )?;
        return Ok(Some(IdleAction::Parked {
            deployment_id: deployment.into(),
            instance,
            operation_id: receipt.operation_id,
        }));
    }
    let reason = if parked {
        "parked_idle"
    } else if refused_park {
        "ready_idle_park_refused"
    } else {
        "ready_idle_restart_only"
    };
    let revision: i64 = tx.query_row(
        "SELECT revision FROM deployments WHERE id=?1",
        [deployment],
        |r| r.get(0),
    )?;
    let receipt = crate::Store::accept_instance_stop_in_transaction(
        tx,
        s,
        "idle",
        &source.fence(),
        &format!("idle-stop:{reason}:{generation}:{since}"),
        now,
        deadline,
        &StopCommand {
            scope: Some(instance_scope(deployment, instance)),
            revision: Some(revision),
        },
    )?;
    journal(
        tx,
        &receipt.operation_id,
        "idle_stop",
        &format!("deployment {deployment}: instance {instance} idle since {since} ({reason}); an ordinary stop leaves it eligible for on-demand activation"),
    )?;
    Ok(Some(IdleAction::Stopped {
        deployment_id: deployment.into(),
        instance,
        operation_id: receipt.operation_id,
        reason,
    }))
}

// --- preinitialize ------------------------------------------------------------

fn preinitialize_scope(deployment: &str) -> String {
    residency_scope(deployment, None, "preinitialize")
}

/// The receipt of an open or finished preinitialize operation.
fn preinitialize_receipt(
    tx: &Transaction<'_>,
    operation: &str,
) -> Result<PreinitializeReceipt, LifecycleError> {
    let raw: String = tx
        .query_row(
            "SELECT response_json FROM command_receipts WHERE operation_id=?1 AND command_scope LIKE '%#preinitialize' ORDER BY rowid LIMIT 1",
            [operation],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(LifecycleError::CorruptStoredData)?;
    decode(&raw)
}

impl crate::Store {
    /// SPEC §6.3, §6.5 `preinitialize deployment`: for each instance in turn,
    /// start, verify (Ready is model readiness) and park at the declared tier,
    /// then continue with the next. Refused (`Unsupported`) for a restart-only
    /// deployment or a host that opted out of deep parking: it must fail
    /// rather than claim a restart-only deployment is prewarmed. The worker
    /// advances it (`advance_preinitialize`); an open one is joined.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_preinitialize_command(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        expected_revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<PreinitializeReceipt, LifecycleError> {
        super::receipt::check_request(principal, deployment, expected_revision, key, deadline)?;
        if now < 0 || deadline <= now {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let scope = preinitialize_scope(deployment);
        let hash = request_hash(
            principal,
            &scope,
            expected_revision,
            "preinitialize",
            deadline,
        )?;
        if let Some(receipt) = lookup(&tx, principal, &scope, key, &hash)? {
            return Ok(receipt);
        }
        super::receipt::command_revision(&tx, deployment, expected_revision)?;
        super::check_managed_command_target(&tx, deployment)?;
        if !parks(&declared(&tx, deployment)?) {
            return Err(LifecycleError::Unsupported);
        }
        let open: Option<String> = tx
            .query_row(
                "SELECT id FROM operations WHERE deployment_id=?1 AND kind='preinitialize' AND state='running' ORDER BY accepted_at,id LIMIT 1",
                [deployment],
                |r| r.get(0),
            )
            .optional()?;
        let receipt = match open {
            Some(operation) => PreinitializeReceipt {
                joined: true,
                ..preinitialize_receipt(&tx, &operation)?
            },
            None => {
                let operation = ulid::Ulid::new().to_string();
                tx.execute(
                    "INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'preinitialize','running')",
                    params![operation, deployment],
                )?;
                journal(
                    &tx,
                    &operation,
                    "preinitialize_accepted",
                    &format!("deployment {deployment}: each instance is started, verified and parked in turn"),
                )?;
                PreinitializeReceipt {
                    operation_id: operation,
                    deployment_id: deployment.into(),
                    revision: expected_revision,
                    accepted_at_ms: now,
                    deadline_ms: deadline,
                    joined: false,
                }
            }
        };
        store_receipt(
            &tx,
            principal,
            &scope,
            key,
            &hash,
            &receipt.operation_id,
            &receipt,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Advance every open preinitialize one step (SPEC §6.5): nothing while
    /// any instance of its deployment has work in flight; otherwise start the
    /// lowest instance that is stopped, or park the lowest that is Ready; done
    /// when every instance the operator did not stop is parked. A start that
    /// does not fit waits (it never displaces active work); a failed start or
    /// a refused park fails the operation, as does its deadline.
    pub fn advance_preinitialize(
        &self,
        s: &CoordinatorSession,
        now: i64,
        eligible: super::placement::Eligible<'_>,
    ) -> Result<Vec<PreinitializeProgress>, LifecycleError> {
        let open: Vec<(String, String)> = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, s)?;
            let rows = tx.prepare(
                "SELECT id,deployment_id FROM operations WHERE kind='preinitialize' AND state='running' ORDER BY accepted_at,id LIMIT 16",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
            rows
        };
        let mut progress = Vec::new();
        for (operation, deployment) in open {
            if let Some(step) = self.advance_one(s, &operation, &deployment, now, eligible)? {
                progress.push(step);
            }
        }
        Ok(progress)
    }

    fn advance_one(
        &self,
        s: &CoordinatorSession,
        operation: &str,
        deployment: &str,
        now: i64,
        eligible: super::placement::Eligible<'_>,
    ) -> Result<Option<PreinitializeProgress>, LifecycleError> {
        let finish = |outcome: &'static str,
                      error: Option<&str>|
         -> Result<Option<PreinitializeProgress>, LifecycleError> {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            let state = if error.is_some() {
                "failed"
            } else {
                "succeeded"
            };
            one(tx.execute(
                "UPDATE operations SET state=?2,error_code=?3 WHERE id=?1 AND state='running'",
                params![operation, state, error],
            )?)?;
            journal(
                &tx,
                operation,
                &format!("preinitialize_{outcome}"),
                &format!("deployment {deployment}: preinitialize {outcome}"),
            )?;
            tx.commit()?;
            Ok(Some(PreinitializeProgress::Finished {
                operation_id: operation.into(),
                deployment_id: deployment.into(),
                outcome,
            }))
        };
        // (instance, observed, holds a binding, open run, operator stopped, admitting)
        type Row = (u32, String, bool, bool, bool, bool);
        let (receipt, rows): (PreinitializeReceipt, Vec<Row>) = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, s)?;
            let receipt = preinitialize_receipt(&tx, operation)?;
            let rows = tx
                .prepare(
                    "SELECT i.instance_index,i.observed_state,
                            EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index AND b.state!='released'),
                            EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.state IN ('queued','running','uncertain'))
                              OR i.pending_start_until_ms IS NOT NULL,
                            i.operator_stopped=1, i.admission_enabled=1
                       FROM deployment_instances i WHERE i.deployment_id=?1 AND i.state='active' ORDER BY i.instance_index",
                )?
                .query_map([deployment], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<Result<_, _>>()?;
            (receipt, rows)
        };
        if now >= receipt.deadline_ms {
            return finish("deadline", Some("preinitialize_deadline"));
        }
        // Sequential: one instance's work at a time.
        if rows.iter().any(|r| r.3) {
            return Ok(None);
        }
        let Some(next) = rows.iter().find(|r| !r.4 && r.1 != "parked") else {
            return finish("succeeded", None);
        };
        let (instance, observed, holds, _, _, admitting) = next.clone();
        let start_key = format!("preinitialize:{operation}:{instance}:start");
        let park_key = format!("preinitialize:{operation}:{instance}:park");
        let revision = receipt.revision;
        if observed == "ready" && holds {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            let park = crate::Store::instance_park_in_transaction(
                &tx,
                s,
                "preinitialize",
                deployment,
                instance,
                &park_key,
                now,
                receipt.deadline_ms,
            );
            match park {
                // The same key replays an earlier park: one that failed (a
                // refused park, say) fails the preinitialize, never retried.
                Ok(park) => {
                    let state: String = tx.query_row(
                        "SELECT state FROM operations WHERE id=?1",
                        [&park.operation_id],
                        |r| r.get(0),
                    )?;
                    tx.commit()?;
                    if state == "failed" {
                        return finish("park_failed", Some("preinitialize_park_failed"));
                    }
                    Ok(Some(PreinitializeProgress::Parking {
                        operation_id: operation.into(),
                        deployment_id: deployment.into(),
                        instance,
                    }))
                }
                Err(LifecycleError::Unsupported) => {
                    drop(tx);
                    finish("unsupported", Some("unsupported_parking"))
                }
                Err(LifecycleError::Sql(error)) => Err(LifecycleError::Sql(error)),
                Err(_) => Ok(None),
            }
        } else if observed == "stopped" && !holds {
            let accepted = {
                let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
                check_session(&tx, s)?;
                let scope = instance_scope(deployment, instance);
                tx.query_row(
                    "SELECT operation_id FROM command_receipts WHERE principal_id='preinitialize' AND command_scope=?1 AND idempotency_key=?2",
                    params![scope, start_key],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            };
            if let Some(start) = accepted {
                // This preinitialize already started the instance; it is
                // stopped again, so that start failed or was stopped.
                let _ = start;
                if !admitting {
                    return finish("start_failed", Some("preinitialize_start_failed"));
                }
                return finish("start_stopped", Some("preinitialize_interrupted"));
            }
            match self.accept_scoped_start_command(
                s,
                "preinitialize",
                deployment,
                super::placement::StartScope::Instance(instance),
                revision,
                &start_key,
                now,
                receipt.deadline_ms,
                eligible,
            ) {
                Ok(start) => Ok(Some(PreinitializeProgress::Started {
                    operation_id: operation.into(),
                    deployment_id: deployment.into(),
                    instance: {
                        let _ = start;
                        instance
                    },
                })),
                // SPEC §6.5: deferred while it would displace active work.
                Err(LifecycleError::CapacityBlocked) => Ok(None),
                Err(LifecycleError::RevisionConflict) => {
                    finish("revision_changed", Some("preinitialize_revision_changed"))
                }
                Err(LifecycleError::Sql(error)) => Err(LifecycleError::Sql(error)),
                Err(LifecycleError::CorruptStoredData) => Err(LifecycleError::CorruptStoredData),
                Err(_) => Ok(None),
            }
        } else {
            // Failed or uncertain launch: the operator resolves it.
            finish("blocked", Some("preinitialize_instance_unavailable"))
        }
    }
}

#[cfg(test)]
#[path = "park_tests.rs"]
mod tests;
