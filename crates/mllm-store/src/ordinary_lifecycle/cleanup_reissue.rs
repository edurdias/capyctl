//! Owner decision 2026-09-22: re-issue of a drain's Stop that expired while its
//! host was offline.
//!
//! A drain's Stop for an offline host waits, planned and unarmed, for the host
//! to reconnect (owner decision 4). An arm refuses an elapsed deadline, so a
//! host that stays away past it would leave the Stop open for ever, with the
//! engine charged and the drain pending. When the host is back, the server
//! closes that Stop as `expired` and issues a fresh ordinary Stop, drain origin
//! and a new deadline, for the same instance, in one transaction.
//!
//! Nothing is released here. The expired Stop never armed, so no control was
//! ever sent for it; its claim passes to the fresh Stop, and the binding,
//! endpoint lease and resource ownership stay exactly as they were until the
//! fresh Stop completes on the host's gone evidence. The drain marker gains the
//! fresh Stop, so the drain stays pending until it settles.
use super::*;

/// The closed code an expired, never-armed drain Stop records.
pub(super) const ERROR_CODE: &str = "expired";

/// The most expired drain Stops one pass considers.
const MAX_PER_PASS: i64 = 64;

/// One drain Stop closed as expired and the fresh Stop issued in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReissuedDrainStop {
    pub host_id: String,
    pub deployment_id: String,
    pub instance: u32,
    pub expired_operation_id: String,
    pub reissued_operation_id: String,
    pub deadline_ms: i64,
}

/// A reissued Stop's predecessor is an ordinary cleanup of the same binding
/// that was closed as expired before it was ever armed.
pub(super) fn validate_predecessor(
    tx: &Transaction<'_>,
    prior: &str,
    binding: &str,
) -> Result<(), LifecycleError> {
    if ulid::Ulid::from_string(prior).is_err() {
        return Err(LifecycleError::CorruptStoredData);
    }
    let exact: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations o JOIN lifecycle_steps s ON s.operation_id=o.id
           WHERE o.id=?1 AND o.kind='ordinary_cleanup' AND o.state='failed' AND o.error_code=?2
           AND s.state='cancelled' AND s.binding_id=?3 AND s.grant_id IS NULL)
         AND NOT EXISTS(SELECT 1 FROM lifecycle_evidence WHERE step_id IN
           (SELECT id FROM lifecycle_steps WHERE operation_id=?1))",
        params![prior, ERROR_CODE, binding],
        |r| r.get(0),
    )?;
    if !exact {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(())
}

impl crate::Store {
    /// Close every expired, never-armed drain Stop whose host is in `online`
    /// and issue a fresh one in its place (see the module documentation).
    ///
    /// A Stop that cannot be proven never armed, or whose instance moved on, is
    /// left exactly as it was: still planned, owned and charged.
    pub fn reissue_expired_drain_stops(
        &self,
        session: &CoordinatorSession,
        online: &std::collections::BTreeSet<String>,
        now_ms: i64,
    ) -> Result<Vec<ReissuedDrainStop>, LifecycleError> {
        if now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        if online.is_empty() {
            return Ok(Vec::new());
        }
        let online = serde_json::to_string(online).map_err(|_| LifecycleError::Invalid)?;
        let candidates: Vec<(String, String)> = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, session)?;
            let mut query = tx.prepare(
                "SELECT s.id, h.host_id FROM lifecycle_steps s
                   JOIN operations o ON o.id=s.operation_id
                   JOIN lifecycle_runs r ON r.operation_id=s.operation_id
                   JOIN host_drains h ON h.operation_id=o.id
                   JOIN remote_binding_ingress ri ON ri.binding_id=s.binding_id AND ri.host_id=h.host_id
                 WHERE o.kind='ordinary_cleanup' AND o.state='pending' AND s.state='planned'
                   AND r.state='queued' AND s.session_id=?1 AND r.session_id=?1
                   AND r.deadline_ms<=?2
                   AND h.host_id IN (SELECT value FROM json_each(?3))
                 ORDER BY o.accepted_at, o.id LIMIT ?4",
            )?;
            let rows = query
                .query_map(params![session.id(), now_ms, online, MAX_PER_PASS], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<Result<_, _>>()?;
            rows
        };
        let mut done = Vec::new();
        for (step, host) in candidates {
            match self.reissue_one(session, &step, &host, now_ms) {
                Ok(reissued) => done.push(reissued),
                // Not provably unarmed, or no longer current: retained as is.
                Err(LifecycleError::Conflict | LifecycleError::Stale) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(done)
    }

    fn reissue_one(
        &self,
        s: &CoordinatorSession,
        id: &str,
        host: &str,
        now: i64,
    ) -> Result<ReissuedDrainStop, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (old, e, state) = read(&tx, id)?;
        let old_operation = old.receipt.operation_id.clone();
        // SPEC §13.2: only a Stop that provably never sent anything may be
        // closed without evidence. A planned step with no issue time, no
        // evidence and no recorded arm was never armed, and a control is only
        // ever sent from an armed step.
        if state != "planned"
            || old.issued_at_ms.is_some()
            || now < old.receipt.deadline_ms
            || now < old.receipt.accepted_at_ms
        {
            return Err(LifecycleError::Conflict);
        }
        current_cleanup(&tx, s, &old)?;
        retained(&tx, &old, &e, &state)?;
        let effects: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_evidence WHERE step_id=?1)
               OR EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND grant_id IS NOT NULL)
               OR EXISTS(SELECT 1 FROM management_events WHERE operation_id=?2
                   AND kind IN ('ordinary_cleanup_armed','ordinary_cleanup_completed'))",
            params![id, old.receipt.operation_id],
            |r| r.get(0),
        )?;
        let drained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM host_drains WHERE host_id=?1 AND operation_id=?2)
               AND EXISTS(SELECT 1 FROM remote_binding_ingress WHERE binding_id=?3 AND host_id=?1)",
            params![host, old.receipt.operation_id, old.receipt.binding_id],
            |r| r.get(0),
        )?;
        if effects || !drained {
            return Err(LifecycleError::Conflict);
        }
        let duration = old
            .receipt
            .deadline_ms
            .checked_sub(old.receipt.accepted_at_ms)
            .filter(|duration| *duration > 0)
            .ok_or(LifecycleError::CorruptStoredData)?;
        let deadline = now.checked_add(duration).ok_or(LifecycleError::Invalid)?;

        // The fresh Stop takes the expired one's claim; the instance moves to a
        // new generation with it, and nothing else changes.
        let fence = target(&old);
        let operation = ulid::Ulid::new().to_string();
        crate::lifecycle::insert_owned_cleanup_run(
            &tx,
            s,
            &fence,
            &operation,
            deadline,
            "ordinary_cleanup",
        )?;
        Self::handoff_claims_in_transaction(
            &tx,
            s,
            &operation,
            &old.receipt.operation_id,
            std::slice::from_ref(&fence),
        )?;
        let generation: i64 = tx.query_row(
            "SELECT generation FROM lifecycle_runs WHERE operation_id=?1",
            [&operation],
            |r| r.get(0),
        )?;

        // Close the expired Stop. It never armed, so this sends and releases
        // nothing; its claim is already the fresh Stop's.
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state='planned' AND session_id=?2 AND grant_id IS NULL",
            params![id, s.id()],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND session_id=?2 AND state='queued'",
            params![old.receipt.operation_id, s.id()],
        )?)?;
        one(tx.execute(
            "UPDATE operations SET state='failed',error_code=?2 WHERE id=?1 AND state='pending' AND error_code IS NULL",
            params![old.receipt.operation_id, ERROR_CODE],
        )?)?;
        read(&tx, id)?;
        event(
            &tx,
            s,
            &old,
            OrdinaryCleanupTransition::ExpiredUnarmed,
            None,
        )?;

        // The fresh Stop: same principal and scope, the drain's own key for
        // this re-issue, a new deadline of the original length.
        let key = format!("drain-reissue:{}", old.receipt.operation_id);
        let receipt = OrdinaryCleanupReceipt {
            operation_id: operation.clone(),
            step_id: ulid::Ulid::new().to_string(),
            binding_id: old.receipt.binding_id.clone(),
            incarnation: old.receipt.incarnation.clone(),
            revision: old.receipt.revision,
            generation,
            accepted_at_ms: now,
            deadline_ms: deadline,
        };
        let command_scope = old.command_scope();
        let command_revision = old.command_revision();
        let deployment_id = old.source.deployment_id.clone();
        let instance = old.source.instance_index;
        let p = CleanupPlan {
            version: 1,
            kind: CleanupKind::OrdinaryCleanup,
            principal: old.principal.clone(),
            key: key.clone(),
            receipt: receipt.clone(),
            source: old.source,
            source_state: old.source_state,
            association: old.association,
            issued_at_ms: None,
            scope: old.scope,
            command_revision: old.command_revision,
            reissue_of: Some(old.receipt.operation_id.clone()),
        };
        tx.execute(
            "INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",
            params![receipt.step_id, receipt.operation_id, deployment_id, receipt.binding_id, s.id(), encode(&p)?],
        )?;
        tx.execute(
            "INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                p.principal,
                command_scope,
                key,
                hash_in(&p.principal, &command_scope, command_revision, deadline)?,
                receipt.operation_id,
                encode(&receipt)?
            ],
        )?;
        let (fresh, fresh_effective, fresh_state) = read(&tx, &receipt.step_id)?;
        current_cleanup(&tx, s, &fresh)?;
        retained(&tx, &fresh, &fresh_effective, &fresh_state)?;
        event(&tx, s, &fresh, OrdinaryCleanupTransition::Accepted, None)?;
        // Owner decision 4: the drain stays pending until the fresh Stop settles.
        tx.execute(
            "INSERT OR IGNORE INTO host_drains(host_id,operation_id,recorded_at_ms) VALUES(?1,?2,?3)",
            params![host, operation, now],
        )?;
        // SPEC §17: both transitions are journaled with the change itself.
        let expired = &old_operation;
        for (journaled, state, evidence) in [
            (
                expired.as_str(),
                "drain_stop_expired",
                format!(
                    "deployment {deployment_id}: instance {instance} drain Stop expired while host \
                     {host} was offline; it was never armed, so nothing was sent; closed as expired \
                     and replaced by {operation}; nothing released"
                ),
            ),
            (
                operation.as_str(),
                "drain_stop_reissued",
                format!(
                    "deployment {deployment_id}: instance {instance} drain Stop reissued after host \
                     {host} reconnected, replacing expired {expired}; deadline {deadline}; cleanup \
                     completes only on gone evidence"
                ),
            ),
        ] {
            tx.execute(
                "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,?4,?5)",
                params![ulid::Ulid::new().to_string(), host, journaled, state, evidence],
            )?;
        }
        tx.commit()?;
        Ok(ReissuedDrainStop {
            host_id: host.to_owned(),
            deployment_id,
            instance,
            expired_operation_id: old_operation,
            reissued_operation_id: operation,
            deadline_ms: deadline,
        })
    }
}
