//! Closed explicit Stop for an ordinary Initialize that has never armed.
//! Acceptance transfers responsibility; only the owned worker, after its prior
//! task exits, calls completion. This module confers no runtime effect authority.
use super::cleanup::OrdinaryCleanupReceipt;
use super::*;
use crate::events::{append_event, EventMetadata, EventOperationId, UnarmedStopTransition};

const ERROR_CODE: &str = "stopped_before_initialize_armed";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StopKind {
    OrdinaryCleanup,
    OrdinaryUnarmedStop,
}

/// Observation-only common command receipt. Legacy cleanup APIs retain their
/// original receipt; this command envelope cannot be used to arm either path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrdinaryStopReceipt {
    kind: StopKind,
    pub operation_id: String,
    pub step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub revision: i64,
    pub generation: i64,
    pub accepted_at_ms: i64,
    pub deadline_ms: i64,
}
impl OrdinaryStopReceipt {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn revision(&self) -> i64 {
        self.revision
    }
}
impl From<OrdinaryCleanupReceipt> for OrdinaryStopReceipt {
    fn from(r: OrdinaryCleanupReceipt) -> Self {
        Self {
            kind: StopKind::OrdinaryCleanup,
            operation_id: r.operation_id,
            step_id: r.step_id,
            binding_id: r.binding_id,
            incarnation: r.incarnation,
            revision: r.revision,
            generation: r.generation,
            accepted_at_ms: r.accepted_at_ms,
            deadline_ms: r.deadline_ms,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopPlan {
    version: u8,
    principal: String,
    key: String,
    receipt: OrdinaryStopReceipt,
    source: Plan,
}
fn target(p: &StopPlan) -> DeploymentFence {
    DeploymentFence {
        deployment_id: p.source.deployment_id.clone(),
        revision: p.receipt.revision,
        generation: p.receipt.generation,
    }
}
fn event(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &StopPlan,
    transition: UnarmedStopTransition,
) -> Result<(), LifecycleError> {
    let id = |v: &str| {
        v.parse()
            .map(EventOperationId::generated)
            .map_err(|_| LifecycleError::CorruptStoredData)
    };
    append_event(
        tx,
        &EventMetadata::UnarmedStopRecorded {
            transition,
            operation_id: id(&p.receipt.operation_id)?,
            deployment_id: id(&p.source.deployment_id)?,
            step_id: id(&p.receipt.step_id)?,
            session_epoch: session.epoch(),
            committed_epoch: None,
        },
    )
    .map(|_| ())
    .map_err(|e| match e {
        crate::events::EventWriteError::Sql(e) => LifecycleError::Sql(e),
        _ => LifecycleError::CorruptStoredData,
    })
}

fn read(tx: &Transaction<'_>, id: &str) -> Result<(StopPlan, String), LifecycleError> {
    let (raw, state) = {
        let mut statement=tx.prepare("SELECT s.step_json,s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='ordinary_unarmed_stop'")?;
        let mut rows = statement.query([id])?;
        let row = rows.next()?.ok_or(LifecycleError::CorruptStoredData)?;
        (
            super::receipt::bounded_text(row, 0, 1 << 20)?,
            super::receipt::bounded_text(row, 1, 16)?,
        )
    };
    let p: StopPlan = decode(&raw)?;
    let r = &p.receipt;
    let original = &p.source;
    super::receipt::historical_source(tx, original)?;
    let e = decode_effective_snapshot(&original.effective_json)
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    if p.version != 1
        || r.kind != StopKind::OrdinaryUnarmedStop
        || r.step_id != id
        || r.binding_id != original.binding_id
        || r.incarnation != original.incarnation
        || r.revision != original.revision
        || Some(r.generation) != original.generation.checked_add(1)
        || r.accepted_at_ms < original.accepted_at_ms
        || r.deadline_ms <= r.accepted_at_ms
        || r.deadline_ms
            .checked_sub(r.accepted_at_ms)
            .is_none_or(|d| d > e.request_deadline_ms)
        || original.execution.is_some()
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    super::receipt::check_request(
        &p.principal,
        &original.deployment_id,
        r.revision,
        &p.key,
        r.deadline_ms,
    )
    .map_err(super::receipt::historical_error)?;
    if [&r.operation_id, &r.step_id]
        .iter()
        .any(|id| ulid::Ulid::from_string(id).is_err())
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let terminal = match state.as_str() {
        "planned" => false,
        "completed" => true,
        _ => return Err(LifecycleError::CorruptStoredData),
    };
    let run = crate::lifecycle::validate_cleanup_run(
        tx,
        &target(&p),
        &r.operation_id,
        &original.session_id,
        r.deadline_ms,
        Some(&original.operation_id),
    )?;
    // The common Stop run validator admits armed cleanup predecessors too.
    // This closed variant must additionally prove the frozen planned handoff.
    let unarmed_history: bool = tx.query_row(
        "SELECT json_extract(plan_json,'$.handoffs[0].steps[0].state')='planned' FROM lifecycle_runs WHERE operation_id=?1",
        [&r.operation_id], |r| r.get(0),
    )?;
    if !unarmed_history {
        return Err(LifecycleError::CorruptStoredData);
    }
    let source_run = crate::lifecycle::validate_initialize_run(
        tx,
        &original.fence(),
        &original.operation_id,
        &original.session_id,
        original.deadline_ms,
    )?;
    if run != if terminal { "succeeded" } else { "queued" }
        || source_run != if terminal { "failed" } else { "queued" }
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND s.operation_id=?2 AND s.deployment_id=?3 AND s.binding_id=?4 AND s.session_id=?5 AND s.ordinal=0 AND s.grant_id IS NULL AND o.deployment_id=?3 AND o.kind='ordinary_unarmed_stop' AND o.state=?6 AND o.error_code IS NULL) AND (SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?2)=1 AND EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?7 AND s.step_json=?8 AND s.state=?9 AND s.grant_id IS NULL AND o.state=?10 AND o.error_code IS ?11) AND EXISTS(SELECT 1 FROM command_receipts WHERE principal_id=?12 AND command_scope=?13 AND idempotency_key=?14 AND request_hash=?15 AND operation_id=?2 AND response_json=?16) AND (SELECT COUNT(*) FROM command_receipts WHERE operation_id=?2)=1 AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE operation_id=?17)",params![id,r.operation_id,original.deployment_id,r.binding_id,original.session_id,if terminal {"succeeded"}else{"pending"},original.step_id,encode(original)?,if terminal {"cancelled"}else{"planned"},if terminal {"failed"}else{"pending"},terminal.then_some(ERROR_CODE),p.principal,cleanup::scope(&original.deployment_id),p.key,cleanup::hash(&p.principal,&original.fence(),r.deadline_ms)?,encode(r)?,original.operation_id],|r|r.get(0))?;
    if !exact {
        return Err(LifecycleError::CorruptStoredData);
    }
    expiry::no_effects_with_successor(
        tx,
        original,
        Some((&r.step_id, &r.operation_id)),
        !terminal,
    )?;
    let b: BindingDto = decode(&original.binding_json)?;
    let endpoint: std::net::SocketAddr = b
        .endpoint
        .parse()
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    if endpoint.ip() != std::net::Ipv4Addr::LOCALHOST
        || !(e.host.endpoint_port_range.start..=e.host.endpoint_port_range.end)
            .contains(&endpoint.port())
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let reserved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND state=?2) AND (SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?1)=?3 AND (?4 OR EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?1 AND host='127.0.0.1' AND port=?5)) AND (?4=0 OR NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE operation_id=?6))",params![r.binding_id,if terminal {"released"}else{"reserved"},i64::from(!terminal),terminal,endpoint.port(),r.operation_id],|r|r.get(0))?;
    if !reserved {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok((p, state))
}

fn current(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &StopPlan,
) -> Result<(), LifecycleError> {
    check_session(tx, session)?;
    let r = &p.receipt;
    let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3 AND kind='model' AND desired_state='stopped' AND observed_state='stopped' AND suspended=0 AND admission_enabled=0 AND dispatch_enabled=0) AND EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND operation_id=?4 AND revision=?2 AND generation=?3) AND (SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?4)=1 AND (SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?1 AND state!='released')=1 AND NOT EXISTS(SELECT 1 FROM lifecycle_steps WHERE deployment_id=?1 AND id NOT IN (?5,?6) AND state IN ('planned','armed','uncertain'))",params![p.source.deployment_id,r.revision,r.generation,r.operation_id,p.source.step_id,r.step_id],|r|r.get(0))?;
    if !exact || p.source.session_id != session.id() {
        return Err(LifecycleError::Stale);
    }
    Ok(())
}

pub(super) fn lookup(
    tx: &Transaction<'_>,
    principal: &str,
    f: &DeploymentFence,
    key: &str,
    deadline: i64,
    operation: &str,
    raw: &str,
) -> Result<OrdinaryStopReceipt, LifecycleError> {
    let receipt: OrdinaryStopReceipt = decode(raw)?;
    let (p, _) = read(tx, &receipt.step_id).map_err(super::receipt::historical_error)?;
    if p.receipt != receipt
        || receipt.operation_id != operation
        || receipt.deadline_ms != deadline
        || p.source.deployment_id != f.deployment_id
        || receipt.revision != f.revision
        || p.principal != principal
        || p.key != key
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn accept(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    principal: &str,
    f: &DeploymentFence,
    key: &str,
    now: i64,
    deadline: i64,
) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
    let id:Option<String>=tx.query_row("SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.deployment_id=?1 AND o.kind='initialize' AND b.state!='released' AND s.state='planned'",[&f.deployment_id],|r|r.get(0)).optional()?;
    let Some(id) = id else { return Ok(None) };
    let (original, e, state) = load(tx, &id)?;
    // ADR 0011 decision 4: a deployment that gave up closes its own admission.
    // An operator Stop of that deployment must still be accepted, or the
    // reservation and the endpoint lease are held until the original deadline
    // with no recourse. The `no_effects_with_successor` proof below still shows
    // that the step never armed, exactly as the deadline release does.
    super::current_admitted(tx, s, &original, false, false)?;
    if original.fence() != *f
        || state != "planned"
        || now < original.accepted_at_ms
        || deadline <= now
        || deadline
            .checked_sub(now)
            .is_none_or(|d| d > e.request_deadline_ms)
    {
        return Err(LifecycleError::Conflict);
    }
    expiry::no_effects_with_successor(tx, &original, None, true)?;
    let stopped:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND observed_state='stopped' AND dispatch_enabled=0) AND NOT EXISTS(SELECT 1 FROM lifecycle_steps WHERE deployment_id=?1 AND id!=?2 AND state IN ('planned','armed','uncertain'))",params![f.deployment_id,id],|r|r.get(0))?;
    if !stopped {
        return Err(LifecycleError::Conflict);
    }
    let next = crate::Store::fence_lifecycle_in_transaction(tx, s, f, false)?;
    let receipt = OrdinaryStopReceipt {
        kind: StopKind::OrdinaryUnarmedStop,
        operation_id: ulid::Ulid::new().to_string(),
        step_id: ulid::Ulid::new().to_string(),
        binding_id: original.binding_id.clone(),
        incarnation: original.incarnation.clone(),
        revision: next.revision,
        generation: next.generation,
        accepted_at_ms: now,
        deadline_ms: deadline,
    };
    crate::lifecycle::insert_owned_cleanup_run(
        tx,
        s,
        &next,
        &receipt.operation_id,
        deadline,
        "ordinary_unarmed_stop",
    )?;
    crate::Store::handoff_claims_in_transaction(
        tx,
        s,
        &receipt.operation_id,
        &original.operation_id,
        std::slice::from_ref(&next),
    )?;
    let p = StopPlan {
        version: 1,
        principal: principal.into(),
        key: key.into(),
        receipt: receipt.clone(),
        source: original,
    };
    tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![receipt.step_id,receipt.operation_id,f.deployment_id,receipt.binding_id,s.id(),encode(&p)?])?;
    tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,cleanup::scope(&f.deployment_id),key,cleanup::hash(principal,f,deadline)?,receipt.operation_id,encode(&receipt)?])?;
    one(tx.execute(
        "UPDATE deployments SET admission_enabled=0 WHERE id=?1",
        [&f.deployment_id],
    )?)?;
    read(tx, &receipt.step_id)?;
    current(tx, s, &p)?;
    event(tx, s, &p, UnarmedStopTransition::Accepted)?;
    Ok(Some(receipt))
}

impl crate::Store {
    /// Find only an exact closed successor; never infer cancellation from a
    /// missing association. The caller must await its prior task before release.
    pub fn unarmed_stop_for_predecessor(
        &self,
        s: &CoordinatorSession,
        predecessor: &str,
    ) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let id:Option<String>=tx.query_row("SELECT c.id FROM lifecycle_steps c JOIN operations o ON o.id=c.operation_id JOIN lifecycle_steps p ON p.binding_id=c.binding_id WHERE p.id=?1 AND o.kind='ordinary_unarmed_stop' AND c.state='planned' AND c.session_id=?2",params![predecessor,s.id()],|r|r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        let (p, _) = read(&tx, &id)?;
        if p.source.step_id != predecessor {
            return Err(LifecycleError::CorruptStoredData);
        }
        current(&tx, s, &p)?;
        Ok(Some(p.receipt))
    }

    /// Worker-only lifecycle transition for the exact accepted Stop. It accepts
    /// no caller-selected binding, release proof, or resource accounting input.
    pub fn complete_unarmed_stop(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, state) = read(&tx, id)?;
        if state == "completed" {
            return Ok(false);
        }
        current(&tx, s, &p)?;
        let r = &p.receipt;
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='planned'",
            [&p.source.step_id],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state='queued'",
            [&p.source.operation_id],
        )?)?;
        one(tx.execute("UPDATE operations SET state='failed',error_code=?2 WHERE id=?1 AND state='pending' AND error_code IS NULL",params![p.source.operation_id,ERROR_CODE])?)?;
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='planned'",
            [&r.step_id],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='queued'",
            [&r.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='pending'",
            [&r.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE runtime_bindings SET state='released' WHERE id=?1 AND state='reserved'",
            [&r.binding_id],
        )?)?;
        one(tx.execute(
            "DELETE FROM endpoint_leases WHERE binding_id=?1",
            [&r.binding_id],
        )?)?;
        // Spec §3: the key row is deleted in the same transaction that releases
        // the binding, so the stored secret set is exactly the set of engines that
        // exist. The coordinator seals the key before it arms the step, so a start
        // stopped while still planned has one; a launch that never stored a key
        // has none, which is why this is not `one`.
        tx.execute(
            "DELETE FROM engine_secrets WHERE binding_id=?1",
            [&r.binding_id],
        )?;
        one(tx.execute("DELETE FROM lifecycle_claims WHERE deployment_id=?1 AND operation_id=?2 AND revision=?3 AND generation=?4",params![p.source.deployment_id,r.operation_id,r.revision,r.generation])?)?;
        read(&tx, id)?;
        event(&tx, s, &p, UnarmedStopTransition::Completed)?;
        tx.commit()?;
        Ok(true)
    }

    /// Bounded discovery for the owned worker, after previous work has exited.
    pub fn next_unarmed_stop(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Option<OrdinaryStopReceipt>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let id:Option<String>=tx.query_row("SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE o.kind='ordinary_unarmed_stop' AND s.state='planned' AND s.session_id=?1 ORDER BY o.accepted_at,o.id LIMIT 1",[s.id()],|r|r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        let (p, _) = read(&tx, &id)?;
        current(&tx, s, &p)?;
        Ok(Some(p.receipt))
    }
}
