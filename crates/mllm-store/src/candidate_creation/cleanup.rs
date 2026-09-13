use super::initialize::ArmResult;
use super::initialize::{validated_initialize, ValidatedInitialize};
use crate::dispatch::CoordinatorSession;
use crate::events::CandidateLifecycleTransition;
use crate::lifecycle::completion::{
    accounting, association, canonical_members, check_session, decode, encode, event, fresh,
    identity_dtos, isolated, members, nonempty_receipt, policy_ttl,
};
use crate::lifecycle::{insert_candidate_cleanup_run, validate_cleanup_run, IdentityDto};
use crate::lifecycle::{DeploymentFence, LifecycleError};
use mllm_domain::completion::{CleanupEvidence, ProcessIdentity};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupMode {
    TerminateOwned,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateCleanupReceipt {
    operation_id: String,
    step_id: String,
    run_id: String,
    binding_id: String,
    incarnation: String,
    revision: i64,
    generation: i64,
    accepted_at_ms: i64,
    deadline_ms: i64,
}
impl CandidateCleanupReceipt {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn step_id(&self) -> &str {
        &self.step_id
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
    pub fn revision(&self) -> i64 {
        self.revision
    }
    pub fn generation(&self) -> i64 {
        self.generation
    }
    pub fn accepted_at_ms(&self) -> i64 {
        self.accepted_at_ms
    }
    pub fn deadline_ms(&self) -> i64 {
        self.deadline_ms
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupCommand {
    expected_revision: i64,
    action: CleanupAction,
    deadline_ms: i64,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CleanupAction {
    Cleanup,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCleanupReceiptV1 {
    version: u8,
    method: String,
    target: String,
    principal_id: String,
    request_hash: String,
    run_id: String,
    operation_id: String,
    step_id: String,
    binding_id: String,
    incarnation: String,
    revision: i64,
    generation: i64,
    accepted_at_ms: i64,
    deadline_ms: i64,
}
impl CandidateCleanupReceiptV1 {
    fn public(&self) -> CandidateCleanupReceipt {
        CandidateCleanupReceipt {
            operation_id: self.operation_id.clone(),
            step_id: self.step_id.clone(),
            run_id: self.run_id.clone(),
            binding_id: self.binding_id.clone(),
            incarnation: self.incarnation.clone(),
            revision: self.revision,
            generation: self.generation,
            accepted_at_ms: self.accepted_at_ms,
            deadline_ms: self.deadline_ms,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CleanupKind {
    CandidateCleanup,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCleanupStepV1 {
    version: u8,
    kind: CleanupKind,
    principal_id: String,
    run_id: String,
    operation_id: String,
    step_id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    session_id: String,
    binding_id: String,
    incarnation: String,
    association_step_id: String,
    authorization_operation_id: String,
    manifest_digest: String,
    recipe_fingerprint: String,
    accepted_at_ms: i64,
    cleanup_origin_ms: i64,
    max_cleanup_duration_ms: i64,
    deadline_ms: i64,
    predecessor_operation_id: Option<String>,
    predecessor_cleanup_operation_id: Option<String>,
    mode: CleanupMode,
    identities: Vec<IdentityDto>,
}
impl CandidateCleanupStepV1 {
    fn fence(&self) -> DeploymentFence {
        DeploymentFence {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            generation: self.generation,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCleanupArmedV2 {
    version: u8,
    planned: CandidateCleanupStepV1,
    issued_at_ms: i64,
}
struct ReadCleanup {
    planned: CandidateCleanupStepV1,
    issued: Option<i64>,
    state: String,
    run_state: String,
    receipt: CandidateCleanupReceiptV1,
    initialize: ValidatedInitialize,
    // Populated only after the complete bounded predecessor chain validates.
    predecessor_operations: Vec<String>,
}

fn recovery_mode(previous: &CleanupMode, issued: Option<i64>) -> CleanupMode {
    if *previous == CleanupMode::InspectOwnedGone || issued.is_some() {
        CleanupMode::InspectOwnedGone
    } else {
        CleanupMode::TerminateOwned
    }
}
fn target(run: &str) -> String {
    format!("/management/v1/qualification-runs/{run}/actions")
}
fn hash(
    principal: &str,
    run: &str,
    revision: i64,
    deadline: i64,
) -> Result<String, LifecycleError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            encode(&(
                1,
                "POST",
                target(run),
                principal,
                run,
                revision,
                "cleanup",
                deadline
            ))?
            .as_bytes()
        )
    ))
}
fn corrupt<T>(result: Result<T, super::CandidateCreationError>) -> Result<T, LifecycleError> {
    result.map_err(|e| match e {
        super::CandidateCreationError::Sql(e) => LifecycleError::Sql(e),
        _ => LifecycleError::CorruptStoredData,
    })
}

/// Immutable one-record reader. It never follows cleanup/accounting links recursively.
fn read_one(tx: &Transaction<'_>, id: &str) -> Result<ReadCleanup, LifecycleError> {
    type Row = (
        String,
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        String,
        String,
        Option<String>,
    );
    let row:Option<Row>=tx.query_row("SELECT s.operation_id,s.deployment_id,s.binding_id,s.session_id,s.state,s.ordinal,s.grant_id,s.step_json,a.run_id,a.predecessor_cleanup_operation_id FROM lifecycle_steps s JOIN candidate_cleanup_actions a ON a.step_id=s.id AND a.operation_id=s.operation_id WHERE s.id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?))).optional()?;
    let Some((
        operation,
        deployment,
        binding,
        session,
        state,
        ordinal,
        grant,
        json,
        run,
        predecessor,
    )) = row
    else {
        let cleanup:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='candidate_cleanup')",[id],|r|r.get(0))?;
        return Err(if cleanup {
            LifecycleError::CorruptStoredData
        } else {
            LifecycleError::Unsupported
        });
    };
    let (p, issued) = match decode::<CandidateCleanupStepV1>(&json) {
        Ok(p) => (p, None),
        Err(_) => {
            let a: CandidateCleanupArmedV2 = decode(&json)?;
            if a.version != 2 {
                return Err(LifecycleError::CorruptStoredData);
            }
            (a.planned, Some(a.issued_at_ms))
        }
    };
    let v = validated_initialize(tx, &p.association_step_id)?;
    let a = association(tx, &v)?.ok_or(LifecycleError::CorruptStoredData)?;
    let c = v.snapshot.receipt();
    let identities = members(&p.identities)?;
    if p.version != 1
        || operation != p.operation_id
        || deployment != p.deployment_id
        || binding != p.binding_id
        || session != p.session_id
        || ordinal != 0
        || grant.is_some()
        || run != p.run_id
        || predecessor != p.predecessor_cleanup_operation_id
        || id != p.step_id
        || !super::ulid(id)
        || !super::ulid(&operation)
        || !super::ulid(&session)
        || p.run_id != c.run_id()
        || p.deployment_id != c.deployment_id()
        || p.revision != c.revision()
        || p.generation <= c.generation()
        || p.binding_id != c.binding_id()
        || p.incarnation != c.incarnation()
        || p.authorization_operation_id != c.operation_id()
        || p.manifest_digest != c.manifest_digest()
        || p.recipe_fingerprint != c.recipe_fingerprint()
        || !c.allow_owned_abort_cleanup()
        || p.cleanup_origin_ms < c.accepted_at_ms()
        || p.accepted_at_ms < p.cleanup_origin_ms
        || p.deadline_ms <= p.accepted_at_ms
        || p.max_cleanup_duration_ms
            != v.snapshot
                .reviewed_manifest()
                .limits()
                .max_cleanup_duration_ms()
        || p.cleanup_origin_ms
            .checked_add(p.max_cleanup_duration_ms)
            .is_none_or(|max| p.deadline_ms > max)
        || identities != members(&a.identities)?
        || issued.is_some_and(|t| t < p.accepted_at_ms || t >= p.deadline_ms)
        || (!matches!(state.as_str(), "planned" | "cancelled") && issued.is_none())
        || (state == "planned" && issued.is_some())
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let owner: String = tx.query_row(
        "SELECT principal_id FROM qualification_runs WHERE id=?1",
        [&run],
        |r| r.get(0),
    )?;
    if owner != p.principal_id {
        return Err(LifecycleError::CorruptStoredData);
    }
    let records:Vec<(String,String,String,String,String)>=tx.prepare("SELECT principal_id,command_scope,idempotency_key,request_hash,response_json FROM command_receipts WHERE operation_id=?1")?.query_map([&operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<Result<_,_>>()?;
    if records.len() != 1 {
        return Err(LifecycleError::CorruptStoredData);
    }
    let (principal, scope, key, column_hash, raw) = &records[0];
    let receipt: CandidateCleanupReceiptV1 = decode(raw)?;
    if principal != &p.principal_id
        || scope != &format!("POST {}", target(&run))
        || !super::valid_id(key)
        || receipt.version != 1
        || receipt.method != "POST"
        || receipt.target != target(&run)
        || receipt.principal_id != *principal
        || receipt.request_hash != *column_hash
        || receipt.request_hash != hash(principal, &run, p.revision, p.deadline_ms)?
        || receipt.run_id != run
        || receipt.operation_id != operation
        || receipt.step_id != id
        || receipt.binding_id != binding
        || receipt.incarnation != p.incarnation
        || receipt.revision != p.revision
        || receipt.generation != p.generation
        || receipt.accepted_at_ms != p.accepted_at_ms
        || receipt.deadline_ms != p.deadline_ms
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind='candidate_cleanup' AND idempotency_key IS NULL) AND EXISTS(SELECT 1 FROM generation_history WHERE deployment_id=?2 AND generation=?3) AND (SELECT count(*) FROM lifecycle_steps WHERE operation_id=?1)=1",params![operation,deployment,p.generation],|r|r.get(0))?;
    if !valid {
        return Err(LifecycleError::CorruptStoredData);
    }
    let run_state = validate_cleanup_run(
        tx,
        &p.fence(),
        &operation,
        &session,
        p.deadline_ms,
        p.predecessor_operation_id.as_deref(),
    )?;
    Ok(ReadCleanup {
        planned: p,
        issued,
        state,
        run_state,
        receipt,
        initialize: v,
        predecessor_operations: Vec::new(),
    })
}
fn read(tx: &Transaction<'_>, id: &str) -> Result<ReadCleanup, LifecycleError> {
    let mut result = read_one(tx, id)?;
    let origin = &result.planned;
    let mut prior = origin.predecessor_cleanup_operation_id.clone();
    let mut child = origin.clone();
    let mut visited = std::collections::BTreeSet::from([origin.operation_id.clone()]);
    while let Some(operation) = prior {
        if visited.len() >= 64 || !visited.insert(operation.clone()) {
            return Err(LifecycleError::CorruptStoredData);
        }
        let step: String = tx
            .query_row(
                "SELECT step_id FROM candidate_cleanup_actions WHERE operation_id=?1",
                [&operation],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LifecycleError::CorruptStoredData)?;
        let previous = read_one(tx, &step)?;
        let p = previous.planned;
        if child.predecessor_operation_id.as_deref() != Some(&operation)
            || p.run_id != origin.run_id
            || p.binding_id != origin.binding_id
            || p.incarnation != origin.incarnation
            || p.association_step_id != origin.association_step_id
            || p.cleanup_origin_ms != origin.cleanup_origin_ms
            || p.deadline_ms != origin.deadline_ms
            || p.max_cleanup_duration_ms != origin.max_cleanup_duration_ms
            || p.session_id == child.session_id
            || p.generation >= child.generation
            || p.accepted_at_ms > child.accepted_at_ms
            || child.mode != recovery_mode(&p.mode, previous.issued)
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        prior = p.predecessor_cleanup_operation_id.clone();
        result.predecessor_operations.push(operation);
        child = p;
    }
    if child.mode != CleanupMode::TerminateOwned
        || child.accepted_at_ms != child.cleanup_origin_ms
        || child
            .predecessor_operation_id
            .as_ref()
            .is_some_and(|p| p != &result.initialize.context.token.operation_id)
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    if let Some(initialize) = child.predecessor_operation_id {
        result.predecessor_operations.push(initialize);
    }
    Ok(result)
}
fn current(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    r: &ReadCleanup,
) -> Result<(), LifecycleError> {
    let p = &r.planned;
    if p.session_id != s.id() {
        return Err(LifecycleError::Stale);
    }
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3) AND (SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?4)=1 AND EXISTS(SELECT 1 FROM lifecycle_claims WHERE operation_id=?4 AND deployment_id=?1 AND revision=?2 AND generation=?3) AND NOT EXISTS(SELECT 1 FROM candidate_cleanup_actions WHERE predecessor_cleanup_operation_id=?4)",params![p.deployment_id,p.revision,p.generation,p.operation_id],|r|r.get(0))?;
    if !valid {
        return Err(LifecycleError::Stale);
    }
    isolated(tx, &p.deployment_id)?;
    super::initialize::validate_retained_initialize(tx, &p.association_step_id)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GoneKind {
    OwnedCleanup,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupEvidenceV1 {
    version: u8,
    kind: GoneKind,
    step_id: String,
    binding_id: String,
    incarnation: String,
    identities: Vec<IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
}
fn evidence_value(id: &str, e: &CleanupEvidence) -> Result<CleanupEvidenceV1, LifecycleError> {
    nonempty_receipt(&e.receipt)?;
    if !super::ulid(id) || !super::ulid(&e.binding_id) || !super::ulid(&e.incarnation) {
        return Err(LifecycleError::Invalid);
    }
    let v = CleanupEvidenceV1 {
        version: 1,
        kind: GoneKind::OwnedCleanup,
        step_id: id.into(),
        binding_id: e.binding_id.clone(),
        incarnation: e.incarnation.clone(),
        identities: identity_dtos(&canonical_members(&e.identities)?),
        observed_at_ms: e.observed_at_ms,
        receipt: e.receipt.clone(),
    };
    encode(&v)?;
    Ok(v)
}
fn recorded(
    tx: &Transaction<'_>,
    r: &ReadCleanup,
) -> Result<Option<CleanupEvidenceV1>, LifecycleError> {
    let p = &r.planned;
    let row: Option<(String, u64)> = tx
        .query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [&p.step_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((raw, epoch)) = row else {
        if r.state == "completed" {
            return Err(LifecycleError::CorruptStoredData);
        }
        return Ok(None);
    };
    let mut e: CleanupEvidenceV1 = decode(&raw)?;
    e.identities = identity_dtos(&members(&e.identities)?);
    let ledger =
        crate::resource_ledger::read_snapshot(tx).map_err(|_| LifecycleError::CorruptStoredData)?;
    let grant: u64 = tx.query_row(
        "SELECT committed_epoch FROM resource_grants WHERE id=?1",
        [r.initialize
            .context
            .grant_id
            .as_ref()
            .ok_or(LifecycleError::CorruptStoredData)?],
        |r| r.get(0),
    )?;
    let closed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_runs WHERE id=?1 AND cleanup_state='verified_gone' AND cleanup_step_id=?2) AND EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?3 AND incarnation=?4 AND state='released') AND NOT EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?3) AND EXISTS(SELECT 1 FROM operations WHERE id=?5 AND state='succeeded')",params![p.run_id,p.step_id,p.binding_id,p.incarnation,p.operation_id],|r|r.get(0))?;
    if e.version != 1
        || e.step_id != p.step_id
        || e.binding_id != p.binding_id
        || e.incarnation != p.incarnation
        || members(&e.identities)? != members(&p.identities)?
        || e.observed_at_ms < r.issued.ok_or(LifecycleError::CorruptStoredData)?
        || e.observed_at_ms > p.deadline_ms
        || nonempty_receipt(&e.receipt).is_err()
        || r.state != "completed"
        || r.run_state != "succeeded"
        || !closed
        || ledger.owners.contains_key(&p.deployment_id)
        || epoch <= grant
        || epoch > ledger.epoch
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(e))
}
pub(crate) fn validate_gone_history(
    tx: &Transaction<'_>,
    v: &ValidatedInitialize,
) -> Result<(), LifecycleError> {
    let id:Option<String>=tx.query_row("SELECT cleanup_step_id FROM qualification_runs WHERE id=?1 AND cleanup_state='verified_gone'",[v.snapshot.receipt().run_id()],|r|r.get(0)).optional()?.flatten();
    let r = read(tx, &id.ok_or(LifecycleError::CorruptStoredData)?)?;
    if r.planned.association_step_id != v.context.token.step_id || recorded(tx, &r)?.is_none() {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(())
}

impl crate::Store {
    /// Creates separately bounded cleanup intent from frozen original ownership permission.
    pub fn accept_candidate_cleanup(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        command: &str,
        now: i64,
    ) -> Result<CandidateCleanupReceipt, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        if !super::valid_id(principal)
            || !super::ulid(run)
            || !super::valid_id(key)
            || command.len() > super::MAX_BYTES
        {
            return Err(LifecycleError::Invalid);
        }
        let command: CleanupCommand =
            serde_json::from_str(command).map_err(|_| LifecycleError::Invalid)?;
        let CleanupAction::Cleanup = command.action;
        let hash = hash(
            principal,
            run,
            command.expected_revision,
            command.deadline_ms,
        )?;
        let scope = format!("POST {}", target(run));
        let old:Option<(String,String)>=tx.query_row("SELECT request_hash,operation_id FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,scope,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((old_hash, op)) = old {
            if old_hash != hash {
                return Err(LifecycleError::Conflict);
            }
            let id: String = tx
                .query_row(
                    "SELECT step_id FROM candidate_cleanup_actions WHERE operation_id=?1",
                    [op],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or(LifecycleError::CorruptStoredData)?;
            let r = read(&tx, &id)?;
            accounting(&tx, &r.initialize)?;
            return Ok(r.receipt.public());
        }
        let snapshot =
            corrupt(super::read_snapshot(&tx, principal, run))?.ok_or(LifecycleError::Conflict)?;
        let c = snapshot.receipt();
        if !c.allow_owned_abort_cleanup()
            || command.expected_revision != c.revision()
            || snapshot.cleanup_state() != super::CandidateCleanupState::Retained
        {
            return Err(LifecycleError::Conflict);
        }
        let id:String=tx.query_row("SELECT step_id FROM owned_launch_associations WHERE binding_id=?1 AND incarnation=?2",params![c.binding_id(),c.incarnation()],|r|r.get(0)).optional()?.ok_or(LifecycleError::Conflict)?;
        let v = validated_initialize(&tx, &id)?;
        accounting(&tx, &v)?;
        let a = association(&tx, &v)?.ok_or(LifecycleError::Conflict)?;
        isolated(&tx, c.deployment_id())?;
        let mut origin = now;
        let mut mode = CleanupMode::TerminateOwned;
        let mut prior_cleanup = None;
        let last:Vec<String>=tx.prepare("SELECT a.step_id FROM candidate_cleanup_actions a WHERE a.run_id=?1 AND NOT EXISTS(SELECT 1 FROM candidate_cleanup_actions b WHERE b.predecessor_cleanup_operation_id=a.operation_id)")?.query_map([run],|r|r.get(0))?.collect::<Result<_,_>>()?;
        if last.len() > 1 {
            return Err(LifecycleError::CorruptStoredData);
        }
        if let Some(id) = last.first() {
            let previous = read(&tx, id)?;
            let p = &previous.planned;
            if p.session_id == s.id()
                || !matches!(previous.state.as_str(), "planned" | "armed" | "uncertain")
                || command.deadline_ms != p.deadline_ms
            {
                return Err(LifecycleError::Conflict);
            }
            origin = p.cleanup_origin_ms;
            mode = recovery_mode(&p.mode, previous.issued);
            prior_cleanup = Some(p.operation_id.clone());
        }
        let max = snapshot
            .reviewed_manifest()
            .limits()
            .max_cleanup_duration_ms();
        if origin < c.accepted_at_ms()
            || now < origin
            || now >= command.deadline_ms
            || origin
                .checked_add(max)
                .is_none_or(|end| command.deadline_ms > end)
        {
            return Err(LifecycleError::Invalid);
        }
        let fence: DeploymentFence = tx.query_row(
            "SELECT id,revision,current_generation FROM deployments WHERE id=?1",
            [c.deployment_id()],
            |r| {
                Ok(DeploymentFence {
                    deployment_id: r.get(0)?,
                    revision: r.get(1)?,
                    generation: r.get(2)?,
                })
            },
        )?;
        if fence.revision != c.revision() {
            return Err(LifecycleError::Stale);
        }
        let predecessor: Option<String> = tx
            .query_row(
                "SELECT operation_id FROM lifecycle_claims WHERE deployment_id=?1",
                [c.deployment_id()],
                |r| r.get(0),
            )
            .optional()?;
        if prior_cleanup
            .as_ref()
            .is_some_and(|p| predecessor.as_ref() != Some(p))
            || predecessor.as_ref().is_some_and(|p| {
                Some(p) != prior_cleanup.as_ref() && p != &v.context.token.operation_id
            })
        {
            return Err(LifecycleError::Conflict);
        }
        if predecessor.is_none() && v.state != "completed" {
            return Err(LifecycleError::Conflict);
        }
        let next = Self::fence_lifecycle_in_transaction(&tx, s, &fence, false)?;
        let operation = ulid::Ulid::new().to_string();
        let step = ulid::Ulid::new().to_string();
        insert_candidate_cleanup_run(&tx, s, &next, &operation, command.deadline_ms)?;
        if let Some(previous) = &predecessor {
            Self::handoff_claims_in_transaction(
                &tx,
                s,
                &operation,
                previous,
                std::slice::from_ref(&next),
            )?;
            tx.execute("UPDATE lifecycle_steps SET state=CASE WHEN state='planned' THEN 'cancelled' ELSE 'uncertain' END WHERE operation_id=?1 AND state IN ('planned','armed','uncertain')",[previous])?;
            tx.execute(
                "UPDATE lifecycle_runs SET state='uncertain' WHERE operation_id=?1",
                [previous],
            )?;
        } else {
            tx.execute(
                "INSERT INTO lifecycle_claims VALUES(?1,?2,?3,?4)",
                params![
                    next.deployment_id,
                    operation,
                    next.revision,
                    next.generation
                ],
            )?;
        }
        let p = CandidateCleanupStepV1 {
            version: 1,
            kind: CleanupKind::CandidateCleanup,
            principal_id: principal.into(),
            run_id: run.into(),
            operation_id: operation.clone(),
            step_id: step.clone(),
            deployment_id: next.deployment_id.clone(),
            revision: next.revision,
            generation: next.generation,
            session_id: s.id().into(),
            binding_id: c.binding_id().into(),
            incarnation: c.incarnation().into(),
            association_step_id: id,
            authorization_operation_id: c.operation_id().into(),
            manifest_digest: c.manifest_digest().into(),
            recipe_fingerprint: c.recipe_fingerprint().into(),
            accepted_at_ms: now,
            cleanup_origin_ms: origin,
            max_cleanup_duration_ms: max,
            deadline_ms: command.deadline_ms,
            predecessor_operation_id: predecessor,
            predecessor_cleanup_operation_id: prior_cleanup.clone(),
            mode,
            identities: a.identities,
        };
        let receipt = CandidateCleanupReceiptV1 {
            version: 1,
            method: "POST".into(),
            target: target(run),
            principal_id: principal.into(),
            request_hash: hash.clone(),
            run_id: run.into(),
            operation_id: operation.clone(),
            step_id: step.clone(),
            binding_id: p.binding_id.clone(),
            incarnation: p.incarnation.clone(),
            revision: p.revision,
            generation: p.generation,
            accepted_at_ms: now,
            deadline_ms: p.deadline_ms,
        };
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![step,operation,p.deployment_id,p.binding_id,s.id(),encode(&p)?])?;
        tx.execute(
            "INSERT INTO candidate_cleanup_actions VALUES(?1,?2,?3,?4)",
            params![operation, run, step, prior_cleanup],
        )?;
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,scope,key,hash,operation,encode(&receipt)?])?;
        tx.execute("UPDATE qualification_runs SET state='aborted' WHERE id=?1 AND state IN ('accepted','running')",[run])?;
        tx.execute(
            "UPDATE deployments SET admission_enabled=0,dispatch_enabled=0 WHERE id=?1",
            [c.deployment_id()],
        )?;
        event(
            &tx,
            s,
            &operation,
            c.deployment_id(),
            &step,
            CandidateLifecycleTransition::CleanupAccepted,
            None,
        )?;
        read(&tx, &step)?;
        tx.commit()?;
        Ok(receipt.public())
    }
    pub fn arm_candidate_cleanup(
        &self,
        s: &CoordinatorSession,
        id: &str,
        now: i64,
    ) -> Result<ArmResult, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let r = read(&tx, id)?;
        accounting(&tx, &r.initialize)?;
        if r.issued.is_some() && matches!(r.state.as_str(), "armed" | "uncertain" | "completed") {
            if r.state == "completed" {
                recorded(&tx, &r)?;
            }
            return Ok(ArmResult::AlreadyRecorded);
        }
        current(&tx, s, &r)?;
        let p = r.planned;
        let pending: bool = tx.query_row(
            "SELECT state='pending' FROM operations WHERE id=?1",
            [&p.operation_id],
            |r| r.get(0),
        )?;
        if r.state != "planned"
            || r.run_state != "queued"
            || !pending
            || now < p.accepted_at_ms
            || now >= p.deadline_ms
        {
            return Err(LifecycleError::Conflict);
        }
        tx.execute(
            "UPDATE lifecycle_steps SET state='armed',step_json=?1 WHERE id=?2",
            params![
                encode(&CandidateCleanupArmedV2 {
                    version: 2,
                    planned: p.clone(),
                    issued_at_ms: now
                })?,
                id
            ],
        )?;
        tx.execute(
            "UPDATE operations SET state='running' WHERE id=?1",
            [&p.operation_id],
        )?;
        tx.execute(
            "UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1",
            [&p.operation_id],
        )?;
        tx.execute(
            "UPDATE runtime_bindings SET state='uncertain' WHERE id=?1",
            [&p.binding_id],
        )?;
        event(
            &tx,
            s,
            &p.operation_id,
            &p.deployment_id,
            id,
            CandidateLifecycleTransition::CleanupArmed,
            None,
        )?;
        tx.commit()?;
        Ok(ArmResult::New { step_id: id.into() })
    }
    /// Reading/cloning this context grants no send authority. Await predecessor exit
    /// and hold the lifetime controller lock before acting on a newly armed context.
    pub fn candidate_cleanup_execution(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<CleanupExecutionContext, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let r = read(&tx, id)?;
        current(&tx, s, &r)?;
        if r.state != "armed" || r.run_state != "running" {
            return Err(LifecycleError::Conflict);
        }
        let p = r.planned;
        Ok(CleanupExecutionContext {
            fence: p.fence(),
            operation_id: p.operation_id,
            step_id: p.step_id,
            binding_id: p.binding_id,
            incarnation: p.incarnation,
            identities: members(&p.identities)?,
            issued_at_ms: r.issued.ok_or(LifecycleError::CorruptStoredData)?,
            deadline_ms: p.deadline_ms,
            mode: p.mode,
        })
    }
    pub fn complete_cleanup(
        &self,
        s: &CoordinatorSession,
        id: &str,
        e: &CleanupEvidence,
        now: i64,
        ttl: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let supplied = evidence_value(id, e)?;
        let r = read(&tx, id)?;
        if let Some(old) = recorded(&tx, &r)? {
            return if old == supplied {
                Ok(())
            } else {
                Err(LifecycleError::Conflict)
            };
        }
        current(&tx, s, &r)?;
        let p = &r.planned;
        let running: bool = tx.query_row(
            "SELECT state='running' FROM operations WHERE id=?1",
            [&p.operation_id],
            |r| r.get(0),
        )?;
        if r.state != "armed"
            || r.run_state != "running"
            || !running
            || supplied.binding_id != p.binding_id
            || supplied.incarnation != p.incarnation
            || members(&supplied.identities)? != members(&p.identities)?
        {
            return Err(LifecycleError::Conflict);
        }
        let persisted = policy_ttl(&tx, r.initialize.snapshot.receipt().host_id())?;
        if ttl != persisted {
            return Err(LifecycleError::Invalid);
        }
        fresh(
            r.issued.ok_or(LifecycleError::CorruptStoredData)?,
            p.deadline_ms,
            e.observed_at_ms,
            now,
            persisted,
        )?;
        // The immutable candidate lane has one binding, making deployment leases
        // across generations/sessions an exact incarnation scope.
        crate::dispatch::settle_verified_candidate_cleanup(&tx, &p.deployment_id, &p.binding_id)?;
        // The entry reader already validated the whole chain in this transaction.
        // Reuse its exact scope instead of rereading every remaining suffix.
        for previous in &r.predecessor_operations {
            tx.execute("UPDATE lifecycle_steps SET state='cancelled' WHERE operation_id=?1 AND state IN ('planned','armed','uncertain')",[previous])?;
            tx.execute("UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state IN ('queued','running','uncertain')",[previous])?;
            tx.execute("UPDATE operations SET state='failed',error_code='resolved_by_owned_cleanup' WHERE id=?1 AND state IN ('pending','running')",[previous])?;
        }
        let other:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE deployment_id=?1 AND id!=?2 AND state IN ('planned','armed','uncertain'))",params![p.deployment_id,id],|r|r.get(0))?;
        if other {
            return Err(LifecycleError::Conflict);
        }
        crate::resource_ledger::release_verified_candidate_owner(&tx, &p.deployment_id)?;
        tx.execute(
            "DELETE FROM endpoint_leases WHERE binding_id=?1",
            [&p.binding_id],
        )?;
        tx.execute(
            "UPDATE runtime_bindings SET state='released' WHERE id=?1",
            [&p.binding_id],
        )?;
        tx.execute("UPDATE deployments SET observed_state='stopped',desired_state='stopped',admission_enabled=0,dispatch_enabled=0 WHERE id=?1",[&p.deployment_id])?;
        tx.execute("UPDATE qualification_runs SET cleanup_state='verified_gone',cleanup_step_id=?1 WHERE id=?2",params![id,p.run_id])?;
        tx.execute(
            "UPDATE lifecycle_steps SET state='completed' WHERE id=?1",
            [id],
        )?;
        tx.execute(
            "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1",
            [&p.operation_id],
        )?;
        tx.execute(
            "UPDATE operations SET state='succeeded' WHERE id=?1",
            [&p.operation_id],
        )?;
        let epoch = crate::resource_ledger::advance_completion_epoch(&tx)?;
        tx.execute(
            "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
            params![id, encode(&supplied)?, epoch],
        )?;
        tx.execute(
            "DELETE FROM lifecycle_claims WHERE operation_id=?1",
            [&p.operation_id],
        )?;
        event(
            &tx,
            s,
            &p.operation_id,
            &p.deployment_id,
            id,
            CandidateLifecycleTransition::CleanupCompleted,
            Some(epoch),
        )?;
        validate_gone_history(&tx, &r.initialize)?;
        tx.commit()?;
        Ok(())
    }
}
