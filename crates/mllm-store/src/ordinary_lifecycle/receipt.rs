//! Observation-only command receipts. Reading one never grants arm authority.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifiedStartReceipt {
    operation_id: String,
    deployment_id: String,
    step_id: String,
    binding_id: String,
    incarnation: String,
    revision: i64,
    generation: i64,
    accepted_at_ms: i64,
    deadline_ms: i64,
    joined: bool,
}

impl QualifiedStartReceipt {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn deployment_id(&self) -> &str {
        &self.deployment_id
    }
    pub fn step_id(&self) -> &str {
        &self.step_id
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
    pub fn joined(&self) -> bool {
        self.joined
    }

    fn from_plan(p: &Plan, joined: bool) -> Self {
        Self {
            operation_id: p.operation_id.clone(),
            deployment_id: p.deployment_id.clone(),
            step_id: p.step_id.clone(),
            binding_id: p.binding_id.clone(),
            incarnation: p.incarnation.clone(),
            revision: p.revision,
            generation: p.generation,
            accepted_at_ms: p.accepted_at_ms,
            deadline_ms: p.deadline_ms,
            joined,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    version: u8,
    method: String,
    action: String,
    principal: String,
    scope: String,
    key: String,
    request_hash: String,
    requested_deadline_ms: i64,
    accepted_identity: String,
    receipt: QualifiedStartReceipt,
}

fn scope(deployment: &str) -> String {
    format!("POST:/management/v1/deployments/{deployment}/actions")
}

fn hash(
    principal: &str,
    deployment: &str,
    revision: i64,
    deadline: i64,
) -> Result<String, LifecycleError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            encode(&(1, principal, scope(deployment), revision, "start", deadline))?.as_bytes()
        )
    ))
}

fn accepted_identity(p: &Plan, joined: bool) -> Result<String, LifecycleError> {
    let mut accepted = p.clone();
    accepted.execution = None;
    Ok(format!(
        "{:x}",
        Sha256::digest(encode(&(accepted, joined))?.as_bytes())
    ))
}

pub(super) fn historical_error(error: LifecycleError) -> LifecycleError {
    match error {
        LifecycleError::Sql(rusqlite::Error::QueryReturnedNoRows) => {
            LifecycleError::CorruptStoredData
        }
        LifecycleError::Sql(error) => LifecycleError::Sql(error),
        _ => LifecycleError::CorruptStoredData,
    }
}

pub(super) fn bounded_text(
    row: &rusqlite::Row<'_>,
    column: usize,
    max_bytes: usize,
) -> Result<String, LifecycleError> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column)? else {
        return Err(LifecycleError::CorruptStoredData);
    };
    if bytes.len() > max_bytes {
        return Err(LifecycleError::CorruptStoredData);
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| LifecycleError::CorruptStoredData)
}

fn historical_plan(tx: &Transaction<'_>, id: &str) -> Result<Plan, LifecycleError> {
    let mut statement = tx.prepare("SELECT step_json FROM lifecycle_steps WHERE id=?1")?;
    let mut rows = statement.query([id])?;
    let row = rows.next()?.ok_or(LifecycleError::CorruptStoredData)?;
    decode(&bounded_text(row, 0, 1 << 20)?)
}

/// Inspect immutable associations only: a released binding and an old session
/// remain valid history, never evidence of current ownership or qualification.
fn historical(tx: &Transaction<'_>, stored: &StoredReceipt) -> Result<(), LifecycleError> {
    let r = &stored.receipt;
    let p = historical_plan(tx, &r.step_id)?;
    if p.version != 1
        || p.revision < 1
        || p.generation < 1
        || p.accepted_at_ms < 0
        || p.deadline_ms <= p.accepted_at_ms
        || *r != QualifiedStartReceipt::from_plan(&p, r.joined)
        || stored.accepted_identity != accepted_identity(&p, r.joined)?
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    historical_source(tx, &p)
}

pub(super) fn historical_source(tx: &Transaction<'_>, p: &Plan) -> Result<(), LifecycleError> {
    if p.version != 1
        || p.revision < 1
        || p.generation < 1
        || p.accepted_at_ms < 0
        || p.deadline_ms <= p.accepted_at_ms
    {
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
    .map_err(|error| match error {
        crate::managed_configuration::ManagedConfigurationError::Sql(error) => {
            LifecycleError::Sql(error)
        }
        _ => LifecycleError::CorruptStoredData,
    })?;
    // Re-derive the identity this binding must carry instead of matching one
    // shape of it. ADR 0011 decision 1: every residency is identified the same
    // way now, from its recipe and host, so no shape here needs special-casing.
    let identity = super::binding_identity(&e)?;
    let binding: BindingDto = decode(&p.binding_json)?;
    if binding.version != 1
        || binding.qualification_id != identity.id()
        || binding.payload != identity.payload()?
        || Some(&binding.credential_ref) != e.profile.security.credential_ref.as_ref()
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    crate::lifecycle::validate_candidate_initialize_run(
        tx,
        &p.fence(),
        &p.operation_id,
        &p.session_id,
        p.deadline_ms,
    )
    .map_err(historical_error)?;
    let exact: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND s.operation_id=?2 AND s.deployment_id=?3 AND s.binding_id=?4 AND s.session_id=?5 AND s.ordinal=0 AND o.kind='qualified_initialize' AND o.deployment_id=?3) AND (SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?2)=1 AND EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?4 AND deployment_id=?3 AND revision=?6 AND incarnation=?7 AND ownership='managed' AND binding_json=?8) AND EXISTS(SELECT 1 FROM effective_revisions WHERE deployment_id=?3 AND revision=?6 AND effective_json=?9 AND fingerprint=?10) AND EXISTS(SELECT 1 FROM operations WHERE deployment_id=?3 AND kind='managed_configuration_create' AND state='succeeded') AND NOT EXISTS(SELECT 1 FROM qualification_runs WHERE deployment_id=?3)",
        params![p.step_id,p.operation_id,p.deployment_id,p.binding_id,p.session_id,p.revision,p.incarnation,p.binding_json,p.effective_json,e.qualification_fingerprint], |row| row.get(0))?;
    if !exact {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(())
}

fn lookup_in_transaction(
    tx: &Transaction<'_>,
    principal: &str,
    deployment: &str,
    expected_revision: i64,
    key: &str,
    requested_deadline: i64,
) -> Result<Option<QualifiedStartReceipt>, LifecycleError> {
    check_request(
        principal,
        deployment,
        expected_revision,
        key,
        requested_deadline,
    )?;
    let scope = scope(deployment);
    let request_hash = hash(principal, deployment, expected_revision, requested_deadline)?;
    let prior = {
        let mut statement = tx.prepare("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3")?;
        let mut rows = statement.query(params![principal, scope, key])?;
        rows.next()?
            .map(|row| {
                Ok::<_, LifecycleError>((
                    bounded_text(row, 0, 64)?,
                    bounded_text(row, 1, 26)?,
                    bounded_text(row, 2, 1 << 20)?,
                ))
            })
            .transpose()?
    };
    if let Some((old_hash, operation, raw)) = prior {
        if old_hash != request_hash {
            return Err(LifecycleError::IdempotencyConflict);
        }
        let stored: StoredReceipt = decode(&raw)?;
        if stored.version != 1
            || stored.method != "POST"
            || stored.action != "start"
            || stored.principal != principal
            || stored.scope != scope
            || stored.key != key
            || stored.request_hash != request_hash
            || stored.requested_deadline_ms != requested_deadline
            || stored.receipt.operation_id != operation
            || stored.receipt.deployment_id != deployment
            || stored.receipt.revision != expected_revision
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        historical(tx, &stored)?;
        return Ok(Some(stored.receipt));
    }
    Ok(None)
}

pub(super) fn check_request(
    principal: &str,
    deployment: &str,
    expected_revision: i64,
    key: &str,
    requested_deadline: i64,
) -> Result<(), LifecycleError> {
    if principal.trim().is_empty()
        || principal.len() > 256
        || key.trim().is_empty()
        || key.len() > 256
        || ulid::Ulid::from_string(deployment).is_err()
        || expected_revision < 1
        || requested_deadline <= 0
    {
        return Err(LifecycleError::Invalid);
    }
    Ok(())
}

impl crate::Store {
    /// Read an exact scoped receipt in a read-only transaction. Current session
    /// validation precedes history lookup, including when the receipt is absent.
    /// Historical validation never grants execution or performs new acceptance.
    pub fn qualified_start_command_receipt(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline: i64,
    ) -> Result<Option<QualifiedStartReceipt>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        lookup_in_transaction(
            &tx,
            principal,
            deployment,
            expected_revision,
            key,
            requested_deadline,
        )
    }

    /// Accept a principal-scoped Start command atomically. Exact retries return
    /// committed history before checking today's deployment or qualification.
    /// The caller must use the normal arm path to obtain any execution authority.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_qualified_start_command(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        expected_revision: i64,
        key: &str,
        now: i64,
        requested_deadline: i64,
    ) -> Result<QualifiedStartReceipt, LifecycleError> {
        check_request(
            principal,
            deployment,
            expected_revision,
            key,
            requested_deadline,
        )?;
        if now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let scope = scope(deployment);
        let request_hash = hash(principal, deployment, expected_revision, requested_deadline)?;
        if let Some(receipt) = lookup_in_transaction(
            &tx,
            principal,
            deployment,
            expected_revision,
            key,
            requested_deadline,
        )? {
            return Ok(receipt);
        }
        if requested_deadline <= now {
            return Err(LifecycleError::Invalid);
        }
        let fence = command_fence(&tx, deployment, expected_revision)?;
        let accepted = Self::accept_qualified_start_in_transaction(
            &tx,
            session,
            &fence,
            now,
            requested_deadline,
            true,
        )?;
        let p = historical_plan(&tx, &accepted.step_id)?;
        let receipt = QualifiedStartReceipt::from_plan(&p, accepted.joined);
        let stored = StoredReceipt {
            version: 1,
            method: "POST".into(),
            action: "start".into(),
            principal: principal.into(),
            scope: scope.clone(),
            key: key.into(),
            request_hash: request_hash.clone(),
            requested_deadline_ms: requested_deadline,
            accepted_identity: accepted_identity(&p, receipt.joined)?,
            receipt: receipt.clone(),
        };
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)", params![principal,scope,key,request_hash,receipt.operation_id,encode(&stored)?])?;
        historical(&tx, &stored)?;
        tx.commit()?;
        Ok(receipt)
    }
}

pub(super) fn command_fence(
    tx: &Transaction<'_>,
    deployment: &str,
    expected_revision: i64,
) -> Result<DeploymentFence, LifecycleError> {
    let (revision, generation): (i64, i64) = tx
        .query_row(
            "SELECT revision,current_generation FROM deployments WHERE id=?1",
            [deployment],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or(LifecycleError::NotFound)?;
    if revision != expected_revision {
        return Err(LifecycleError::RevisionConflict);
    }
    Ok(DeploymentFence {
        deployment_id: deployment.into(),
        revision,
        generation,
    })
}
