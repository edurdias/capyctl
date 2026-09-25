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
        self.next_ordinary_cleanup_reachable(session, None, 0)
    }

    /// As `next_ordinary_cleanup`, deferring every cleanup of a remote binding
    /// whose host is not in `online` (owner decision 4, 2026-09-22). A deferred
    /// cleanup is never armed, so nothing is sent and nothing is uncertain; it
    /// stays planned, owned and charged until its host reconnects, and cleanups
    /// on other hosts are not held behind it. `None` defers nothing.
    ///
    /// A remote cleanup whose accepted deadline passed while it was deferred can
    /// no longer be armed (the arm refuses an elapsed deadline). It is not
    /// selected, so it cannot halt the worker; it stays planned, owned and
    /// charged, with its engine running. When it is a drain's Stop, its host's
    /// reconnection closes it as expired and issues a fresh one
    /// (`reissue_expired_drain_stops`); any other is left for the operator.
    pub fn next_ordinary_cleanup_reachable(
        &self,
        session: &CoordinatorSession,
        online: Option<&std::collections::BTreeSet<String>>,
        now_ms: i64,
    ) -> Result<Option<OrdinaryCleanupReceipt>, LifecycleError> {
        Ok(self
            .next_ordinary_cleanup_among(session, online, now_ms, &Default::default())?
            .map(|(receipt, _)| receipt))
    }

    /// As `next_ordinary_cleanup_reachable`, skipping every cleanup whose
    /// instance is in `busy.instances`, and answering with the instance the
    /// cleanup belongs to (`None` for a binding without one), which is the lane
    /// the coordinator drives it on.
    // ADR 0015: a Stop never waits behind another deployment's effect.
    pub fn next_ordinary_cleanup_among(
        &self,
        session: &CoordinatorSession,
        online: Option<&std::collections::BTreeSet<String>>,
        now_ms: i64,
        busy: &super::lanes::BusyLanes,
    ) -> Result<Option<(OrdinaryCleanupReceipt, Option<super::lanes::InstanceLane>)>, LifecycleError>
    {
        let online = online
            .map(|hosts| serde_json::to_string(hosts).map_err(|_| LifecycleError::Invalid))
            .transpose()?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let id: Option<String> = tx.query_row(
            &format!("SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id JOIN lifecycle_runs r ON r.operation_id=s.operation_id WHERE o.kind='ordinary_cleanup' AND s.state='planned' AND s.session_id=?1 AND r.session_id=?1 AND (?2 IS NULL OR NOT EXISTS(SELECT 1 FROM remote_binding_ingress ri WHERE ri.binding_id=s.binding_id AND (ri.host_id NOT IN (SELECT value FROM json_each(?2)) OR r.deadline_ms<=?3))) AND {} ORDER BY o.accepted_at,o.id LIMIT 1", super::lanes::lane_free(4)),
            params![session.id(), online, now_ms, busy.instances_json()?], |r| r.get(0),
        ).optional()?;
        let Some(id) = id else { return Ok(None) };
        let (p, e, state) = read(&tx, &id)?;
        current_cleanup(&tx, session, &p)?;
        retained(&tx, &p, &e, &state)?;
        tx.commit()?;
        let lane = self.binding_lane(&p.receipt.binding_id)?;
        Ok(Some((p.receipt, lane)))
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
        // Owner decision 2026-09-22: closed at its deadline, never armed; a
        // drain issued a fresh Stop in its place.
        if state == "cancelled" {
            return Ok(OrdinaryCleanupStatus::Expired);
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
            || context(&tx, &p)? != *expected
            || now < expected.issued_at_ms
            || now >= expected.deadline_ms
        {
            return Err(LifecycleError::Conflict);
        }
        Ok(e.host.observation_ttl_ms)
    }
}
