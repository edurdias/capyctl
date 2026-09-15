//! Candidate physical completion retains conservative accounting and closed admission.
use super::*;
use crate::candidate_creation::initialize::{
    validate_current_initialize, validate_retained_initialize, validated_initialize,
    ValidatedInitialize,
};
use crate::events::{
    append_event, CandidateLifecycleTransition, EventMetadata, EventOperationId, EventWriteError,
};
use mllm_domain::completion::{
    verify_completion, CompletionEvidence, CompletionExpectation, Milestone, OwnedLaunchReceipt,
    TransitionToken,
};

pub(crate) fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, LifecycleError> {
    if text.len() > MAX_DTO_BYTES {
        return Err(LifecycleError::CorruptStoredData);
    }
    serde_json::from_str(text).map_err(|_| LifecycleError::CorruptStoredData)
}
pub(crate) fn encode(value: &impl Serialize) -> Result<String, LifecycleError> {
    bounded_json(value)
}
pub(crate) fn check_session(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
) -> Result<(), LifecycleError> {
    crate::dispatch::check_session(tx, s).map_err(|e| match e {
        crate::dispatch::DispatchError::Sql(e) => LifecycleError::Sql(e),
        _ => LifecycleError::Stale,
    })
}
pub(crate) fn canonical_members(
    ids: &[ProcessIdentity],
) -> Result<Vec<ProcessIdentity>, LifecycleError> {
    if ids.len() != 2
        || ids.iter().any(|i| {
            i.role.len() > MAX_DTO_BYTES / 4
                || i.boot_id.len() > MAX_DTO_BYTES / 4
                || i.pid == 0
                || i.start_ticks == 0
                || i.boot_id.trim().is_empty()
        })
    {
        return Err(LifecycleError::Invalid);
    }
    let mut ids = ids.to_vec();
    ids.sort();
    if ids[0].role != "api"
        || ids[1].role != "worker-0"
        || ids[0].pid == ids[1].pid
        || ids[0].boot_id != ids[1].boot_id
    {
        return Err(LifecycleError::Invalid);
    }
    Ok(ids)
}
pub(crate) fn identity_dtos(ids: &[ProcessIdentity]) -> Vec<IdentityDto> {
    ids.iter()
        .map(|i| IdentityDto {
            role: i.role.clone(),
            pid: i.pid,
            boot_id: i.boot_id.clone(),
            start_ticks: i.start_ticks,
        })
        .collect()
}
pub(crate) fn members(dtos: &[IdentityDto]) -> Result<Vec<ProcessIdentity>, LifecycleError> {
    if dtos.len() != 2 {
        return Err(LifecycleError::CorruptStoredData);
    }
    canonical_members(
        &dtos
            .iter()
            .map(|i| ProcessIdentity {
                role: i.role.clone(),
                pid: i.pid,
                boot_id: i.boot_id.clone(),
                start_ticks: i.start_ticks,
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|_| LifecycleError::CorruptStoredData)
}
pub(crate) fn nonempty_receipt(receipt: &str) -> Result<(), LifecycleError> {
    if receipt.trim().is_empty() || receipt.len() > MAX_DTO_BYTES / 2 {
        Err(LifecycleError::Invalid)
    } else {
        Ok(())
    }
}
pub(crate) fn fresh(
    issued: i64,
    deadline: i64,
    observed: i64,
    now: i64,
    ttl: i64,
) -> Result<(), LifecycleError> {
    if issued < 0
        || ttl <= 0
        || observed < issued
        || now < observed
        || now > deadline
        || observed.checked_add(ttl).is_none_or(|end| now > end)
    {
        return Err(LifecycleError::Rejected("evidence freshness".into()));
    }
    Ok(())
}
pub(crate) fn policy_ttl(tx: &Transaction<'_>, host: &str) -> Result<i64, LifecycleError> {
    crate::resource_policy::read_singleton_policy(tx, host)
        .map_err(|_| LifecycleError::CorruptStoredData)?
        .map(|p| p.controls.observation_ttl_ms)
        .filter(|ttl| *ttl > 0)
        .ok_or(LifecycleError::CorruptStoredData)
}
pub(crate) fn isolated(tx: &Transaction<'_>, deployment: &str) -> Result<(), LifecycleError> {
    let closed:bool=tx.query_row("SELECT admission_enabled=0 AND dispatch_enabled=0 AND desired_state='stopped' AND NOT EXISTS(SELECT 1 FROM deployment_routes WHERE deployment_id=?1) FROM deployments WHERE id=?1",[deployment],|r|r.get(0))?;
    if !closed {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}
pub(crate) fn event(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    operation: &str,
    deployment: &str,
    step: &str,
    transition: CandidateLifecycleTransition,
    epoch: Option<u64>,
) -> Result<(), LifecycleError> {
    let id = |s: &str| {
        s.parse()
            .map(EventOperationId::generated)
            .map_err(|_| LifecycleError::CorruptStoredData)
    };
    append_event(
        tx,
        &EventMetadata::CandidateLifecycleRecorded {
            transition,
            operation_id: id(operation)?,
            deployment_id: id(deployment)?,
            step_id: id(step)?,
            session_epoch: s.epoch(),
            committed_epoch: epoch,
        },
    )
    .map_err(|e| match e {
        EventWriteError::Sql(e) => LifecycleError::Sql(e),
        _ => LifecycleError::CorruptStoredData,
    })?;
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnedLaunchAssociationV1 {
    version: u8,
    pub step_id: String,
    session_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
}
fn association_value(
    v: &ValidatedInitialize,
    r: &OwnedLaunchReceipt,
) -> Result<OwnedLaunchAssociationV1, LifecycleError> {
    nonempty_receipt(&r.receipt)?;
    if r.binding_id != v.context.binding_id || r.incarnation != v.context.incarnation {
        return Err(LifecycleError::Conflict);
    }
    let ids = canonical_members(&r.identities)?;
    let value = OwnedLaunchAssociationV1 {
        version: 1,
        step_id: v.context.token.step_id.clone(),
        session_id: v.session_id.clone(),
        binding_id: r.binding_id.clone(),
        incarnation: r.incarnation.clone(),
        identities: identity_dtos(&ids),
        observed_at_ms: r.observed_at_ms,
        receipt: r.receipt.clone(),
    };
    encode(&value)?;
    Ok(value)
}
/// Does not load accounting or cleanup, keeping history validation acyclic.
pub(crate) fn association(
    tx: &Transaction<'_>,
    v: &ValidatedInitialize,
) -> Result<Option<OwnedLaunchAssociationV1>, LifecycleError> {
    let row:Option<(String,String,String)>=tx.query_row("SELECT binding_id,incarnation,association_json FROM owned_launch_associations WHERE step_id=?1",[&v.context.token.step_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((binding, incarnation, json)) = row else {
        return Ok(None);
    };
    let mut a: OwnedLaunchAssociationV1 = decode(&json)?;
    let ids = members(&a.identities)?;
    a.identities = identity_dtos(&ids);
    if a.version != 1
        || a.step_id != v.context.token.step_id
        || a.session_id != v.session_id
        || a.binding_id != v.context.binding_id
        || binding != a.binding_id
        || a.incarnation != v.context.incarnation
        || incarnation != a.incarnation
        || a.observed_at_ms < v.context.issued_at_ms
        || a.observed_at_ms > v.context.deadline_ms
        || nonempty_receipt(&a.receipt).is_err()
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let raw: String = tx.query_row(
        "SELECT identities_json FROM runtime_bindings WHERE id=?1",
        [&binding],
        |r| r.get(0),
    )?;
    let stored: Vec<IdentityDto> = decode(&raw)?;
    if members(&stored)? != ids {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(a))
}
pub(crate) fn accounting(
    tx: &Transaction<'_>,
    v: &ValidatedInitialize,
) -> Result<(), LifecycleError> {
    let state: String = tx.query_row(
        "SELECT state FROM runtime_bindings WHERE id=?1",
        [&v.context.binding_id],
        |r| r.get(0),
    )?;
    if state == "released" {
        crate::candidate_creation::cleanup::validate_gone_history(tx, v)
    } else if crate::candidate_creation::progression::is_v3(tx, &v.context.token.step_id)? {
        crate::candidate_creation::progression::validated_anchor(tx, &v.context.token.step_id)
            .map(|_| ())
    } else {
        validate_retained_initialize(tx, &v.context.token.step_id)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenDto {
    deployment_id: String,
    revision: i64,
    generation: i64,
    operation_id: String,
    step_id: String,
    qualification_id: String,
}
impl From<&TransitionToken> for TokenDto {
    fn from(t: &TransitionToken) -> Self {
        Self {
            deployment_id: t.deployment_id.clone(),
            revision: t.revision,
            generation: t.generation,
            operation_id: t.operation_id.clone(),
            step_id: t.step_id.clone(),
            qualification_id: t.qualification_id.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MilestoneDto {
    Quiesced,
    MemoryReleased,
    AllocationsRestored,
    WeightsUsable,
    CacheValid,
    ModelUsable,
}
impl From<Milestone> for MilestoneDto {
    fn from(m: Milestone) -> Self {
        match m {
            Milestone::Quiesced => Self::Quiesced,
            Milestone::MemoryReleased => Self::MemoryReleased,
            Milestone::AllocationsRestored => Self::AllocationsRestored,
            Milestone::WeightsUsable => Self::WeightsUsable,
            Milestone::CacheValid => Self::CacheValid,
            Milestone::ModelUsable => Self::ModelUsable,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadyKind {
    ReadyCompletion,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionEvidenceV1 {
    version: u8,
    kind: ReadyKind,
    token: TokenDto,
    identities: Vec<IdentityDto>,
    observed_at_ms: i64,
    control_receipt: Option<String>,
    milestones: Vec<MilestoneDto>,
}
pub(crate) fn completion_value(e: &CompletionEvidence) -> Result<CompletionEvidenceV1, LifecycleError> {
    nonempty_receipt(
        e.control_receipt
            .as_deref()
            .ok_or(LifecycleError::Invalid)?,
    )?;
    if e.milestones.len() > 6
        || [
            &e.token.deployment_id,
            &e.token.operation_id,
            &e.token.step_id,
            &e.token.qualification_id,
        ]
        .iter()
        .any(|s| s.len() > 4096)
    {
        return Err(LifecycleError::Invalid);
    }
    let value = CompletionEvidenceV1 {
        version: 1,
        kind: ReadyKind::ReadyCompletion,
        token: (&e.token).into(),
        identities: identity_dtos(&canonical_members(&e.identities)?),
        observed_at_ms: e.observed_at_ms,
        control_receipt: e.control_receipt.clone(),
        milestones: e.milestones.iter().copied().map(Into::into).collect(),
    };
    encode(&value)?;
    Ok(value)
}
fn validate_recorded_ready(
    tx: &Transaction<'_>,
    v: &ValidatedInitialize,
    a: &OwnedLaunchAssociationV1,
    raw: &str,
    epoch: u64,
) -> Result<CompletionEvidenceV1, LifecycleError> {
    let mut e: CompletionEvidenceV1 = decode(raw)?;
    e.identities = identity_dtos(&members(&e.identities)?);
    let ledger =
        crate::resource_ledger::read_snapshot(tx).map_err(|_| LifecycleError::CorruptStoredData)?;
    let grant_epoch: u64 = tx.query_row(
        "SELECT committed_epoch FROM resource_grants WHERE id=?1",
        [v.context
            .grant_id
            .as_ref()
            .ok_or(LifecycleError::CorruptStoredData)?],
        |r| r.get(0),
    )?;
    let op: bool = tx.query_row(
        "SELECT state='succeeded' FROM operations WHERE id=?1",
        [&v.context.token.operation_id],
        |r| r.get(0),
    )?;
    if e.version != 1
        || e.token != TokenDto::from(&v.context.token)
        || e.identities != a.identities
        || e.observed_at_ms < v.context.issued_at_ms
        || e.observed_at_ms > v.context.deadline_ms
        || nonempty_receipt(e.control_receipt.as_deref().unwrap_or("")).is_err()
        || e.milestones
            != vec![
                MilestoneDto::AllocationsRestored,
                MilestoneDto::WeightsUsable,
                MilestoneDto::CacheValid,
                MilestoneDto::ModelUsable,
            ]
        || v.state != "completed"
        || v.run_state != "succeeded"
        || !op
        || epoch <= grant_epoch
        || epoch > ledger.epoch
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(e)
}
impl crate::Store {
    /// Trusted collector seam; management clients cannot certify launch membership.
    pub fn record_owned_launch(
        &self,
        s: &CoordinatorSession,
        id: &str,
        r: &OwnedLaunchReceipt,
        now: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        if crate::ordinary_lifecycle::is_ordinary(&tx, id)? {
            crate::ordinary_lifecycle::record_launch(&tx, s, id, r, now)?;
            tx.commit()?;
            return Ok(());
        }
        let v3 = crate::candidate_creation::progression::is_v3(&tx, id)?;
        let v = if v3 {
            crate::candidate_creation::progression::validated_anchor(&tx, id)?
        } else {
            validated_initialize(&tx, id)?
        };
        let supplied = association_value(&v, r)?;
        accounting(&tx, &v)?;
        if let Some(old) = association(&tx, &v)? {
            return if old == supplied {
                Ok(())
            } else {
                Err(LifecycleError::Conflict)
            };
        }
        if v3 {
            crate::candidate_creation::progression::current_anchor(&tx, s, id)?;
        } else {
            validate_current_initialize(&tx, s, id)?;
        }
        isolated(&tx, &v.context.token.deployment_id)?;
        fresh(
            v.context.issued_at_ms,
            v.context.deadline_ms,
            r.observed_at_ms,
            now,
            policy_ttl(&tx, v.snapshot.receipt().host_id())?,
        )?;
        let raw: String = tx.query_row(
            "SELECT identities_json FROM runtime_bindings WHERE id=?1",
            [&r.binding_id],
            |r| r.get(0),
        )?;
        let old: Vec<IdentityDto> = decode(&raw)?;
        if !old.is_empty() && (old.len() != 1 || old[0] != supplied.identities[0]) {
            return Err(LifecycleError::Conflict);
        }
        tx.execute("INSERT INTO owned_launch_associations(step_id,binding_id,incarnation,association_json) VALUES(?1,?2,?3,?4)",params![id,r.binding_id,r.incarnation,encode(&supplied)?])?;
        tx.execute(
            "UPDATE runtime_bindings SET identities_json=?1 WHERE id=?2",
            params![encode(&supplied.identities)?, r.binding_id],
        )?;
        event(
            &tx,
            s,
            &v.context.token.operation_id,
            &v.context.token.deployment_id,
            id,
            CandidateLifecycleTransition::OwnedLaunchAssociated,
            None,
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn complete_step(
        &self,
        s: &CoordinatorSession,
        id: &str,
        e: &CompletionEvidence,
        now: i64,
        ttl: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let supplied = completion_value(e)?;
        if crate::ordinary_lifecycle::is_ordinary(&tx, id)? {
            crate::ordinary_lifecycle::complete(&tx, s, id, e, now, ttl)?;
            tx.commit()?;
            return Ok(());
        }
        let v = validated_initialize(&tx, id)?;
        accounting(&tx, &v)?;
        let a = association(&tx, &v)?.ok_or(LifecycleError::Conflict)?;
        let old: Option<(String, u64)> = tx
            .query_row(
                "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((raw, epoch)) = old {
            return if validate_recorded_ready(&tx, &v, &a, &raw, epoch)? == supplied {
                Ok(())
            } else {
                Err(LifecycleError::Conflict)
            };
        }
        if v.state == "completed" {
            return Err(LifecycleError::CorruptStoredData);
        }
        validate_current_initialize(&tx, s, id)?;
        isolated(&tx, &v.context.token.deployment_id)?;
        let outstanding:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1) OR EXISTS(SELECT 1 FROM lifecycle_steps WHERE operation_id=?2 AND id!=?3 AND state IN ('armed','uncertain'))",params![v.context.token.deployment_id,v.context.token.operation_id,id],|r|r.get(0))?;
        if outstanding {
            return Err(LifecycleError::Conflict);
        }
        let persisted = policy_ttl(&tx, v.snapshot.receipt().host_id())?;
        if ttl != persisted {
            return Err(LifecycleError::Invalid);
        }
        fresh(
            v.context.issued_at_ms,
            v.context.deadline_ms,
            e.observed_at_ms,
            now,
            persisted,
        )?;
        verify_completion(
            &CompletionExpectation {
                token: v.context.token.clone(),
                identities: members(&a.identities)?,
                target: v
                    .context
                    .completion_target
                    .clone()
                    .ok_or(LifecycleError::CorruptStoredData)?,
                issued_at_ms: v.context.issued_at_ms,
                deadline_ms: v.context.deadline_ms,
            },
            e,
            now,
            persisted,
        )
        .map_err(|e| LifecycleError::Rejected(e.to_string()))?;
        let epoch = crate::resource_ledger::advance_completion_epoch(&tx)?;
        tx.execute(
            "UPDATE runtime_bindings SET state='live' WHERE id=?1",
            [&v.context.binding_id],
        )?;
        tx.execute(
            "UPDATE deployments SET observed_state='ready' WHERE id=?1",
            [&v.context.token.deployment_id],
        )?;
        tx.execute(
            "UPDATE lifecycle_steps SET state='completed' WHERE id=?1",
            [id],
        )?;
        tx.execute(
            "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1",
            [&v.context.token.operation_id],
        )?;
        tx.execute(
            "UPDATE operations SET state='succeeded' WHERE id=?1",
            [&v.context.token.operation_id],
        )?;
        tx.execute(
            "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
            params![id, encode(&supplied)?, epoch],
        )?;
        tx.execute(
            "DELETE FROM lifecycle_claims WHERE operation_id=?1",
            [&v.context.token.operation_id],
        )?;
        event(
            &tx,
            s,
            &v.context.token.operation_id,
            &v.context.token.deployment_id,
            id,
            CandidateLifecycleTransition::ReadyCompleted,
            Some(epoch),
        )?;
        tx.commit()?;
        Ok(())
    }
}
