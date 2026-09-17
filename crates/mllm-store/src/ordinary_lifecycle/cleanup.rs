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
        identities: identity_dtos(&canonical_members(&e.identities)?),
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
    association: Association,
    issued_at_ms: Option<i64>,
}

pub(super) fn scope(deployment: &str) -> String {
    format!("POST:/management/v1/deployments/{deployment}/actions")
}
pub(super) fn hash(
    principal: &str,
    target: &DeploymentFence,
    deadline: i64,
) -> Result<String, LifecycleError> {
    // Generation is a service-resolved acceptance fence, not a request-body field.
    Ok(format!(
        "{:x}",
        Sha256::digest(
            encode(&(
                1,
                principal,
                scope(&target.deployment_id),
                target.revision,
                "stop",
                deadline
            ))?
            .as_bytes()
        )
    ))
}
fn target(p: &CleanupPlan) -> DeploymentFence {
    DeploymentFence {
        deployment_id: p.source.deployment_id.clone(),
        revision: p.receipt.revision,
        generation: p.receipt.generation,
    }
}
fn context(p: &CleanupPlan) -> Result<CleanupExecutionContext, LifecycleError> {
    Ok(CleanupExecutionContext {
        operation_id: p.receipt.operation_id.clone(),
        step_id: p.receipt.step_id.clone(),
        binding_id: p.receipt.binding_id.clone(),
        incarnation: p.receipt.incarnation.clone(),
        fence: target(p),
        identities: members(&p.association.identities)?,
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
    crate::managed_configuration::validate_revision_history(
        tx,
        &p.deployment_id,
        p.revision,
        &p.effective_json,
    )
    .map_err(|_| LifecycleError::CorruptStoredData)?;
    let exact: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND s.operation_id=?2 AND s.deployment_id=?3 AND s.binding_id=?4 AND s.session_id=?5 AND s.ordinal=0 AND s.step_json=?6 AND o.kind='initialize' AND o.deployment_id=?3) AND EXISTS(SELECT 1 FROM effective_revisions WHERE deployment_id=?3 AND revision=?7 AND effective_json=?8 AND fingerprint=?9) AND EXISTS(SELECT 1 FROM operations WHERE deployment_id=?3 AND kind='managed_configuration_create' AND state='succeeded') AND (SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?2)=1", params![p.step_id,p.operation_id,p.deployment_id,p.binding_id,p.session_id,encode(p)?,p.revision,p.effective_json,e.recipe_fingerprint], |r|r.get(0))?;
    // Re-derive the identity this binding must carry rather than matching the
    // qualified spelling of it. A restart-only deployment is identified by its
    // recipe and host, and cleanup is engine-agnostic anyway: it proves the
    // recorded processes are gone, which is the same proof whatever started them.
    let identity = super::binding_identity(&e)?;
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
    let footprint = resource_ledger::encode(&phase(&e.resources.cold, ResourcePhase::Cold))
        .map_err(resource)?;
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
    let binding:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND incarnation=?4 AND ownership='managed' AND binding_json=?5)",params![p.binding_id,p.deployment_id,p.revision,p.incarnation,p.binding_json],|r|r.get(0))?;
    if !binding {
        return Err(LifecycleError::CorruptStoredData);
    }
    let association = super::association(tx, p)?.ok_or(LifecycleError::Conflict)?;
    if association.observed_at_ms < execution.issued_at_ms
        || association.observed_at_ms > p.deadline_ms
    {
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
        || r.generation
            != original
                .generation
                .checked_add(1)
                .ok_or(LifecycleError::CorruptStoredData)?
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
    let association = super::association(tx, original)?.ok_or(LifecycleError::Conflict)?;
    if association != p.association
        || association.observed_at_ms < original.execution.as_ref().unwrap().issued_at_ms
        || association.observed_at_ms > original.deadline_ms
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let predecessor = (p.source_state != "completed").then_some(original.operation_id.as_str());
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
        _ => return Err(LifecycleError::CorruptStoredData),
    };
    if p.issued_at_ms
        .is_some_and(|issued| issued < r.accepted_at_ms || issued >= r.deadline_ms)
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND operation_id=?2 AND deployment_id=?3 AND binding_id=?4 AND session_id=?5 AND ordinal=0 AND grant_id IS NULL) AND EXISTS(SELECT 1 FROM operations WHERE id=?2 AND deployment_id=?3 AND kind='ordinary_cleanup' AND state=?6 AND error_code IS NULL) AND (SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?2)=1 AND EXISTS(SELECT 1 FROM command_receipts WHERE principal_id=?7 AND command_scope=?8 AND idempotency_key=?9 AND request_hash=?10 AND operation_id=?2 AND response_json=?11) AND (SELECT COUNT(*) FROM command_receipts WHERE operation_id=?2)=1",params![id,r.operation_id,original.deployment_id,r.binding_id,original.session_id,expected_operation,p.principal,scope(&original.deployment_id),p.key,hash(&p.principal,&original.fence(),r.deadline_ms)?,encode(r)?],|r|r.get(0))?;
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
    let source_exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND s.state=?2 AND r.state=?3 AND o.state=?4 AND o.error_code IS ?5)",params![original.step_id,expected_source_state,expected_source_run,expected_source_operation,if source_terminal {Some("resolved_by_owned_cleanup")} else {None}],|r|r.get(0))?;
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
    let released:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND state='released') AND NOT EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?1) AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE operation_id=?2)",params![p.receipt.binding_id,p.receipt.operation_id],|r|r.get(0))?;
    if state != "completed"
        || !released
        || gone.version != 1
        || gone.step_id != p.receipt.step_id
        || gone.binding_id != p.receipt.binding_id
        || gone.incarnation != p.receipt.incarnation
        || gone.identities != p.association.identities
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
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3 AND desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0) AND (SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?4)=1 AND EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND operation_id=?4 AND revision=?2 AND generation=?3)",params![f.deployment_id,f.revision,f.generation,p.receipt.operation_id],|r|r.get(0))?;
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
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND incarnation=?4 AND ownership='managed' AND binding_json=?5 AND state=?6) AND (SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?2 AND state!='released')=1 AND EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?1 AND host='127.0.0.1' AND port=?7) AND (SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?1)=1 AND NOT EXISTS(SELECT 1 FROM lifecycle_steps WHERE deployment_id=?2 AND id NOT IN (?8,?9) AND state IN ('planned','armed','uncertain'))",params![original.binding_id,original.deployment_id,original.revision,original.incarnation,original.binding_json,binding_state,endpoint.port(),original.step_id,p.receipt.step_id],|r|r.get(0))?;
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    let expected = if p.source_state == "completed" {
        phase(&e.resources.ready, ResourcePhase::Ready)
    } else {
        phase(&e.resources.cold, ResourcePhase::Cold)
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
        || ledger.owners.get(&original.deployment_id) != Some(&expected)
        || ledger.epoch <= original.execution.as_ref().unwrap().expected_epoch
    {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}

impl crate::Store {
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
            &DeploymentFence {
                deployment_id: deployment.into(),
                revision,
                generation: 0,
            },
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
        self.accept_stop_command(s, principal, deployment, revision, key, now, deadline, false)
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
            &DeploymentFence {
                deployment_id: deployment.into(),
                revision,
                generation: 0,
            },
            key,
            deadline,
        )? {
            return Ok(receipt);
        }
        let fence = super::receipt::command_fence(&tx, deployment, revision)?;
        super::check_managed_command_target(&tx, deployment)?;
        // Written with the acceptance, never after it. A stop that committed while
        // the intent did not would be undone by the next inference request.
        if administrative {
            tx.execute(
                "UPDATE deployments SET admin_stopped=1 WHERE id=?1",
                [deployment],
            )?;
        }
        if let Some(receipt) =
            super::unarmed_stop::accept(&tx, s, principal, &fence, key, now, deadline)?
        {
            tx.commit()?;
            return Ok(receipt);
        }
        let receipt = Self::accept_ordinary_cleanup_in_transaction(
            &tx, s, principal, &fence, key, now, deadline,
        )?;
        tx.commit()?;
        Ok(receipt.into())
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
        let receipt =
            Self::accept_ordinary_cleanup_in_transaction(&tx, s, principal, f, key, now, deadline)?;
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
    ) -> Result<OrdinaryCleanupReceipt, LifecycleError> {
        let request_hash = hash(principal, f, deadline)?;
        let (raw,state):(String,String)=tx.query_row("SELECT s.step_json,s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.deployment_id=?1 AND o.kind='initialize' AND b.state!='released'",[&f.deployment_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(LifecycleError::Conflict)?;
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
        let association = super::association(tx, &original)?.ok_or(LifecycleError::Conflict)?;
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
            tx.execute("INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation) VALUES(?1,?2,?3,?4)",params![f.deployment_id,operation,next.revision,next.generation])?;
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
        };
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![receipt.step_id,receipt.operation_id,f.deployment_id,receipt.binding_id,s.id(),encode(&p)?])?;
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,scope(&f.deployment_id),key,request_hash,receipt.operation_id,encode(&receipt)?])?;
        tx.execute(
            "UPDATE deployments SET admission_enabled=0 WHERE id=?1",
            [&f.deployment_id],
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
        let context = context(&p)?;
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
    let receipt = command_lookup(tx, principal, f, key, deadline)?;
    receipt
        .map(|r| {
            let (p, _, _) = read(tx, &r.step_id).map_err(super::receipt::historical_error)?;
            Ok(p.receipt)
        })
        .transpose()
}

fn command_lookup(
    tx: &Transaction<'_>,
    principal: &str,
    f: &DeploymentFence,
    key: &str,
    deadline: i64,
) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
    let request_hash = hash(principal, f, deadline)?;
    let prior = {
        let mut statement = tx.prepare("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3")?;
        let mut rows = statement.query(params![principal, scope(&f.deployment_id), key])?;
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
            return super::unarmed_stop::lookup(tx, principal, f, key, deadline, &operation, &raw)
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
        || p.source.deployment_id != f.deployment_id
        || p.source.revision != f.revision
        || p.principal != principal
        || p.key != key
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
    if state != "armed"
        || evidence.binding_id != p.receipt.binding_id
        || evidence.incarnation != p.receipt.incarnation
        || canonical_members(&evidence.identities)? != members(&p.association.identities)?
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
    let unproven:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND NOT (revision=?2 AND generation=?3 AND session_id=?4))",params![original.deployment_id,original.revision,original.generation,original.session_id],|r|r.get(0))?;
    if unproven {
        return Err(LifecycleError::Conflict);
    }
    tx.execute("DELETE FROM request_leases WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND session_id=?4",params![original.deployment_id,original.revision,original.generation,original.session_id])?;
    if p.source_state != "completed" {
        one(tx.execute("UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state IN ('armed','uncertain')",[&original.step_id])?)?;
        one(tx.execute("UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state IN ('running','uncertain')",[&original.operation_id])?)?;
        one(tx.execute("UPDATE operations SET state='failed',error_code='resolved_by_owned_cleanup' WHERE id=?1 AND state='running'",[&original.operation_id])?)?;
    }
    one(tx.execute(
        "DELETE FROM resource_owners WHERE owner_id=?1",
        [&original.deployment_id],
    )?)?;
    one(tx.execute(
        "DELETE FROM endpoint_leases WHERE binding_id=?1",
        [&p.receipt.binding_id],
    )?)?;
    one(tx.execute(
        "UPDATE runtime_bindings SET state='released' WHERE id=?1 AND state='uncertain'",
        [&p.receipt.binding_id],
    )?)?;
    one(tx.execute("UPDATE deployments SET observed_state='stopped' WHERE id=?1 AND revision=?2 AND current_generation=?3 AND desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0",params![original.deployment_id,p.receipt.revision,p.receipt.generation])?)?;
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
