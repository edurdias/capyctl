//! Physical completion retains conservative accounting and closed admission.
use super::*;
use mllm_domain::completion::{
    CompletionEvidence, Milestone, OwnedLaunchReceipt, TransitionToken,
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
/// Spec §4: a launch's canonical membership is one `api` identity plus one or
/// more workers named contiguously from `worker-0`, all sharing a boot id with
/// distinct pids. A tensor-parallel launch needs several workers, and vLLM's
/// EngineCore already numbers them this way.
pub(crate) fn canonical_members(
    ids: &[ProcessIdentity],
) -> Result<Vec<ProcessIdentity>, LifecycleError> {
    if ids.len() < 2
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
    let boot_id = ids[0].boot_id.clone();
    let mut pids = std::collections::BTreeSet::new();
    let mut roles = std::collections::BTreeSet::new();
    if ids.iter().any(|i| {
        i.boot_id != boot_id || !pids.insert(i.pid) || !roles.insert(i.role.clone())
    }) {
        return Err(LifecycleError::Invalid);
    }
    // Every role is distinct (checked above), so removing `api` and then every
    // expected `worker-i` in turn only succeeds, with nothing left over, when
    // the role set is exactly {api, worker-0, ..., worker-(len-2)}.
    if !roles.remove("api") {
        return Err(LifecycleError::Invalid);
    }
    for worker in 0..ids.len() - 1 {
        if !roles.remove(&format!("worker-{worker}")) {
            return Err(LifecycleError::Invalid);
        }
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
/// The stored form read back as identities, with no shape required of the set.
/// Callers that need a canonical launch membership use `members`; a caller that
/// must also accept the sets a launch holds before it is complete checks its own
/// shape on top of this.
pub(crate) fn identities(dtos: &[IdentityDto]) -> Vec<ProcessIdentity> {
    dtos.iter()
        .map(|i| ProcessIdentity {
            role: i.role.clone(),
            pid: i.pid,
            boot_id: i.boot_id.clone(),
            start_ticks: i.start_ticks,
        })
        .collect()
}
/// Spec §4: the stored-association sibling of `canonical_members`, accepting
/// the same shape (`api` plus workers numbered contiguously from `worker-0`).
pub(crate) fn members(dtos: &[IdentityDto]) -> Result<Vec<ProcessIdentity>, LifecycleError> {
    canonical_members(&identities(dtos)).map_err(|_| LifecycleError::CorruptStoredData)
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenDto {
    deployment_id: String,
    revision: i64,
    generation: i64,
    operation_id: String,
    step_id: String,
}
impl From<&TransitionToken> for TokenDto {
    fn from(t: &TransitionToken) -> Self {
        Self {
            deployment_id: t.deployment_id.clone(),
            revision: t.revision,
            generation: t.generation,
            operation_id: t.operation_id.clone(),
            step_id: t.step_id.clone(),
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
        Err(LifecycleError::Unsupported)
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
        completion_value(e)?;
        if crate::ordinary_lifecycle::is_ordinary(&tx, id)? {
            crate::ordinary_lifecycle::complete(&tx, s, id, e, now, ttl)?;
            tx.commit()?;
            return Ok(());
        }
        Err(LifecycleError::Unsupported)
    }
}
