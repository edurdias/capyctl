//! Request leases a crashed coordinator session left on an adopted launch.
//!
//! SPEC §10: queued inference bodies and open streams are not promised to
//! survive a server crash, but client disconnect is not proof the engine stopped
//! working. Conservative accounting is retained until cancellation
//! acknowledgement, completion observation, or controlled cleanup.
//!
//! A restarted coordinator adopts a Ready launch together with the request
//! leases its retired session held on that exact fence (`recovery.rs`,
//! `local_recovery.rs`). Those leases stay charged, and dispatch stays closed
//! while any remain, so an operator Stop still reaches the engine. They are
//! closed as abandoned only by completion observation, never by a timer:
//!
//! - the current session's fresh model probe proved the recorded group alive and
//!   serving, after that session began (the retired session can no longer send);
//! - then the engine was observed quiescent — nothing running or waiting in the
//!   engine and nothing in flight on its only inference path — no earlier than
//!   that probe, and fresh within the host's observation ttl.
//!
//! A controlled cleanup also settles them: gone evidence for the whole recorded
//! group proves no request can still run (`cleanup.rs`).
use super::*;
use mllm_domain::completion::ProcessIdentity;

/// Completion observation for a retired session's requests on one launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuiescenceEvidence {
    pub binding_id: String,
    pub incarnation: String,
    /// The recorded group the probe and the quiescence observation named.
    pub identities: Vec<ProcessIdentity>,
    /// When this session's fresh model probe proved the engine alive.
    pub readiness_observed_at_ms: i64,
    /// When the engine and its inference path were observed with no request
    /// running, waiting or in flight.
    pub quiescent_at_ms: i64,
    pub receipt: String,
}

/// Leases on `p`'s exact fence held by a session other than `s`.
pub(super) fn count(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &Plan,
) -> Result<i64, LifecycleError> {
    Ok(tx.query_row(
        "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND session_id!=?4",
        params![p.deployment_id, p.revision, p.generation, s.id()],
        |r| r.get(0),
    )?)
}

/// The journal clause an adoption appends when it carried retired leases.
pub(super) fn retained_note(leases: i64) -> String {
    if leases == 0 {
        String::new()
    } else {
        format!(
            "; {leases} request lease(s) of the retired session stay charged and dispatch \
             stays closed until engine quiescence is observed or the engine is stopped"
        )
    }
}

/// Dispatch may not reopen while any retired session's lease remains.
pub(super) fn require_settled(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &Plan,
) -> Result<(), LifecycleError> {
    let retired: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?3 AND session_id!=?2)",
        params![p.deployment_id, s.id(), p.instance_index],
        |r| r.get(0),
    )?;
    if retired {
        return Err(LifecycleError::Rejected(
            "request leases of a retired session await engine quiescence".into(),
        ));
    }
    Ok(())
}

/// When session `s` began, on the controller clock. Session ids are ULIDs
/// minted by `begin_coordinator_session` from that same clock.
fn session_started_at_ms(s: &CoordinatorSession) -> Result<i64, LifecycleError> {
    let id = ulid::Ulid::from_string(s.id()).map_err(|_| LifecycleError::CorruptStoredData)?;
    i64::try_from(id.timestamp_ms()).map_err(|_| LifecycleError::CorruptStoredData)
}

/// This session's Ready launch `id`, whatever its host.
fn adopted(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
) -> Result<(Plan, EffectiveDeployment, Association), LifecycleError> {
    let (p, e, state) = load(tx, id)?;
    if state != "completed" {
        return Err(LifecycleError::Conflict);
    }
    current_admitted(tx, s, &p, true, false)?;
    let association = association(tx, &p)?.ok_or(LifecycleError::CorruptStoredData)?;
    Ok((p, e, association))
}

impl crate::Store {
    /// How many retired sessions' request leases the Ready launch `id` of this
    /// session still carries. Observation only.
    pub fn retired_request_leases(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<usize, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let (p, _, _) = adopted(&tx, s, id)?;
        let retired: i64 = tx.query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1 AND instance_index=?3 AND session_id!=?2",
            params![p.deployment_id, s.id(), p.instance_index],
            |r| r.get(0),
        )?;
        usize::try_from(retired).map_err(|_| LifecycleError::CorruptStoredData)
    }

    /// Close the retired sessions' request leases on the Ready launch `id` as
    /// abandoned, on completion observation. Returns how many were closed.
    ///
    /// SPEC §10: never by timer. The evidence must name exactly the associated
    /// group, prove the engine alive through this session's own fresh probe, and
    /// observe it quiescent no earlier than that probe, all within the host's
    /// observation ttl. Nothing else about the launch changes: its reservation,
    /// binding and closed dispatch are untouched.
    pub fn abandon_retired_request_leases(
        &self,
        s: &CoordinatorSession,
        id: &str,
        evidence: &QuiescenceEvidence,
        now: i64,
    ) -> Result<usize, LifecycleError> {
        crate::lifecycle::completion::nonempty_receipt(&evidence.receipt)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, e, association) = adopted(&tx, s, id)?;
        if evidence.binding_id != p.binding_id
            || evidence.incarnation != p.incarnation
            || canonical_members(&evidence.identities).ok()
                != Some(members(&association.identities)?)
        {
            return Err(LifecycleError::Conflict);
        }
        let ttl = policy(&tx, &e)?.controls.observation_ttl_ms;
        let started = session_started_at_ms(s)?;
        let fresh = |at: i64| {
            at >= started && at <= now && now.checked_sub(at).is_some_and(|age| age <= ttl)
        };
        if ttl <= 0
            || !fresh(evidence.readiness_observed_at_ms)
            || !fresh(evidence.quiescent_at_ms)
            || evidence.quiescent_at_ms < evidence.readiness_observed_at_ms
        {
            return Err(LifecycleError::Rejected("evidence freshness".into()));
        }
        let closed = tx.execute(
            "DELETE FROM request_leases WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND session_id!=?4",
            params![p.deployment_id, p.revision, p.generation, s.id()],
        )?;
        // A retired lease on any other fence is not this launch's to settle.
        require_settled(&tx, s, &p).map_err(|_| LifecycleError::Conflict)?;
        if closed > 0 {
            let host: Option<String> = tx
                .query_row(
                    "SELECT host_id FROM remote_binding_ingress WHERE binding_id=?1",
                    [&p.binding_id],
                    |r| r.get(0),
                )
                .optional()?;
            tx.execute(
                "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,?4,?5)",
                params![
                    ulid::Ulid::new().to_string(),
                    host,
                    p.operation_id,
                    "request_leases_abandoned",
                    format!(
                        "deployment {}: {closed} request lease(s) of a retired session closed \
                         as abandoned; model probe at {}, quiescent at {}: {}",
                        p.deployment_id,
                        evidence.readiness_observed_at_ms,
                        evidence.quiescent_at_ms,
                        evidence.receipt
                    ),
                ],
            )?;
        }
        tx.commit()?;
        Ok(closed)
    }
}
