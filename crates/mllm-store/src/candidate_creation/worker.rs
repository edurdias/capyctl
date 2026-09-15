//! Bounded observation of current-session V3 Fake Initialize work. No send grant.
use super::*;
use crate::resource_policy::ResourcePolicySnapshot;

// Inspect SQLite's borrowed storage before allocating or decoding any new work
// or history DTO. Non-text and oversized values are stored corruption.
fn bounded_text(
    row: &rusqlite::Row<'_>,
    index: usize,
    max: usize,
) -> Result<String, LifecycleError> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(index)? else {
        return Err(LifecycleError::CorruptStoredData);
    };
    if bytes.len() > max {
        return Err(LifecycleError::CorruptStoredData);
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| LifecycleError::CorruptStoredData)
}

#[derive(Clone, Debug)]
pub struct CandidateInitializeWork {
    pub principal: String,
    pub run_id: String,
    pub operation_id: String,
    pub parent_step_id: String,
    pub initialize_step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub host_id: String,
    pub deadline_ms: i64,
    pub policy: ResourcePolicySnapshot,
}

fn policy(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    now: i64,
) -> Result<ResourcePolicySnapshot, LifecycleError> {
    if p.version != 3 || p.action != Action::Initialize {
        return Err(LifecycleError::Unsupported);
    }
    let snapshot = super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    // The closed program accepts only its reviewed Fake recipe.
    QualificationProgram::resolve(snapshot.reviewed_manifest())?;
    let resource = super::super::initialize::policy(tx, &snapshot)?;
    let qualification = read_candidate_policy(tx, &p.scope.host)
        .map_err(super::super::map_qualification)
        .map_err(creation_error)?
        .ok_or(LifecycleError::Conflict)?;
    if resource.revision != p.scope.resource_policy_revision
        || qualification.revision != p.scope.qualification_policy_revision
        || now < p.accepted_at_ms
        || now >= p.deadline_ms
    {
        return Err(LifecycleError::Stale);
    }
    Ok(resource)
}

impl crate::Store {
    /// Read one oldest current-session candidate action, validating its immutable
    /// history and current authority. Armed work is returned for conservative
    /// handling, never converted back to planned work.
    pub fn next_candidate_initialize(
        &self,
        session: &CoordinatorSession,
        now: i64,
    ) -> Result<Option<CandidateInitializeWork>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let json = tx.query_row(
            "SELECT r.plan_json FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id WHERE o.kind='candidate_action_v3' AND o.state IN ('pending','running') AND r.session_id=?1 AND CASE WHEN typeof(r.plan_json)!='text' OR length(CAST(r.plan_json AS BLOB))>?2 OR NOT json_valid(r.plan_json) THEN 1 ELSE json_extract(r.plan_json,'$.version')=3 AND json_extract(r.plan_json,'$.action')='initialize' END ORDER BY o.rowid LIMIT 1",
            params![session.id(),super::super::MAX_BYTES as i64], |r| Ok(bounded_text(r,0,super::super::MAX_BYTES)),
        ).optional()?.transpose()?;
        let Some(json) = json else { return Ok(None) };
        let p: CandidateActionPlanV3 = decode(&json)?;
        validate_plan(&tx, &p)?;
        current(&tx, session, &p)?;
        let policy = policy(&tx, &p, now)?;
        Ok(Some(CandidateInitializeWork {
            principal: p.scope.principal,
            run_id: p.scope.run_id,
            operation_id: p.scope.operation_id,
            parent_step_id: p.scope.parent_step_id,
            initialize_step_id: p
                .effects
                .first()
                .ok_or(LifecycleError::CorruptStoredData)?
                .step_id
                .clone(),
            binding_id: p.scope.binding_id,
            incarnation: p.scope.incarnation,
            host_id: p.scope.host,
            deadline_ms: p.deadline_ms,
            policy,
        }))
    }

    /// Exact durable retry precedes the worker's current admission flags.
    pub fn candidate_initialize_command_receipt(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        text: &str,
    ) -> Result<Option<CandidateActionReceipt>, LifecycleError> {
        if text.len() > super::super::MAX_BYTES
            || !super::super::valid_id(principal)
            || !super::super::valid_id(key)
            || !super::super::ulid(run)
        {
            return Err(LifecycleError::Invalid);
        }
        let command: Command = serde_json::from_str(text).map_err(|_| LifecycleError::Invalid)?;
        if command.action != WireAction::Initialize
            || command.expected_revision < 1
            || command.deadline_ms < 1
        {
            return Err(LifecycleError::Invalid);
        }
        let scope = format!("POST /management/v1/qualification-runs/{run}/actions");
        let hash = format!(
            "{:x}",
            Sha256::digest(encode(&(3_u8, principal, &scope, &command))?)
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let prior = tx.query_row("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3", params![principal,scope,key], |r| Ok((bounded_text(r,0,64),bounded_text(r,1,26),bounded_text(r,2,super::super::MAX_BYTES)))).optional()?;
        let Some((stored, operation, json)) = prior else {
            return Ok(None);
        };
        let (stored, operation, json) = (stored?, operation?, json?);
        if stored != hash {
            return Err(LifecycleError::IdempotencyConflict);
        }
        let r: ReceiptV3 = decode(&json)?;
        if r.version != 3
            || r.principal != principal
            || r.scope != scope
            || r.key != key
            || r.request_hash != hash
            || r.plan.scope.operation_id != operation
            || r.plan.scope.run_id != run
            || r.plan.action != Action::Initialize
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        validate_plan(&tx, &r.plan)?;
        if r.plan.scope.session_id != session.id() {
            return Err(LifecycleError::Stale);
        }
        Ok(Some(receipt(&r.plan)))
    }

    /// Validate current policy after arm and immediately before the caller sends.
    pub fn revalidate_candidate_initialize_send(
        &self,
        session: &CoordinatorSession,
        child: &str,
        expected: &mllm_domain::completion::StepExecutionContext,
        now: i64,
    ) -> Result<i64, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, child)?;
        validate_plan(&tx, &p)?;
        current(&tx, session, &p)?;
        let resource = policy(&tx, &p, now)?;
        drop(tx);
        let (kind, actual) = self.candidate_effect_execution(session, child)?;
        if kind != PersistedEffectKind::Initialize
            || &actual != expected
            || now < actual.issued_at_ms
        {
            return Err(LifecycleError::Stale);
        }
        Ok(resource.controls.observation_ttl_ms)
    }
}

pub(super) fn probe_policy(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &CandidateActionPlanV3,
    now: i64,
) -> Result<i64, LifecycleError> {
    check_session(tx, session)?;
    validate_plan(tx, p)?;
    current(tx, session, p)?;
    Ok(policy(tx, p, now)?.controls.observation_ttl_ms)
}
