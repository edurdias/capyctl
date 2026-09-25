//! Explicit Stop authority for the original owned Fake incarnation.
//! An arm permits one control only after the worker has awaited predecessor exit.
use super::unarmed_stop::OrdinaryStopReceipt;
use super::*;
use crate::events::OrdinaryCleanupTransition;
use crate::lifecycle::completion::nonempty_receipt;
use mllm_domain::completion::{CleanupEvidence, ProcessIdentity};
use sha2::{Digest, Sha256};

#[path = "cleanup_worker.rs"]
mod worker;
pub use worker::OrdinaryCleanupStatus;
// W12: a restarted coordinator resumes a Stop its retired session accepted.
#[path = "cleanup_adoption.rs"]
mod adoption;
pub use adoption::RetiredCleanup;
// Owner decision 2026-09-22: an expired, never-armed drain Stop is closed and
// issued afresh when its host reconnects.
#[path = "cleanup_reissue.rs"]
mod reissue;
pub use reissue::ReissuedDrainStop;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupMode {
    TerminateOwned,
    /// A cleanup whose terminate already went out; only inspection follows.
    /// The ordinary path does not yet arm this variant — SPEC §13.2 restart
    /// recovery will. It stays because the spec requires it even without a
    /// caller yet.
    InspectOwnedGone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupExecutionContext {
    pub operation_id: String,
    pub step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub fence: DeploymentFence,
    pub identities: Vec<ProcessIdentity>,
    pub issued_at_ms: i64,
    pub deadline_ms: i64,
    pub mode: CleanupMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrdinaryCleanupReceipt {
    pub operation_id: String,
    pub step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub revision: i64,
    pub generation: i64,
    pub accepted_at_ms: i64,
    pub deadline_ms: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CleanupKind {
    OrdinaryCleanup,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceKind {
    OrdinaryOwnedCleanup,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Gone {
    version: u8,
    kind: EvidenceKind,
    step_id: String,
    binding_id: String,
    incarnation: String,
    identities: Vec<IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
}
fn evidence_value(id: &str, e: &CleanupEvidence) -> Result<String, LifecycleError> {
    nonempty_receipt(&e.receipt)?;
    encode(&Gone {
        version: 1,
        kind: EvidenceKind::OrdinaryOwnedCleanup,
        step_id: id.into(),
        binding_id: e.binding_id.clone(),
        incarnation: e.incarnation.clone(),
        // SPEC §6: an unassociated uncertain launch can only have recorded nothing
        // or its API process; an associated one its full canonical group. The
        // guard in `complete` compares against whichever the binding holds.
        identities: identity_dtos(&super::failed_launch::canonical_members_or_empty(
            &e.identities,
        )?),
        observed_at_ms: e.observed_at_ms,
        receipt: e.receipt.clone(),
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupPlan {
    version: u8,
    kind: CleanupKind,
    principal: String,
    key: String,
    receipt: OrdinaryCleanupReceipt,
    source: Plan,
    source_state: String,
    /// Absent only for an uncertain launch that never reached association: a
    /// remote launch whose Initialize outcome was lost (SPEC §13.2). Such a
    /// cleanup must prove gone exactly the identities the binding recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    association: Option<Association>,
    issued_at_ms: Option<i64>,
    /// Owner decision Q7: the command scope the receipt was stored under when
    /// it is an instance's own (`stop instance`, or a sibling of a
    /// deployment-level stop). Absent means the deployment's scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    /// ADR 0013 §7: the deployment revision the command named, when the
    /// instance still runs an earlier one (a count-only revision leaves running
    /// instances untouched). Absent means the source's own revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_revision: Option<i64>,
    /// Owner decision 2026-09-22: the expired, never-armed drain Stop this one
    /// was issued in place of. Its claim was handed to this Stop, so it is this
    /// Stop's predecessor; absent means the source launch is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reissue_of: Option<String>,
}
impl CleanupPlan {
    fn command_scope(&self) -> String {
        self.scope
            .clone()
            .unwrap_or_else(|| scope(&self.source.deployment_id))
    }
    fn command_revision(&self) -> i64 {
        self.command_revision.unwrap_or(self.source.revision)
    }
}

/// How a stop command addresses one instance: the receipt's scope (absent: the
/// deployment's) and the revision the command named (absent: the fence's).
#[derive(Clone, Debug, Default)]
pub(crate) struct StopCommand {
    pub(crate) scope: Option<String>,
    pub(crate) revision: Option<i64>,
}
impl StopCommand {
    fn for_fence(&self, fence: &DeploymentFence) -> Self {
        Self {
            scope: self.scope.clone(),
            revision: self.revision.filter(|revision| *revision != fence.revision),
        }
    }
}

/// The identities a cleanup must prove gone: the association's canonical group,
/// or, for an unassociated uncertain launch, exactly what its binding recorded.
fn expected_identities(
    tx: &Transaction<'_>,
    p: &CleanupPlan,
) -> Result<Vec<ProcessIdentity>, LifecycleError> {
    match &p.association {
        Some(association) => members(&association.identities),
        None => super::failed_launch::recorded_identities(tx, &p.receipt.binding_id),
    }
}

/// SPEC §6.1, §13.2, AGENTS.md (uncertainty retains accounting): gone evidence
/// for an empty set proves nothing by itself. Only an authenticated host can
/// report that a launch it holds was never released; an embedded launch whose
/// arm consumed its one spawn attempt has no such witness, so an unassociated
/// one that recorded no identity is never released on vacuous evidence.
fn vacuous_release(
    tx: &Transaction<'_>,
    p: &CleanupPlan,
    expected: &[ProcessIdentity],
) -> Result<bool, LifecycleError> {
    Ok(p.association.is_none()
        && expected.is_empty()
        && p.source.execution.is_some()
        && !is_remote(tx, &p.receipt.binding_id)?)
}

/// Whether a binding is served through an enrolled host's ingress.
fn is_remote(tx: &Transaction<'_>, binding_id: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_binding_ingress WHERE binding_id=?1)",
        [binding_id],
        |r| r.get(0),
    )?)
}

pub(super) fn scope(deployment: &str) -> String {
    format!("POST:/management/v1/deployments/{deployment}/actions")
}
/// Owner decision Q7: the command scope of one instance's own stop, and of
/// every sibling a deployment-level stop fans out to beyond its first.
pub(crate) fn instance_scope(deployment: &str, instance: u32) -> String {
    format!("POST:/management/v1/deployments/{deployment}/instances/{instance}/actions")
}
pub(super) fn hash_in(
    principal: &str,
    scope: &str,
    revision: i64,
    deadline: i64,
) -> Result<String, LifecycleError> {
    // Generation is a service-resolved acceptance fence, not a request-body field.
    Ok(format!(
        "{:x}",
        Sha256::digest(encode(&(1, principal, scope, revision, "stop", deadline))?.as_bytes())
    ))
}
fn target(p: &CleanupPlan) -> DeploymentFence {
    DeploymentFence {
        deployment_id: p.source.deployment_id.clone(),
        revision: p.receipt.revision,
        generation: p.receipt.generation,
    }
}
fn context(
    tx: &Transaction<'_>,
    p: &CleanupPlan,
) -> Result<CleanupExecutionContext, LifecycleError> {
    Ok(CleanupExecutionContext {
        operation_id: p.receipt.operation_id.clone(),
        step_id: p.receipt.step_id.clone(),
        binding_id: p.receipt.binding_id.clone(),
        incarnation: p.receipt.incarnation.clone(),
        fence: target(p),
        identities: expected_identities(tx, p)?,
        issued_at_ms: p.issued_at_ms.ok_or(LifecycleError::Conflict)?,
        deadline_ms: p.receipt.deadline_ms,
        mode: CleanupMode::TerminateOwned,
    })
}

fn event(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &CleanupPlan,
    transition: OrdinaryCleanupTransition,
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
        &EventMetadata::OrdinaryCleanupRecorded {
            transition,
            operation_id: id(&p.receipt.operation_id)?,
            deployment_id: id(&p.source.deployment_id)?,
            step_id: id(&p.receipt.step_id)?,
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

/// Validate immutable source authority without consulting today's qualification policy.
fn source(tx: &Transaction<'_>, p: &Plan) -> Result<EffectiveDeployment, LifecycleError> {
    if p.version != 1 || p.accepted_at_ms < 0 || p.deadline_ms <= p.accepted_at_ms {
        return Err(LifecycleError::CorruptStoredData);
    }
    for id in [
        &p.operation_id,
        &p.step_id,
        &p.deployment_id,
        &p.binding_id,
        &p.incarnation,
        &p.session_id,
    ] {
        if ulid::Ulid::from_string(id).is_err() {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    let e = decode_effective_snapshot(&p.effective_json)
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    super::validate_frozen(tx, &p.deployment_id, p.revision, &p.effective_json)
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    let exact: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND s.operation_id=?2 AND s.deployment_id=?3 AND s.binding_id=?4 AND s.session_id=?5 AND s.ordinal=0 AND s.step_json=?6 AND o.kind='initialize' AND o.deployment_id=?3) AND (EXISTS(SELECT 1 FROM effective_revisions WHERE deployment_id=?3 AND revision=?7 AND effective_json=?8 AND fingerprint=?9) OR EXISTS(SELECT 1 FROM host_effective_revisions WHERE deployment_id=?3 AND revision=?7 AND outcome='resolved' AND effective_json=?8 AND fingerprint=?9)) AND EXISTS(SELECT 1 FROM operations WHERE deployment_id=?3 AND kind='managed_configuration_create' AND state='succeeded') AND (SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?2)=1", params![p.step_id,p.operation_id,p.deployment_id,p.binding_id,p.session_id,encode(p)?,p.revision,p.effective_json,e.recipe_fingerprint], |r|r.get(0))?;
    // Re-derive the identity this binding must carry rather than matching the
    // qualified spelling of it. A restart-only deployment is identified by its
    // recipe and host, and cleanup is engine-agnostic anyway: it proves the
    // recorded processes are gone, which is the same proof whatever started them.
    let identity = super::binding_identity(tx, &p.deployment_id, p.revision, &e)?;
    let b: BindingDto = decode(&p.binding_json)?;
    if !exact
        || b.version != 1
        || b.identity_id != identity.id()
        || b.payload != identity.payload()?
        || Some(&b.credential_ref) != e.profile.security.credential_ref.as_ref()
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let execution = p.execution.as_ref().ok_or(LifecycleError::Conflict)?;
    if execution.issued_at_ms < p.accepted_at_ms
        || execution.issued_at_ms >= p.deadline_ms
        || execution.policy_revision < 1
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let footprint = resource_ledger::encode(&super::startup::cold(p, &e)).map_err(resource)?;
    let request = encode(&(
        &p.deployment_id,
        &p.operation_id,
        p.revision,
        p.generation,
        execution.expected_epoch,
        &footprint,
    ))?;
    let grant:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM resource_grants WHERE id=?1 AND deployment_id=?2 AND operation_id=?3 AND request_json=?4 AND committed_epoch=?5) AND EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?6 AND grant_id=?1) AND (SELECT COUNT(*) FROM resource_grants WHERE operation_id=?3)=1",params![execution.grant_id,p.deployment_id,p.operation_id,request,execution.expected_epoch.checked_add(1).ok_or(LifecycleError::CorruptStoredData)?,p.step_id],|r|r.get(0))?;
    if !grant {
        return Err(LifecycleError::CorruptStoredData);
    }
    let binding:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND incarnation=?4 AND ownership='managed' AND binding_json=?5 AND instance_index=?6)",params![p.binding_id,p.deployment_id,p.revision,p.incarnation,p.binding_json,p.instance_index],|r|r.get(0))?;
    if !binding {
        return Err(LifecycleError::CorruptStoredData);
    }
    let association = super::association(tx, p)?;
    if association.as_ref().is_some_and(|association| {
        association.observed_at_ms < execution.issued_at_ms
            || association.observed_at_ms > p.deadline_ms
    }) {
        return Err(LifecycleError::CorruptStoredData);
    }
    let state: String = tx.query_row(
        "SELECT state FROM lifecycle_steps WHERE id=?1",
        [&p.step_id],
        |r| r.get(0),
    )?;
    let prior: Option<(String, u64)> = tx
        .query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [&p.step_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if state == "completed" {
        // A completed launch was always associated before its evidence committed.
        let association = association.ok_or(LifecycleError::CorruptStoredData)?;
        let (raw, epoch) = prior.ok_or(LifecycleError::CorruptStoredData)?;
        let stored: crate::lifecycle::completion::CompletionEvidenceV1 = decode(&raw)?;
        let value: serde_json::Value = decode(&raw)?;
        let observed = value["observed_at_ms"]
            .as_i64()
            .ok_or(LifecycleError::CorruptStoredData)?;
        let receipt = value["control_receipt"]
            .as_str()
            .ok_or(LifecycleError::CorruptStoredData)?;
        use mllm_domain::completion::Milestone;
        let expected = completion_value(&CompletionEvidence {
            token: p.context(&e)?.token,
            identities: members(&association.identities)?,
            observed_at_ms: observed,
            control_receipt: Some(receipt.into()),
            milestones: vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable,
            ],
        })?;
        if stored != expected
            || observed < execution.issued_at_ms
            || observed > p.deadline_ms
            || epoch <= execution.expected_epoch + 1
            || epoch > resource_ledger::read_snapshot(tx).map_err(resource)?.epoch
        {
            return Err(LifecycleError::CorruptStoredData);
        }
    } else if prior.is_some() || !matches!(state.as_str(), "armed" | "uncertain" | "cancelled") {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(e)
}

fn read(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<(CleanupPlan, EffectiveDeployment, String), LifecycleError> {
    let (raw,state):(String,String)=tx.query_row("SELECT s.step_json,s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='ordinary_cleanup'",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(LifecycleError::Unsupported)?;
    let p: CleanupPlan = decode(&raw)?;
    let r = &p.receipt;
    let original = &p.source;
    let e = source(tx, original)?;
    let bound = e.request_deadline_ms;
    if p.version != 1
        || r.step_id != id
        || r.binding_id != original.binding_id
        || r.incarnation != original.incarnation
        || r.revision != original.revision
        // ADR 0013 §5: the stop's generation is drawn from the deployment's
        // counter, after the source's; only its order is fixed.
        || r.generation <= original.generation
        || r.accepted_at_ms < original.accepted_at_ms
        || r.deadline_ms <= r.accepted_at_ms
        || r.deadline_ms
            .checked_sub(r.accepted_at_ms)
            .is_none_or(|duration| duration > bound)
        || !matches!(p.source_state.as_str(), "armed" | "uncertain" | "completed")
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    for id in [&r.operation_id, &r.step_id] {
        if ulid::Ulid::from_string(id).is_err() {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    let association = super::association(tx, original)?;
    let issued = original
        .execution
        .as_ref()
        .ok_or(LifecycleError::CorruptStoredData)?
        .issued_at_ms;
    if association != p.association
        || association.as_ref().is_some_and(|association| {
            association.observed_at_ms < issued || association.observed_at_ms > original.deadline_ms
        })
        // SPEC §13.2: only a launch retained as uncertain may be cleaned up
        // without an association; every other source was associated first.
        || (association.is_none() && p.source_state != "uncertain")
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    // A reissued Stop took its claim over from the expired one it replaced.
    let predecessor = match &p.reissue_of {
        Some(prior) => {
            reissue::validate_predecessor(tx, prior, &r.binding_id)?;
            Some(prior.as_str())
        }
        None => (p.source_state != "completed").then_some(original.operation_id.as_str()),
    };
    let run = crate::lifecycle::validate_cleanup_run(
        tx,
        &target(&p),
        &r.operation_id,
        &original.session_id,
        r.deadline_ms,
        predecessor,
    )?;
    // Session rotation changes retained armed rows to uncertain. Recognizing
    // that exact durable transition permits receipt observation only: every
    // effectful path separately requires the original session to be current.
    let retired: bool = tx.query_row(
        "SELECT session_id!=?1 FROM coordinator_session WHERE singleton=1",
        [&original.session_id],
        |r| r.get(0),
    )?;
    let expected_operation = match state.as_str() {
        "planned" if run == "queued" && p.issued_at_ms.is_none() => "pending",
        "armed" if run == "running" && p.issued_at_ms.is_some() => "running",
        "uncertain" if retired && run == "uncertain" && p.issued_at_ms.is_some() => "running",
        "completed" if run == "succeeded" && p.issued_at_ms.is_some() => "succeeded",
        // Owner decision 2026-09-22: a drain Stop closed at its deadline before
        // it was ever armed.
        "cancelled" if run == "failed" && p.issued_at_ms.is_none() => "failed",
        _ => return Err(LifecycleError::CorruptStoredData),
    };
    let error_code = (state == "cancelled").then_some(reissue::ERROR_CODE);
    if p.issued_at_ms
        .is_some_and(|issued| issued < r.accepted_at_ms || issued >= r.deadline_ms)
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND operation_id=?2 AND deployment_id=?3 AND binding_id=?4 AND session_id=?5 AND ordinal=0 AND grant_id IS NULL) AND EXISTS(SELECT 1 FROM operations WHERE id=?2 AND deployment_id=?3 AND kind='ordinary_cleanup' AND state=?6 AND error_code IS ?12) AND (SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?2)=1 AND EXISTS(SELECT 1 FROM command_receipts WHERE principal_id=?7 AND command_scope=?8 AND idempotency_key=?9 AND request_hash=?10 AND operation_id=?2 AND response_json=?11) AND (SELECT COUNT(*) FROM command_receipts WHERE operation_id=?2)=1",params![id,r.operation_id,original.deployment_id,r.binding_id,original.session_id,expected_operation,p.principal,p.command_scope(),p.key,hash_in(&p.principal,&p.command_scope(),p.command_revision(),r.deadline_ms)?,encode(r)?,error_code],|r|r.get(0))?;
    if !exact {
        return Err(LifecycleError::CorruptStoredData);
    }
    let source_terminal = state == "completed" && p.source_state != "completed";
    let expected_source_state = if source_terminal {
        "cancelled"
    } else if retired && p.source_state == "armed" {
        "uncertain"
    } else {
        &p.source_state
    };
    let expected_source_run = if source_terminal {
        "failed"
    } else if p.source_state == "completed" {
        "succeeded"
    } else if expected_source_state == "uncertain" {
        "uncertain"
    } else {
        "running"
    };
    let expected_source_operation = if source_terminal {
        "failed"
    } else if p.source_state == "completed" {
        "succeeded"
    } else {
        "running"
    };
    let source_is = |step: &str,
                     run: &str,
                     operation: &str,
                     code: Option<&str>|
     -> Result<bool, LifecycleError> {
        Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND s.state=?2 AND r.state=?3 AND o.state=?4 AND o.error_code IS ?5)",params![original.step_id,step,run,operation,code],|r|r.get(0))?)
    };
    let mut source_exact = source_is(
        expected_source_state,
        expected_source_run,
        expected_source_operation,
        source_terminal.then_some("resolved_by_owned_cleanup"),
    )?;
    // An expired Stop's source may since have been resolved by the Stop issued
    // in its place.
    if !source_exact && state == "cancelled" && p.source_state != "completed" {
        source_exact = source_is(
            "cancelled",
            "failed",
            "failed",
            Some("resolved_by_owned_cleanup"),
        )?;
    }
    if !source_exact {
        return Err(LifecycleError::CorruptStoredData);
    }
    recorded(tx, &p, &state)?;
    Ok((p, e, state))
}

fn recorded(
    tx: &Transaction<'_>,
    p: &CleanupPlan,
    state: &str,
) -> Result<Option<String>, LifecycleError> {
    let prior: Option<(String, u64)> = tx
        .query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [&p.receipt.step_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((raw, epoch)) = prior else {
        return if state == "completed" {
            Err(LifecycleError::CorruptStoredData)
        } else {
            Ok(None)
        };
    };
    let gone: Gone = decode(&raw)?;
    nonempty_receipt(&gone.receipt)?;
    let expected = identity_dtos(&expected_identities(tx, p)?);
    let released:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND state='released') AND NOT EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?1) AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE operation_id=?2)",params![p.receipt.binding_id,p.receipt.operation_id],|r|r.get(0))?;
    if state != "completed"
        || !released
        || gone.version != 1
        || gone.step_id != p.receipt.step_id
        || gone.binding_id != p.receipt.binding_id
        || gone.incarnation != p.receipt.incarnation
        || gone.identities != expected
        || gone.observed_at_ms < p.issued_at_ms.ok_or(LifecycleError::CorruptStoredData)?
        || gone.observed_at_ms > p.receipt.deadline_ms
        || epoch <= p.source.execution.as_ref().unwrap().expected_epoch + 1
        || epoch > resource_ledger::read_snapshot(tx).map_err(resource)?.epoch
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(raw))
}

fn current_cleanup(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &CleanupPlan,
) -> Result<(), LifecycleError> {
    check_session(tx, s)?;
    let f = target(p);
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3 AND desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0) AND (SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?4)=1 AND EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND operation_id=?4 AND revision=?2 AND generation=?3)",params![f.deployment_id,f.revision,f.generation,p.receipt.operation_id],|r|r.get(0))?;
    if !exact || p.source.session_id != s.id() {
        return Err(LifecycleError::Stale);
    }
    Ok(())
}

fn retained(
    tx: &Transaction<'_>,
    p: &CleanupPlan,
    e: &EffectiveDeployment,
    state: &str,
) -> Result<(), LifecycleError> {
    let original = &p.source;
    let b: BindingDto = decode(&original.binding_json)?;
    let endpoint: std::net::SocketAddr = b
        .endpoint
        .parse()
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    let binding_state = if state == "armed" || p.source_state != "completed" {
        "uncertain"
    } else {
        "live"
    };
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND incarnation=?4 AND ownership='managed' AND binding_json=?5 AND state=?6) AND (SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?2 AND instance_index=?10 AND state!='released')=1 AND EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?1 AND host='127.0.0.1' AND port=?7) AND (SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?1)=1 AND NOT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.deployment_id=?2 AND b.instance_index=?10 AND s.id NOT IN (?8,?9) AND s.state IN ('planned','armed','uncertain') AND NOT (s.state='uncertain' AND EXISTS(SELECT 1 FROM operations o WHERE o.id=s.operation_id AND o.kind IN ('park','restore'))))",params![original.binding_id,original.deployment_id,original.revision,original.incarnation,original.binding_json,binding_state,endpoint.port(),original.step_id,p.receipt.step_id,original.instance_index],|r|r.get(0))?;
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    // SPEC §7.3 (W5): a completed launch may be parked, or held at a park
    // or restore peak that this stop took over while it was uncertain.
    let held = if p.source_state == "completed" {
        super::park::retained_footprint(e, ledger.owners.get(&original.owner()))
    } else {
        ledger.owners.get(&original.owner()) == Some(&super::startup::cold(original, e))
    };
    let source_state: String = tx.query_row(
        "SELECT state FROM lifecycle_steps WHERE id=?1",
        [&original.step_id],
        |r| r.get(0),
    )?;
    let run = crate::lifecycle::validate_initialize_run(
        tx,
        &original.fence(),
        &original.operation_id,
        &original.session_id,
        original.deadline_ms,
    )?;
    if !exact
        || source_state != p.source_state
        || run
            != if p.source_state == "completed" {
                "succeeded"
            } else if p.source_state == "uncertain" {
                "uncertain"
            } else {
                "running"
            }
        || !held
        || ledger.epoch <= original.execution.as_ref().unwrap().expected_epoch
    {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}

impl crate::Store {
    /// ADR 0018 §4: the longest span, from acceptance, an ordinary stop of this
    /// instance's runtime accepts as its deadline (the launch's frozen request
    /// deadline; a longer one is refused). `None` when the instance holds no
    /// runtime launched through the ordinary path. A read only.
    pub fn instance_stop_window_ms(
        &self,
        deployment: &str,
        instance: u32,
    ) -> Result<Option<i64>, LifecycleError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT s.step_json FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                   JOIN runtime_bindings b ON b.id=s.binding_id
                  WHERE s.deployment_id=?1 AND b.instance_index=?2 AND o.kind='initialize' AND b.state!='released'",
                params![deployment, instance],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let plan: Plan = decode(&raw)?;
        let e = decode_effective_snapshot(&plan.effective_json)
            .map_err(|_| LifecycleError::CorruptStoredData)?;
        Ok(Some(e.request_deadline_ms))
    }

    /// Observation-only exact history, checked before current worker admission.
    pub fn ordinary_stop_command_receipt(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        revision: i64,
        key: &str,
        deadline: i64,
    ) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
        super::receipt::check_request(principal, deployment, revision, key, deadline)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        command_lookup(
            &tx,
            principal,
            &scope(deployment),
            deployment,
            revision,
            key,
            deadline,
        )
    }

    /// Owner decision Q7: the exact receipt of one instance's own stop.
    #[allow(clippy::too_many_arguments)]
    pub fn instance_stop_command_receipt(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        instance: u32,
        revision: i64,
        key: &str,
        deadline: i64,
    ) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
        super::receipt::check_request(principal, deployment, revision, key, deadline)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        command_lookup(
            &tx,
            principal,
            &instance_scope(deployment, instance),
            deployment,
            revision,
            key,
            deadline,
        )
    }

    /// Stop for idleness: the deployment stays eligible for on-demand activation.
    ///
    /// SPEC §6.3 separates this from an administrative stop, and the difference is
    /// the whole point: an idle eviction that suspended automatic activation would
    /// leave a deployment permanently down because nobody happened to call it.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_ordinary_stop_command(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<OrdinaryStopReceipt, LifecycleError> {
        self.accept_stop_command(
            s, principal, deployment, revision, key, now, deadline, false,
        )
    }

    /// An operator's stop: automatic activation is suspended with it.
    ///
    /// SPEC §6.3 requires explicit stop behaviour to survive the next inference
    /// request. The intent is recorded in the acceptance transaction, so a stop that
    /// committed is never left activatable by a crash between the two writes.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_administrative_stop_command(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<OrdinaryStopReceipt, LifecycleError> {
        self.accept_stop_command(s, principal, deployment, revision, key, now, deadline, true)
    }

    /// Resolve generation in the same acceptance transaction, after receipt lookup.
    ///
    /// ADR 0013 §6: a deployment stop drains and stops every instance that holds
    /// a runtime. The first (lowest index) is answered under the deployment's
    /// command scope, exactly as a single-instance stop always was; each sibling
    /// is its own stop under its instance's scope with the same key, accepted in
    /// the same transaction, so a retry replays all of them. A sibling that
    /// cannot accept a stop now refuses the whole command, as the one instance
    /// did before, and nothing is accepted.
    #[allow(clippy::too_many_arguments)]
    fn accept_stop_command(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
        administrative: bool,
    ) -> Result<OrdinaryStopReceipt, LifecycleError> {
        super::receipt::check_request(principal, deployment, revision, key, deadline)?;
        if now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        if let Some(receipt) = command_lookup(
            &tx,
            principal,
            &scope(deployment),
            deployment,
            revision,
            key,
            deadline,
        )? {
            return Ok(receipt);
        }
        let receipt = Self::stop_in_transaction(
            &tx,
            s,
            principal,
            deployment,
            revision,
            key,
            now,
            deadline,
            administrative,
            true,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// The acceptance of a deployment Stop inside the caller's transaction,
    /// after its receipt lookup. `defer`: an operator's Stop that finds a launch
    /// still in flight without an association is recorded as deferred (live
    /// M47) instead of fencing that launch; the deferred Stop's own resolution
    /// passes `false`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stop_in_transaction(
        tx: &Transaction<'_>,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
        administrative: bool,
        defer: bool,
    ) -> Result<OrdinaryStopReceipt, LifecycleError> {
        super::receipt::command_revision(tx, deployment, revision)?;
        super::check_managed_command_target(tx, deployment)?;
        // Written with the acceptance, never after it. A stop that committed while
        // the intent did not would be undone by the next inference request.
        if administrative {
            tx.execute(
                "UPDATE deployments SET admin_stopped=1 WHERE id=?1",
                [deployment],
            )?;
        }
        // Nothing waits to be placed any more: a stop ends every pending start.
        tx.execute(
            "UPDATE deployment_instances SET pending_start_until_ms=NULL WHERE deployment_id=?1",
            [deployment],
        )?;
        let fences = super::receipt::runtime_fences(tx, deployment)?;
        // SPEC §6.1, §6.3: an operator's Stop is always accepted. A deployment
        // holding nothing (a failed launch already released with evidence, or
        // one never started) records the Stop at once: automatic activation is
        // suspended and every instance's desired state is `stopped`. Found live
        // (M16, M53): a FAILED deployment refused its Stop.
        if fences.is_empty() && administrative {
            return super::recorded_stop::accept(
                tx, principal, deployment, revision, key, now, deadline,
            );
        }
        // SPEC §6.3 (live M47): a launch in flight with no association yet has
        // no recorded processes a cleanup could prove gone, and fencing it now
        // would leave the processes it is starting untracked. The operator's
        // Stop is accepted and deferred for that launch only: automatic
        // activation is suspended at once, every sibling instance that can be
        // fenced is stopped now (ADR 0013 §6: a Ready sibling must not keep
        // serving), and the launching instance is stopped by an ordinary Stop
        // when its launch settles (Ready, released after failure, or retained
        // uncertain).
        let deferring = administrative && defer && super::recorded_stop::launching(tx, deployment)?;
        if administrative {
            // SPEC §6.3: the operator stops the whole deployment. An instance
            // holding nothing (its launch failed and was released) wants
            // nothing either; each instance holding a runtime is stopped below
            // and its fence records the same.
            tx.execute(
                "UPDATE deployment_instances SET desired_state='stopped',admission_enabled=0,dispatch_enabled=0
                  WHERE deployment_id=?1 AND NOT EXISTS(SELECT 1 FROM runtime_bindings b
                        WHERE b.deployment_id=deployment_instances.deployment_id
                          AND b.instance_index=deployment_instances.instance_index AND b.state!='released')",
                [deployment],
            )?;
        }
        let mut first = None;
        for (instance, fence) in fences {
            // An instance whose stop is already in flight is being stopped: it
            // has nothing further to accept (a deferred Stop's siblings, or an
            // instance stop the operator issued first).
            if super::recorded_stop::stopping(tx, deployment, instance)? {
                continue;
            }
            // The launching instance is the deferred Stop's own.
            if deferring && super::recorded_stop::launching_instance(tx, deployment, instance)? {
                continue;
            }
            // With a deferred Stop the deployment's command scope answers with
            // the deferred receipt, so every sibling stop has its own scope.
            let command = StopCommand {
                scope: (deferring || first.is_some()).then(|| instance_scope(deployment, instance)),
                revision: Some(revision),
            };
            let receipt = Self::accept_instance_stop_in_transaction(
                tx, s, principal, &fence, key, now, deadline, &command,
            )?;
            first.get_or_insert(receipt);
        }
        if deferring {
            return super::recorded_stop::defer(
                tx, principal, deployment, revision, key, now, deadline,
            );
        }
        // SPEC §6.3: a deployment that holds no runtime has nothing to stop.
        first.ok_or(LifecycleError::Conflict)
    }

    /// Owner decision Q7 (ADR 0013 as amended): stop one instance with verified
    /// cleanup, under that instance's own command scope. `None` when the
    /// instance holds no runtime: recording the operator's stop was the whole
    /// effect. The deployment stays eligible for on-demand activation of its
    /// other instances (SPEC §6.3), so this never sets the operator stop.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_instance_stop_command(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        instance: u32,
        revision: i64,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
        super::receipt::check_request(principal, deployment, revision, key, deadline)?;
        if now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let scope = instance_scope(deployment, instance);
        if let Some(receipt) =
            command_lookup(&tx, principal, &scope, deployment, revision, key, deadline)?
        {
            return Ok(Some(receipt));
        }
        super::receipt::command_revision(&tx, deployment, revision)?;
        super::check_managed_command_target(&tx, deployment)?;
        tx.execute(
            "UPDATE deployment_instances SET pending_start_until_ms=NULL WHERE deployment_id=?1 AND instance_index=?2",
            params![deployment, instance],
        )?;
        let Some(fence) = super::receipt::runtime_fences(&tx, deployment)?
            .into_iter()
            .find(|(index, _)| *index == instance)
            .map(|(_, fence)| fence)
        else {
            tx.commit()?;
            return Ok(None);
        };
        let command = StopCommand {
            scope: Some(scope),
            revision: Some(revision),
        };
        let receipt = Self::accept_instance_stop_in_transaction(
            &tx, s, principal, &fence, key, now, deadline, &command,
        )?;
        tx.commit()?;
        Ok(Some(receipt))
    }

    /// One instance's stop: the unarmed stop of a start that never armed, or the
    /// verified cleanup of a launch that did.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn accept_instance_stop_in_transaction(
        tx: &Transaction<'_>,
        s: &CoordinatorSession,
        principal: &str,
        fence: &DeploymentFence,
        key: &str,
        now: i64,
        deadline: i64,
        command: &StopCommand,
    ) -> Result<OrdinaryStopReceipt, LifecycleError> {
        let command = command.for_fence(fence);
        if let Some(receipt) =
            super::unarmed_stop::accept(tx, s, principal, fence, key, now, deadline, &command)?
        {
            return Ok(receipt);
        }
        Self::accept_ordinary_cleanup_in_transaction(
            tx, s, principal, fence, key, now, deadline, &command,
        )
        .map(Into::into)
    }

    /// Exact receipts replay independently of the service-resolved generation.
    /// Separate keys cannot join retained cleanup. No prior-session adoption.
    pub fn accept_ordinary_cleanup(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        f: &DeploymentFence,
        key: &str,
        now: i64,
        deadline: i64,
    ) -> Result<OrdinaryCleanupReceipt, LifecycleError> {
        if principal.trim().is_empty()
            || principal.len() > 256
            || key.trim().is_empty()
            || key.len() > 256
            || now < 0
            || deadline <= 0
            || f.revision < 1
        {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        if let Some(receipt) =
            lookup(&tx, principal, f, key, deadline).map_err(|error| match error {
                LifecycleError::IdempotencyConflict => LifecycleError::Conflict,
                error => error,
            })?
        {
            return Ok(receipt);
        }
        let receipt = Self::accept_ordinary_cleanup_in_transaction(
            &tx,
            s,
            principal,
            f,
            key,
            now,
            deadline,
            &StopCommand::default(),
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_ordinary_cleanup_in_transaction(
        tx: &Transaction<'_>,
        s: &CoordinatorSession,
        principal: &str,
        f: &DeploymentFence,
        key: &str,
        now: i64,
        deadline: i64,
        command: &StopCommand,
    ) -> Result<OrdinaryCleanupReceipt, LifecycleError> {
        let receipt_scope = command
            .scope
            .clone()
            .unwrap_or_else(|| scope(&f.deployment_id));
        let request_hash = hash_in(
            principal,
            &receipt_scope,
            command.revision.unwrap_or(f.revision),
            deadline,
        )?;
        // ADR 0013 §5: the fence names one instance; only its binding is stopped.
        let instance = crate::instances::fence_instance(tx, f).map_err(|error| match error {
            LifecycleError::Stale => LifecycleError::Conflict,
            error => error,
        })?;
        let (raw,state):(String,String)=tx.query_row("SELECT s.step_json,s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.deployment_id=?1 AND b.instance_index=?2 AND o.kind='initialize' AND b.state!='released'",params![f.deployment_id,instance],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(LifecycleError::Conflict)?;
        let original: Plan = decode(&raw)?;
        let e = source(tx, &original)?;
        super::validate_local(tx, &original, &e, &state)?;
        super::current(tx, s, &original, state == "completed")?;
        if original.fence() != *f
            || deadline <= now
            || now < original.accepted_at_ms
            || deadline
                .checked_sub(now)
                .is_none_or(|d| d > e.request_deadline_ms)
        {
            return Err(LifecycleError::Conflict);
        }
        let association = super::association(tx, &original)?;
        // SPEC §§6.1, 13.2: a launch whose effect exited without an observed
        // outcome is retained as uncertain and may never have been associated (a
        // remote Initialize whose result was lost). Stop takes it over, and must
        // then prove gone exactly the identities its binding recorded. An armed
        // launch is still its effect's to finish, so it is refused as before.
        if association.is_none() {
            if state != "uncertain" {
                return Err(LifecycleError::Conflict);
            }
            super::failed_launch::recorded_identities(tx, &original.binding_id)
                .map_err(|_| LifecycleError::Conflict)?;
        }
        // SPEC §6.3 (W5): a stop takes over the instance's park or restore
        // work; an armed one is still its effect's and refuses the stop.
        super::park::yield_to_stop(tx, s, &f.deployment_id, instance)?;
        let next = Self::fence_lifecycle_in_transaction(tx, s, f, false)?;
        let operation = ulid::Ulid::new().to_string();
        crate::lifecycle::insert_owned_cleanup_run(
            tx,
            s,
            &next,
            &operation,
            deadline,
            "ordinary_cleanup",
        )?;
        if state == "completed" {
            tx.execute("INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation,instance_index) VALUES(?1,?2,?3,?4,?5)",params![f.deployment_id,operation,next.revision,next.generation,instance])?;
        } else {
            Self::handoff_claims_in_transaction(
                tx,
                s,
                &operation,
                &original.operation_id,
                std::slice::from_ref(&next),
            )?;
        }
        let receipt = OrdinaryCleanupReceipt {
            operation_id: operation,
            step_id: ulid::Ulid::new().to_string(),
            binding_id: original.binding_id.clone(),
            incarnation: original.incarnation.clone(),
            revision: next.revision,
            generation: next.generation,
            accepted_at_ms: now,
            deadline_ms: deadline,
        };
        let p = CleanupPlan {
            version: 1,
            kind: CleanupKind::OrdinaryCleanup,
            principal: principal.into(),
            key: key.into(),
            receipt: receipt.clone(),
            source: original,
            source_state: state,
            association,
            issued_at_ms: None,
            scope: command.scope.clone(),
            command_revision: command.revision,
            reissue_of: None,
        };
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![receipt.step_id,receipt.operation_id,f.deployment_id,receipt.binding_id,s.id(),encode(&p)?])?;
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,receipt_scope,key,request_hash,receipt.operation_id,encode(&receipt)?])?;
        // ADR 0013 §6: the stopping instance closes its own admission.
        tx.execute(
            "UPDATE deployment_instances SET admission_enabled=0 WHERE deployment_id=?1 AND instance_index=?2",
            params![f.deployment_id, instance],
        )?;
        read(tx, &receipt.step_id)?;
        event(tx, s, &p, OrdinaryCleanupTransition::Accepted, None)?;
        Ok(receipt)
    }

    /// Only New carries a context. Caller must await the old effect task before
    /// arming, and check a fresh clock immediately before sending the control.
    pub fn arm_ordinary_cleanup_with_context(
        &self,
        s: &CoordinatorSession,
        id: &str,
        now: i64,
    ) -> Result<(ArmResult, Option<CleanupExecutionContext>), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (mut p, e, state) = read(&tx, id)?;
        if state == "completed" {
            return Ok((ArmResult::AlreadyRecorded, None));
        }
        current_cleanup(&tx, s, &p)?;
        retained(&tx, &p, &e, &state)?;
        if state == "armed" {
            return Ok((ArmResult::AlreadyRecorded, None));
        }
        if state != "planned" || now < p.receipt.accepted_at_ms || now >= p.receipt.deadline_ms {
            return Err(LifecycleError::Conflict);
        }
        p.issued_at_ms = Some(now);
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='armed',step_json=?2 WHERE id=?1 AND state='planned'",
            params![id, encode(&p)?],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1 AND state='queued'",
            [&p.receipt.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE operations SET state='running' WHERE id=?1 AND state='pending'",
            [&p.receipt.operation_id],
        )?)?;
        one(tx.execute("UPDATE runtime_bindings SET state='uncertain' WHERE id=?1 AND state IN ('live','uncertain')",[&p.receipt.binding_id])?)?;
        let context = context(&tx, &p)?;
        event(&tx, s, &p, OrdinaryCleanupTransition::Armed, None)?;
        tx.commit()?;
        Ok((ArmResult::New { step_id: id.into() }, Some(context)))
    }
}

fn lookup(
    tx: &Transaction<'_>,
    principal: &str,
    f: &DeploymentFence,
    key: &str,
    deadline: i64,
) -> Result<Option<OrdinaryCleanupReceipt>, LifecycleError> {
    let receipt = command_lookup(
        tx,
        principal,
        &scope(&f.deployment_id),
        &f.deployment_id,
        f.revision,
        key,
        deadline,
    )?;
    receipt
        .map(|r| {
            let (p, _, _) = read(tx, &r.step_id).map_err(super::receipt::historical_error)?;
            Ok(p.receipt)
        })
        .transpose()
}

pub(super) fn command_lookup(
    tx: &Transaction<'_>,
    principal: &str,
    command_scope: &str,
    deployment: &str,
    revision: i64,
    key: &str,
    deadline: i64,
) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
    let request_hash = hash_in(principal, command_scope, revision, deadline)?;
    let prior = {
        let mut statement = tx.prepare("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3")?;
        let mut rows = statement.query(params![principal, command_scope, key])?;
        rows.next()?
            .map(|row| {
                Ok::<_, LifecycleError>((
                    super::receipt::bounded_text(row, 0, 64)?,
                    super::receipt::bounded_text(row, 1, 26)?,
                    super::receipt::bounded_text(row, 2, 1 << 20)?,
                ))
            })
            .transpose()?
    };
    let Some((old, operation, raw)) = prior else {
        return Ok(None);
    };
    if old != request_hash {
        return Err(LifecycleError::IdempotencyConflict);
    }
    let kind = {
        let mut statement = tx.prepare("SELECT kind FROM operations WHERE id=?1")?;
        let mut rows = statement.query([&operation])?;
        let row = rows.next()?.ok_or(LifecycleError::CorruptStoredData)?;
        super::receipt::bounded_text(row, 0, 64)?
    };
    match kind.as_str() {
        "ordinary_unarmed_stop" => {
            return super::unarmed_stop::lookup(
                tx, principal, deployment, revision, key, deadline, &operation, &raw,
            )
            .map(Some)
        }
        // SPEC §6.3: a Stop recorded against a deployment holding nothing.
        // SPEC §6.3 (live M47): a Stop deferred behind an in-flight launch.
        super::recorded_stop::KIND | super::recorded_stop::DEFERRED_KIND => {
            return super::recorded_stop::lookup(tx, deployment, deadline, &operation, &raw)
                .map(Some)
        }
        "ordinary_cleanup" => {}
        _ => return Err(LifecycleError::CorruptStoredData),
    }
    let receipt: OrdinaryCleanupReceipt = decode(&raw)?;
    let (p, _, _) = read(tx, &receipt.step_id).map_err(super::receipt::historical_error)?;
    if p.receipt != receipt
        || receipt.operation_id != operation
        || receipt.deadline_ms != deadline
        || p.source.deployment_id != deployment
        || p.command_revision() != revision
        || p.principal != principal
        || p.key != key
        || p.command_scope() != command_scope
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(receipt.into()))
}

impl crate::Store {
    /// ADR 0011: the only cleanup an mllm store completes is an ordinary one.
    pub fn complete_cleanup(
        &self,
        s: &CoordinatorSession,
        id: &str,
        e: &CleanupEvidence,
        now: i64,
        ttl: i64,
    ) -> Result<(), LifecycleError> {
        self.complete_cleanup_with_clock(s, id, e, ttl, || Ok(now))
    }

    /// The clock is read only once the stored kind is known, so a caller that must
    /// consult a slow source pays for it on the path that will use the reading.
    pub fn complete_cleanup_with_clock(
        &self,
        s: &CoordinatorSession,
        id: &str,
        e: &CleanupEvidence,
        ttl: i64,
        mut clock: impl FnMut() -> Result<i64, LifecycleError>,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let kind: Option<String> = tx
            .query_row(
                "SELECT o.kind FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if kind.as_deref() != Some("ordinary_cleanup") {
            return Err(LifecycleError::Unsupported);
        }
        let now = clock()?;
        complete(&tx, s, id, e, now, ttl)?;
        tx.commit()?;
        Ok(())
    }
}

pub(crate) fn complete(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    evidence: &CleanupEvidence,
    now: i64,
    ttl: i64,
) -> Result<(), LifecycleError> {
    check_session(tx, s)?;
    let supplied = evidence_value(id, evidence)?;
    let (p, e, state) = read(tx, id)?;
    if let Some(old) = recorded(tx, &p, &state)? {
        return if old == supplied {
            Ok(())
        } else {
            Err(LifecycleError::Conflict)
        };
    }
    current_cleanup(tx, s, &p)?;
    retained(tx, &p, &e, &state)?;
    let expected = expected_identities(tx, &p)?;
    if state != "armed"
        || evidence.binding_id != p.receipt.binding_id
        || evidence.incarnation != p.receipt.incarnation
        || super::failed_launch::canonical_members_or_empty(&evidence.identities)? != expected
        || vacuous_release(tx, &p, &expected)?
    {
        return Err(LifecycleError::Conflict);
    }
    if ttl != e.host.observation_ttl_ms {
        return Err(LifecycleError::Invalid);
    }
    fresh(
        p.issued_at_ms.ok_or(LifecycleError::Conflict)?,
        p.receipt.deadline_ms,
        evidence.observed_at_ms,
        now,
        ttl,
    )?;
    let original = &p.source;
    // SPEC §10 controlled cleanup: gone evidence for the whole recorded group
    // proves no request on this fence can still run, including those a crashed
    // session left behind and a restart adopted (W12). A lease on any other
    // fence is not proven by this group's absence.
    // ADR 0013 §5: only this instance's leases; a sibling's are its own.
    let unproven:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?4 AND NOT (revision=?2 AND generation=?3))",params![original.deployment_id,original.revision,original.generation,original.instance_index],|r|r.get(0))?;
    if unproven {
        return Err(LifecycleError::Conflict);
    }
    tx.execute(
        "DELETE FROM request_leases WHERE deployment_id=?1 AND revision=?2 AND generation=?3",
        params![
            original.deployment_id,
            original.revision,
            original.generation
        ],
    )?;
    if p.source_state != "completed" {
        one(tx.execute("UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state IN ('armed','uncertain')",[&original.step_id])?)?;
        one(tx.execute("UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state IN ('running','uncertain')",[&original.operation_id])?)?;
        one(tx.execute("UPDATE operations SET state='failed',error_code='resolved_by_owned_cleanup' WHERE id=?1 AND state='running'",[&original.operation_id])?)?;
    }
    one(tx.execute(
        "DELETE FROM resource_owners WHERE owner_id=?1",
        [&original.owner()],
    )?)?;
    one(tx.execute(
        "DELETE FROM endpoint_leases WHERE binding_id=?1",
        [&p.receipt.binding_id],
    )?)?;
    // Spec §3: the encrypted engine key does not outlive the binding it was issued
    // for. A binding that never had a key stored against it (a legacy or
    // non-vLLM launch) has no row, so this is not `one`.
    tx.execute(
        "DELETE FROM engine_secrets WHERE binding_id=?1",
        [&p.receipt.binding_id],
    )?;
    one(tx.execute(
        "UPDATE runtime_bindings SET state='released' WHERE id=?1 AND state='uncertain'",
        [&p.receipt.binding_id],
    )?)?;
    // W5: a park or restore this stop took over while uncertain is settled by
    // the same gone evidence.
    super::park::settle_after_cleanup(tx, &p.receipt.binding_id)?;
    one(tx.execute("UPDATE deployment_instances SET observed_state='stopped' WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0",params![original.deployment_id,p.receipt.revision,p.receipt.generation])?)?;
    // W10 gap (a): the stopped instance holds nothing, so no closure reason of
    // it can matter; a restart that keeps this generation starts with none.
    crate::switch_state::clear_instance_closures(
        tx,
        &original.deployment_id,
        original.instance_index,
    )?;
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
        [id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='running'",
        [&p.receipt.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='running'",
        [&p.receipt.operation_id],
    )?)?;
    one(tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.receipt.operation_id],
    )?)?;
    let epoch = resource_ledger::advance_completion_epoch(tx)?;
    tx.execute(
        "INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch) VALUES(?1,?2,?3)",
        params![id, supplied, epoch],
    )?;
    event(tx, s, &p, OrdinaryCleanupTransition::Completed, Some(epoch))
}
