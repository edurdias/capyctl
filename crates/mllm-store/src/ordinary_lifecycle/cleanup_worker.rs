//! Observations and final send fencing for the single owned cleanup worker.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrdinaryCleanupStatus {
    Planned,
    Armed,
    Completed,
    Uncertain,
    Superseded,
    Expired,
}

impl crate::Store {
    /// Observe the exact accepted successor of a failed Initialize. Another
    /// deployment's older queued cleanup must not hide this claim handoff.
    pub fn ordinary_cleanup_for_predecessor(
        &self,
        session: &CoordinatorSession,
        predecessor_step: &str,
    ) -> Result<Option<OrdinaryCleanupReceipt>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let id: Option<String> = tx.query_row(
            "SELECT c.id FROM lifecycle_steps c JOIN operations o ON o.id=c.operation_id JOIN lifecycle_steps p ON p.binding_id=c.binding_id WHERE p.id=?1 AND o.kind='ordinary_cleanup' AND c.state='planned' AND c.session_id=?2 AND p.session_id=?2 ORDER BY o.accepted_at,o.id LIMIT 1",
            params![predecessor_step,session.id()], |r| r.get(0),
        ).optional()?;
        let Some(id) = id else { return Ok(None) };
        let (p, e, state) = read(&tx, &id)?;
        if p.source.step_id != predecessor_step {
            return Err(LifecycleError::CorruptStoredData);
        }
        current_cleanup(&tx, session, &p)?;
        retained(&tx, &p, &e, &state)?;
        Ok(Some(p.receipt))
    }

    /// Durable discovery never confers send authority. Already armed work is
    /// deliberately excluded: observing it cannot authorize another control.
    pub fn next_ordinary_cleanup(
        &self,
        session: &CoordinatorSession,
    ) -> Result<Option<OrdinaryCleanupReceipt>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let id: Option<String> = tx.query_row(
            "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN lifecycle_runs r ON r.operation_id=s.operation_id WHERE o.kind='ordinary_cleanup' AND s.state='planned' AND s.session_id=?1 AND r.session_id=?1 ORDER BY o.accepted_at,o.id LIMIT 1",
            [session.id()], |r| r.get(0),
        ).optional()?;
        let Some(id) = id else { return Ok(None) };
        let (p, e, state) = read(&tx, &id)?;
        current_cleanup(&tx, session, &p)?;
        retained(&tx, &p, &e, &state)?;
        Ok(Some(p.receipt))
    }

    /// Strict observation, including canonical evidence, committed epoch and
    /// exact released binding for a terminal result. No current policy grants
    /// cleanup authority; arm and completion retain their full original proof.
    pub fn ordinary_cleanup_status(
        &self,
        session: &CoordinatorSession,
        id: &str,
        now: i64,
    ) -> Result<OrdinaryCleanupStatus, LifecycleError> {
        if ulid::Ulid::from_string(id).is_err() || now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let (p, e, state) = read(&tx, id)?;
        if state == "completed" {
            return Ok(OrdinaryCleanupStatus::Completed);
        }
        if state == "uncertain" {
            return Ok(OrdinaryCleanupStatus::Uncertain);
        }
        match current_cleanup(&tx, session, &p) {
            Err(LifecycleError::Stale) => return Ok(OrdinaryCleanupStatus::Superseded),
            result => result?,
        }
        retained(&tx, &p, &e, &state)?;
        Ok(match state.as_str() {
            "planned" if now >= p.receipt.deadline_ms => OrdinaryCleanupStatus::Expired,
            "planned" => OrdinaryCleanupStatus::Planned,
            "armed" => OrdinaryCleanupStatus::Armed,
            _ => return Err(LifecycleError::CorruptStoredData),
        })
    }

    /// Revalidate a context held only by the caller that received New. This
    /// read is not permission to recover or replay an earlier control.
    pub fn revalidate_ordinary_cleanup_send(
        &self,
        session: &CoordinatorSession,
        id: &str,
        expected: &CleanupExecutionContext,
        now: i64,
    ) -> Result<i64, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let (p, e, state) = read(&tx, id)?;
        current_cleanup(&tx, session, &p)?;
        retained(&tx, &p, &e, &state)?;
        if state != "armed"
            || context(&p)? != *expected
            || now < expected.issued_at_ms
            || now >= expected.deadline_ms
        {
            return Err(LifecycleError::Conflict);
        }
        Ok(e.host.observation_ttl_ms)
    }
}
