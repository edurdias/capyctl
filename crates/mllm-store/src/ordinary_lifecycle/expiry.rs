//! Deadline-only release of an exact ordinary Initialize reservation that never armed.
use super::*;

pub(super) const ERROR_CODE: &str = "deadline_expired_unarmed";

fn no_effects(tx: &Transaction<'_>, p: &Plan) -> Result<(), LifecycleError> {
    no_effects_with_successor(tx, p, None, true)
}

// Only the closed unarmed Stop validator may admit its separately validated
// successor. Historical reads ignore a replacement's current ownership.
pub(super) fn no_effects_with_successor(
    tx: &Transaction<'_>,
    p: &Plan,
    successor: Option<(&str, &str)>,
    current: bool,
) -> Result<(), LifecycleError> {
    if p.execution.is_some() {
        return Err(LifecycleError::Conflict);
    }
    let clean: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND identities_json='[]')
         AND NOT EXISTS(SELECT 1 FROM owned_launch_associations WHERE step_id=?2 OR binding_id=?1 OR incarnation=?3)
         AND NOT EXISTS(SELECT 1 FROM lifecycle_evidence WHERE step_id IN (SELECT id FROM lifecycle_steps WHERE id=?2 OR binding_id=?1))
         AND NOT EXISTS(SELECT 1 FROM lifecycle_steps WHERE (id=?2 OR binding_id=?1) AND (state IN ('armed','uncertain') OR (state='completed' AND id IS NOT ?6) OR grant_id IS NOT NULL))
         AND NOT EXISTS(SELECT 1 FROM resource_grants WHERE operation_id=?4 OR operation_id=?7)
         AND ((?8=0 AND EXISTS(SELECT 1 FROM deployments WHERE id=?5 AND (revision>?9 OR current_generation>?10+1))) OR NOT EXISTS(SELECT 1 FROM resource_owners WHERE owner_id=?5))
         AND NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?5 AND (?8 OR (revision<=?9 AND generation<=?10+1)))
         AND NOT EXISTS(SELECT 1 FROM lifecycle_steps WHERE binding_id=?1 AND id!=?2 AND id IS NOT ?6)",
        params![p.binding_id,p.step_id,p.incarnation,p.operation_id,p.deployment_id,successor.map(|s|s.0),successor.map(|s|s.1),current,p.revision,p.generation],
        |r| r.get(0),
    )?;
    if !clean {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}

pub(super) fn terminal(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &Plan,
    effective: &EffectiveDeployment,
    now: i64,
) -> Result<(), LifecycleError> {
    if p.session_id != session.id() || now < p.deadline_ms {
        return Err(LifecycleError::Stale);
    }
    validate_local(tx, p, effective, "cancelled")?;
    no_effects(tx, p)?;
    let exact: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3 AND kind='model' AND desired_state='stopped' AND observed_state='stopped' AND suspended=0 AND admission_enabled=0 AND dispatch_enabled=0)
         AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 OR operation_id=?4)",
        params![p.deployment_id,p.revision,p.generation,p.operation_id],|r|r.get(0),
    )?;
    if !exact {
        return Err(LifecycleError::Stale);
    }
    Ok(())
}

impl crate::Store {
    /// Fail only a current, expired, unarmed ordinary Initialize. This is not
    /// cleanup authority: any evidence of an arm or effects denies release.
    /// Exact terminal retries are read-only and require the same current fence.
    pub fn expire_unarmed_qualified_initialize(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        now_ms: i64,
    ) -> Result<bool, LifecycleError> {
        if ulid::Ulid::from_string(step_id).is_err() || now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let (p, e, state) = load(&tx, step_id)?;
        let changed = expire_in_transaction(&tx, session, &p, &e, &state, now_ms)?;
        tx.commit()?;
        Ok(changed)
    }
}

pub(super) fn expire_in_transaction(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &Plan,
    e: &EffectiveDeployment,
    state: &str,
    now_ms: i64,
) -> Result<bool, LifecycleError> {
    if state == "cancelled" {
        terminal(tx, session, p, e, now_ms)?;
        return Ok(false);
    }
    current(tx, session, p, false)?;
    if state != "planned" || now_ms < p.deadline_ms {
        return Err(LifecycleError::Conflict);
    }
    no_effects(tx, p)?;
    let stopped: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND observed_state='stopped' AND dispatch_enabled=0)",[&p.deployment_id],|r|r.get(0))?;
    if !stopped {
        return Err(LifecycleError::Conflict);
    }
    one(tx.execute("UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='planned' AND session_id=?2 AND grant_id IS NULL",params![p.step_id,session.id()])?)?;
    one(tx.execute("UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND session_id=?2 AND state='queued'",params![p.operation_id,session.id()])?)?;
    one(tx.execute("UPDATE operations SET state='failed',error_code=?2 WHERE id=?1 AND state='pending' AND error_code IS NULL",params![p.operation_id,ERROR_CODE])?)?;
    one(tx.execute("UPDATE deployments SET desired_state='stopped',observed_state='stopped',admission_enabled=0,dispatch_enabled=0 WHERE id=?1 AND revision=?2 AND current_generation=?3",params![p.deployment_id,p.revision,p.generation])?)?;
    one(tx.execute("UPDATE runtime_bindings SET state='released' WHERE id=?1 AND incarnation=?2 AND state='reserved'",params![p.binding_id,p.incarnation])?)?;
    let binding: BindingDto = decode(&p.binding_json)?;
    let endpoint: std::net::SocketAddr = binding
        .endpoint
        .parse()
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    one(tx.execute(
        "DELETE FROM endpoint_leases WHERE binding_id=?1 AND host='127.0.0.1' AND port=?2",
        params![p.binding_id, endpoint.port()],
    )?)?;
    one(tx.execute("DELETE FROM lifecycle_claims WHERE deployment_id=?1 AND operation_id=?2 AND revision=?3 AND generation=?4",params![p.deployment_id,p.operation_id,p.revision,p.generation])?)?;
    event(tx, session, p, Transition::ExpiredUnarmed, None)?;
    Ok(true)
}
