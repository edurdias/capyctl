//! Adoption of an accepted Stop whose coordinator session is gone (W12).
//!
//! SPEC §13.2: on restart, reconcile before dispatch. A Stop accepted before a
//! crash left its cleanup step `planned` (never armed) or `uncertain` (armed,
//! so a Terminate may already have gone out). Both belonged to the retired
//! session and nothing could act on them: the deployment was stranded with its
//! reservation charged and every further Stop refused as a conflict.
//!
//! A restarted coordinator adopts such a cleanup, with the launch it stops, in
//! one transaction. Everything is rewritten to the current session exactly as
//! that session would have written it, then revalidated as a `planned` cleanup
//! of this session before commit, so the ordinary worker path arms it and
//! completes it only on gone evidence for exactly the recorded group. An
//! `uncertain` step returns to `planned`: its Terminate is re-sent to the same
//! recorded identities, which terminates nothing else (identity includes boot
//! and start ticks), and the journal records that it may be a repeat.
//!
//! The Stop keeps the deadline it was accepted with; a cleanup whose deadline
//! has passed is not adopted, because the worker never sends past it.
use super::*;

/// A retired session's accepted Stop that the current session may adopt, with
/// the launch it stops (the driver is rebuilt from it). Observation only.
pub struct RetiredCleanup {
    pub work: crate::ordinary_lifecycle::worker::InitializeWork,
    pub receipt: OrdinaryCleanupReceipt,
    /// The launch runs on an enrolled remote host, not in this process's host.
    pub remote: bool,
}

/// The most retired cleanups one discovery pass returns.
const MAX_RETIRED: i64 = 64;

fn remote(tx: &Transaction<'_>, binding_id: &str) -> Result<Option<String>, LifecycleError> {
    Ok(tx
        .query_row(
            "SELECT host_id FROM remote_binding_ingress WHERE binding_id=?1",
            [binding_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Validate one retired session's cleanup exactly as it was left.
fn retired(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    now: i64,
) -> Result<(CleanupPlan, EffectiveDeployment, String), LifecycleError> {
    let (p, e, state) = read(tx, id)?;
    if p.source.session_id == s.id()
        || !matches!(state.as_str(), "planned" | "uncertain")
        // A source still armed at acceptance has no settled outcome to stop from.
        || !matches!(p.source_state.as_str(), "completed" | "uncertain")
    {
        return Err(LifecycleError::Conflict);
    }
    if now >= p.receipt.deadline_ms {
        return Err(LifecycleError::Rejected(
            "the Stop deadline has passed".into(),
        ));
    }
    Ok((p, e, state))
}

impl crate::Store {
    /// Retired sessions' accepted Stops this session may adopt, oldest first.
    pub fn retired_cleanups(
        &self,
        s: &CoordinatorSession,
        now: i64,
    ) -> Result<Vec<RetiredCleanup>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let ids = tx
            .prepare(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                 WHERE o.kind='ordinary_cleanup' AND s.state IN ('planned','uncertain') AND s.session_id!=?1
                 ORDER BY o.accepted_at,o.id LIMIT ?2",
            )?
            .query_map(params![s.id(), MAX_RETIRED], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut cleanups = Vec::new();
        for id in ids {
            // A cleanup that no longer validates stays with its retired session:
            // nothing of it is released, it is only not adopted.
            let Ok((p, e, _)) = retired(&tx, s, &id, now) else {
                continue;
            };
            let remote = remote(&tx, &p.source.binding_id)?.is_some();
            let Ok(work) =
                crate::ordinary_lifecycle::worker::prepare_work(&tx, p.source.clone(), e)
            else {
                continue;
            };
            cleanups.push(RetiredCleanup {
                work,
                receipt: p.receipt,
                remote,
            });
        }
        Ok(cleanups)
    }

    /// Transfer one retired session's accepted Stop, and the launch it stops, to
    /// this session as a `planned` cleanup the worker will arm.
    ///
    /// SPEC §13.2: nothing is released and nothing is sent here. The reservation,
    /// claim, binding and recorded identities are kept exactly; completion still
    /// needs gone evidence for exactly the recorded group.
    pub fn adopt_retired_cleanup(
        &self,
        s: &CoordinatorSession,
        id: &str,
        now: i64,
    ) -> Result<OrdinaryCleanupReceipt, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (mut p, _, state) = retired(&tx, s, id, now)?;
        let old = std::mem::replace(&mut p.source.session_id, s.id().into());
        // The launch the Stop targets moves with it.
        one(tx.execute(
            "UPDATE lifecycle_steps SET session_id=?2,step_json=?3 WHERE id=?1 AND session_id=?4",
            params![p.source.step_id, s.id(), encode(&p.source)?, old],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET session_id=?2 WHERE operation_id=?1 AND session_id=?3",
            params![p.source.operation_id, s.id(), old],
        )?)?;
        if let Some(association) = p.association.as_mut() {
            association.session_id = s.id().into();
            one(tx.execute(
                "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
                params![p.source.step_id, encode(association)?],
            )?)?;
        }
        let armed_before = state == "uncertain";
        if armed_before {
            // The prior arm's outcome is unknown; the cleanup is re-armed from
            // `planned` so the worker sends one fresh, fenced Terminate.
            p.issued_at_ms = None;
            one(tx.execute(
                "UPDATE lifecycle_runs SET state='queued' WHERE operation_id=?1 AND state='uncertain'",
                [&p.receipt.operation_id],
            )?)?;
            one(tx.execute(
                "UPDATE operations SET state='pending' WHERE id=?1 AND state='running'",
                [&p.receipt.operation_id],
            )?)?;
            if p.source_state == "completed" {
                // Arming marked a Ready binding uncertain; `planned` holds it as
                // the Ready binding it still is until the re-arm marks it again.
                one(tx.execute(
                    "UPDATE runtime_bindings SET state='live' WHERE id=?1 AND state='uncertain'",
                    [&p.receipt.binding_id],
                )?)?;
            }
        }
        one(tx.execute(
            "UPDATE lifecycle_steps SET session_id=?2,state='planned',step_json=?3 WHERE id=?1 AND session_id=?4 AND state IN ('planned','uncertain')",
            params![id, s.id(), encode(&p)?, old],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET session_id=?2 WHERE operation_id=?1 AND session_id=?3",
            params![p.receipt.operation_id, s.id(), old],
        )?)?;
        // Revalidate as this session's own planned cleanup before commit.
        let (adopted, e, state) = read(&tx, id)?;
        if state != "planned" || adopted.receipt != p.receipt {
            return Err(LifecycleError::CorruptStoredData);
        }
        current_cleanup(&tx, s, &adopted)?;
        retained(&tx, &adopted, &e, &state)?;
        let host = remote(&tx, &p.receipt.binding_id)?;
        tx.execute(
            "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,?4,?5)",
            params![
                ulid::Ulid::new().to_string(),
                host,
                p.receipt.operation_id,
                "cleanup_adopted",
                format!(
                    "deployment {}: a restarted controller adopted its accepted Stop; the \
                     cleanup resumes and completes only on gone evidence for the recorded \
                     group{}",
                    p.source.deployment_id,
                    if armed_before {
                        "; a Terminate may already have been sent before the restart and is \
                         sent again to the same recorded identities"
                    } else {
                        ""
                    }
                ),
            ],
        )?;
        tx.commit()?;
        Ok(p.receipt)
    }
}

impl crate::Store {
    /// ADR 0015 follow-up (SPEC §13.2, ADR 0011 decision 4): return this
    /// session's armed cleanup whose Terminate could not be classified (its host
    /// dropped mid-Stop, the send timed out) to `planned`, so the ordinary path
    /// re-arms it and sends one fresh, fenced Terminate to exactly the recorded
    /// identities once the host is reachable. Before this, only a restarted
    /// coordinator (via [`Self::adopt_retired_cleanup`]) could resume such a
    /// cleanup, so the instance stayed charged and paused for the whole session.
    ///
    /// Nothing is released and nothing is sent here: the binding, claim,
    /// reservation and recorded identities are kept exactly, completion still
    /// needs gone evidence for the recorded group, and the journal records that
    /// the next Terminate may be a repeat. The Stop keeps its accepted deadline;
    /// past it the cleanup is not re-planned (the arm refuses an elapsed
    /// deadline) and stays uncertain and charged.
    ///
    /// `Ok(false)` when the step is already `planned` (it failed before its
    /// arm; ordinary discovery retries it). Anything else — another session's
    /// step, a completed or cancelled one — is a conflict.
    pub fn replan_unproven_cleanup(
        &self,
        s: &CoordinatorSession,
        id: &str,
        now: i64,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (mut p, _, state) = read(&tx, id)?;
        let owned: Option<String> = tx
            .query_row(
                "SELECT session_id FROM lifecycle_steps WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if owned.as_deref() != Some(s.id()) {
            return Err(LifecycleError::Conflict);
        }
        match state.as_str() {
            "planned" => return Ok(false),
            "armed" => {}
            _ => return Err(LifecycleError::Conflict),
        }
        if now >= p.receipt.deadline_ms {
            return Err(LifecycleError::Rejected(
                "the Stop deadline has passed".into(),
            ));
        }
        p.issued_at_ms = None;
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='planned',step_json=?2 WHERE id=?1 AND state='armed' AND session_id=?3",
            params![id, encode(&p)?, s.id()],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='queued' WHERE operation_id=?1 AND state='running'",
            [&p.receipt.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE operations SET state='pending' WHERE id=?1 AND state='running'",
            [&p.receipt.operation_id],
        )?)?;
        if p.source_state == "completed" {
            // Arming marked the Ready binding uncertain; `planned` holds it as
            // the Ready binding it still is until the re-arm marks it again.
            one(tx.execute(
                "UPDATE runtime_bindings SET state='live' WHERE id=?1 AND state='uncertain'",
                [&p.receipt.binding_id],
            )?)?;
        }
        // Revalidate as this session's own planned cleanup before commit.
        let (replanned, e, state) = read(&tx, id)?;
        if state != "planned" || replanned.receipt != p.receipt {
            return Err(LifecycleError::CorruptStoredData);
        }
        current_cleanup(&tx, s, &replanned)?;
        retained(&tx, &replanned, &e, &state)?;
        let host = remote(&tx, &p.receipt.binding_id)?;
        tx.execute(
            "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,?4,?5)",
            params![
                ulid::Ulid::new().to_string(),
                host,
                p.receipt.operation_id,
                "cleanup_retried",
                format!(
                    "deployment {}: its Stop could not be proven and is retried in this \
                     session; a Terminate may already have been sent and is sent again to \
                     the same recorded identities",
                    p.source.deployment_id
                ),
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::{armed_ordinary, identity};
    use super::super::super::*;
    use super::super::OrdinaryCleanupStatus;
    use mllm_domain::completion::{CleanupEvidence, ProcessIdentity};

    fn group() -> Vec<ProcessIdentity> {
        vec![identity("api", 71), identity("worker-0", 72)]
    }

    /// A Ready embedded launch in the fixture's session.
    fn ready() -> (
        crate::Store,
        CoordinatorSession,
        DeploymentFence,
        StepExecutionContext,
    ) {
        let (store, session, fence, execution) = armed_ordinary();
        let step = execution.token.step_id.clone();
        let now = execution.issued_at_ms + 5;
        store
            .record_owned_launch(
                &session,
                &step,
                &OwnedLaunchReceipt {
                    binding_id: execution.binding_id.clone(),
                    incarnation: execution.incarnation.clone(),
                    identities: group(),
                    observed_at_ms: now,
                    receipt: "owned launch observed".into(),
                },
                now,
            )
            .unwrap();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        store
            .complete_step(
                &session,
                &step,
                &CompletionEvidence {
                    token: execution.token.clone(),
                    identities: group(),
                    observed_at_ms: now,
                    control_receipt: Some("model list names the route".into()),
                    milestones: vec![
                        mllm_domain::completion::Milestone::AllocationsRestored,
                        mllm_domain::completion::Milestone::WeightsUsable,
                        mllm_domain::completion::Milestone::CacheValid,
                        mllm_domain::completion::Milestone::ModelUsable,
                    ],
                },
                now,
                ttl,
            )
            .unwrap();
        (store, session, fence, execution)
    }

    fn gone(execution: &StepExecutionContext, at: i64) -> CleanupEvidence {
        CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: group(),
            observed_at_ms: at,
            receipt: "every recorded process gone".into(),
        }
    }

    fn owns(store: &crate::Store, deployment: &str) -> bool {
        store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(deployment)
    }

    /// W12: a Stop accepted just before a crash was stranded: its cleanup was
    /// the retired session's, and every later Stop was refused. The restarted
    /// session adopts it and the ordinary path completes it on gone evidence.
    // T33 T32 T10
    #[test]
    fn a_restart_resumes_a_stop_accepted_before_the_crash() {
        let (store, old, fence, execution) = ready();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10_000;
        let receipt = store
            .accept_administrative_stop_command(
                &old,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-before-crash",
                now,
                now + 50_000,
            )
            .unwrap();
        let session = store.begin_coordinator_session().unwrap();
        // Before adoption the Stop is nobody's, and a second Stop cannot join it.
        assert!(store.next_ordinary_cleanup(&session).unwrap().is_none());
        assert!(store.retired_local_launches(&session).unwrap().is_empty());
        let retired = store.retired_cleanups(&session, now + 1).unwrap();
        assert_eq!(retired.len(), 1);
        assert!(!retired[0].remote);
        assert_eq!(retired[0].receipt.step_id, receipt.step_id);
        assert_eq!(retired[0].work.step_id(), step);
        store
            .adopt_retired_cleanup(&session, &receipt.step_id, now + 1)
            .unwrap();
        assert!(store
            .retired_cleanups(&session, now + 2)
            .unwrap()
            .is_empty());
        assert!(store
            .adopt_retired_cleanup(&session, &receipt.step_id, now + 2)
            .is_err());
        // Adoption releases nothing.
        assert!(owns(&store, &fence.deployment_id));
        let next = store.next_ordinary_cleanup(&session).unwrap().unwrap();
        assert_eq!(next.step_id, receipt.step_id);
        let (_, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 3)
            .unwrap();
        assert_eq!(context.unwrap().identities, group());
        store
            .complete_cleanup(
                &session,
                &receipt.step_id,
                &gone(&execution, now + 4),
                now + 5,
                ttl,
            )
            .unwrap();
        assert_eq!(
            store
                .ordinary_cleanup_status(&session, &receipt.step_id, now + 6)
                .unwrap(),
            OrdinaryCleanupStatus::Completed
        );
        assert!(!owns(&store, &fence.deployment_id));
    }

    /// W12: a cleanup armed before the crash went uncertain: a Terminate may
    /// have gone out. It is adopted back to planned, re-armed once, and the
    /// journal says the Terminate may be a repeat. Leases the crashed session
    /// held are released with the gone proof (controlled cleanup, SPEC §10).
    // T33 T32 T34
    #[test]
    fn a_restart_resumes_a_stop_that_was_armed_when_it_crashed() {
        let (store, old, fence, execution) = ready();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10_000;
        store
            .conn
            .execute(
                "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('crashed',?1,?2,?3,?4,'inflight')",
                rusqlite::params![fence.deployment_id, fence.revision, fence.generation, old.id()],
            )
            .unwrap();
        let receipt = store
            .accept_administrative_stop_command(
                &old,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-armed-crash",
                now,
                now + 50_000,
            )
            .unwrap();
        store
            .arm_ordinary_cleanup_with_context(&old, &receipt.step_id, now + 1)
            .unwrap();
        let session = store.begin_coordinator_session().unwrap();
        assert_eq!(
            store
                .ordinary_cleanup_status(&session, &receipt.step_id, now + 2)
                .unwrap(),
            OrdinaryCleanupStatus::Uncertain
        );
        assert_eq!(store.retired_cleanups(&session, now + 2).unwrap().len(), 1);
        store
            .adopt_retired_cleanup(&session, &receipt.step_id, now + 2)
            .unwrap();
        let evidence: String = store
            .conn
            .query_row(
                "SELECT evidence FROM journal_entries WHERE state='cleanup_adopted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            evidence.contains("may already have been sent"),
            "{evidence}"
        );
        assert_eq!(
            store
                .ordinary_cleanup_status(&session, &receipt.step_id, now + 3)
                .unwrap(),
            OrdinaryCleanupStatus::Planned
        );
        let (_, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 3)
            .unwrap();
        assert!(context.is_some(), "one fresh, fenced arm");
        store
            .complete_cleanup(
                &session,
                &receipt.step_id,
                &gone(&execution, now + 4),
                now + 5,
                ttl,
            )
            .unwrap();
        assert!(!owns(&store, &fence.deployment_id));
        let leases: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM request_leases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(leases, 0);
    }

    /// ADR 0015 follow-up: an armed cleanup whose Terminate went unproven in
    /// this session is re-planned in the same session, re-armed once, and
    /// completes on gone evidence, without waiting for a restart. Nothing is
    /// released by the re-plan itself.
    // T32 T34 T10
    #[test]
    fn an_unproven_cleanup_is_retried_in_its_own_session() {
        let (store, session, fence, execution) = ready();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10_000;
        let receipt = store
            .accept_administrative_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-unproven",
                now,
                now + 50_000,
            )
            .unwrap();
        // Planned (never armed): nothing to re-plan.
        assert!(!store
            .replan_unproven_cleanup(&session, &receipt.step_id, now + 1)
            .unwrap());
        store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 2)
            .unwrap();
        // The send was lost: the step is armed and not discoverable.
        assert!(store.next_ordinary_cleanup(&session).unwrap().is_none());
        assert!(store
            .replan_unproven_cleanup(&session, &receipt.step_id, now + 3)
            .unwrap());
        assert!(owns(&store, &fence.deployment_id), "the re-plan releases nothing");
        assert_eq!(
            store
                .ordinary_cleanup_status(&session, &receipt.step_id, now + 4)
                .unwrap(),
            OrdinaryCleanupStatus::Planned
        );
        let next = store.next_ordinary_cleanup(&session).unwrap().unwrap();
        assert_eq!(next.step_id, receipt.step_id);
        let (_, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 5)
            .unwrap();
        assert!(context.is_some(), "one fresh, fenced arm");
        store
            .complete_cleanup(
                &session,
                &receipt.step_id,
                &gone(&execution, now + 6),
                now + 7,
                ttl,
            )
            .unwrap();
        assert!(!owns(&store, &fence.deployment_id));
        let retried: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM journal_entries WHERE state='cleanup_retried'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retried, 1);
    }

    /// Past its accepted deadline an unproven cleanup is not re-planned: it
    /// stays armed and charged.
    // T32
    #[test]
    fn an_unproven_cleanup_past_its_deadline_is_not_retried() {
        let (store, session, fence, execution) = ready();
        let now = execution.issued_at_ms + 10_000;
        let receipt = store
            .accept_administrative_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-late",
                now,
                now + 50_000,
            )
            .unwrap();
        store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        assert!(matches!(
            store.replan_unproven_cleanup(&session, &receipt.step_id, now + 50_000),
            Err(LifecycleError::Rejected(_))
        ));
        assert!(owns(&store, &fence.deployment_id));
        let later = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.replan_unproven_cleanup(&later, &receipt.step_id, now + 2),
            Err(LifecycleError::Conflict)
        ));
    }

    /// The Stop keeps its accepted deadline: past it, nothing is adopted and the
    /// launch stays charged with its retired session.
    // T33 T32
    #[test]
    fn a_stop_past_its_deadline_is_not_adopted() {
        let (store, old, fence, execution) = ready();
        let now = execution.issued_at_ms + 10_000;
        let receipt = store
            .accept_administrative_stop_command(
                &old,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-expired",
                now,
                now + 50_000,
            )
            .unwrap();
        let session = store.begin_coordinator_session().unwrap();
        assert!(store
            .retired_cleanups(&session, now + 50_000)
            .unwrap()
            .is_empty());
        assert!(matches!(
            store.adopt_retired_cleanup(&session, &receipt.step_id, now + 50_000),
            Err(LifecycleError::Rejected(_))
        ));
        assert!(owns(&store, &fence.deployment_id));
    }
}
