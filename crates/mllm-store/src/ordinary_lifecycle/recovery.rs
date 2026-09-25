//! Recovery of remote launches whose outcome the controller could not observe.
//!
//! SPEC §§6.1, 13.2: loss of connectivity or of a controller never releases a
//! reservation without evidence. This module holds the store half of the three
//! recovery paths a remote launch needs:
//!
//! - an operator Stop of a launch that went uncertain before any association was
//!   written (the cleanup path accepts it; see `cleanup.rs`);
//! - a controller restart, after which the new session adopts a retired
//!   session's remote launch so it can still be settled, stopped or re-probed;
//! - host session loss, after which a Ready remote deployment stops dispatching
//!   until a fresh authenticated model probe re-establishes readiness.
use super::*;
use mllm_domain::completion::ProcessIdentity;

/// The most retired launches one discovery pass returns.
const MAX_RETIRED: i64 = 64;
/// The most Ready remote launches one readiness pass returns.
const MAX_READY: i64 = 256;

/// A retired coordinator session's remote launch that the current session may
/// adopt. Discovery is observation only; `adopt_retired_remote_launch` is what
/// transfers it, after revalidating everything in its own transaction.
pub struct RetiredRemoteLaunch {
    pub work: worker::InitializeWork,
    /// True for a launch that reached Ready, false for one retained uncertain.
    pub completed: bool,
}

/// One Ready remote launch of this session, with the group its association
/// recorded. Readiness re-verification must name exactly this group again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteReadyLaunch {
    pub fence: DeploymentFence,
    pub operation_id: String,
    pub step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub host_id: String,
    pub profile_fingerprint: String,
    pub identities: Vec<ProcessIdentity>,
    pub dispatch_enabled: bool,
    /// W10 gap (a): a host-session closure is recorded for this incarnation.
    /// A gate a switch closed is not one, so supervision still records its
    /// own reason when the proving session is lost during a switch drain.
    pub host_closure_recorded: bool,
}

/// Authenticated evidence that the retained engine answered a fresh native model
/// probe. Built only from a host result that the transport validated against the
/// exact probe command, on a session the caller observed current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteReadinessEvidence {
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
}

fn remote_host(tx: &Transaction<'_>, binding_id: &str) -> Result<String, LifecycleError> {
    tx.query_row(
        "SELECT host_id FROM remote_binding_ingress WHERE binding_id=?1",
        [binding_id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(LifecycleError::Conflict)
}

fn journal(
    tx: &Transaction<'_>,
    host: &str,
    operation: &str,
    state: &str,
    evidence: &str,
) -> Result<(), LifecycleError> {
    tx.execute(
        "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,?4,?5)",
        params![ulid::Ulid::new().to_string(), host, operation, state, evidence],
    )?;
    Ok(())
}

/// Validate one retired session's remote launch exactly as that session left it.
///
/// SPEC §13.2: adoption is safe only because the process lock admits one live
/// coordinator and the retired session can no longer act; everything else about
/// the launch must still be precisely what it recorded. A deployment with any
/// other unfinished work is not adopted: that work is its own reconciliation.
fn retired(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
) -> Result<(Plan, EffectiveDeployment, bool, Option<Association>), LifecycleError> {
    let (p, e, state) = load(tx, id)?;
    let completed = match state.as_str() {
        "completed" => true,
        "uncertain" => false,
        _ => return Err(LifecycleError::Conflict),
    };
    if p.session_id == s.id() {
        return Err(LifecycleError::Conflict);
    }
    remote_host(tx, &p.binding_id)?;
    // SPEC §10: request leases a crash left behind stay charged through adoption;
    // they are this launch's retired requests, settled only on quiescence
    // evidence (`retired_leases`). Any other lease is its own reconciliation.
    let exact: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3 AND kind='model' AND desired_state='ready' AND suspended=0 AND (?4=0 OR observed_state IN ('ready','parked')))
         AND NOT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.deployment_id=?1 AND b.instance_index=?8 AND s.id!=?5 AND s.state IN ('planned','armed','uncertain') AND NOT EXISTS(SELECT 1 FROM operations po WHERE po.id=s.operation_id AND po.kind IN ('park','restore')))
         AND NOT EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND instance_index=?8 AND operation_id!=?6 AND state NOT IN ('succeeded','failed') AND operation_id NOT IN (SELECT id FROM operations WHERE kind IN ('park','restore')))
         AND (?4=0 OR NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?8 AND (session_id=?7 OR revision!=?2 OR generation!=?3)))
         AND (?4=1 OR (SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?6 AND deployment_id=?1 AND revision=?2 AND generation=?3)=1)",
        params![p.deployment_id, p.revision, p.generation, completed, p.step_id, p.operation_id, s.id(), p.instance_index],
        |r| r.get(0),
    )?;
    if !exact {
        return Err(LifecycleError::Conflict);
    }
    let association = association(tx, &p)?;
    match (&association, completed) {
        (None, true) => return Err(LifecycleError::CorruptStoredData),
        (None, false) => {
            super::failed_launch::recorded_identities(tx, &p.binding_id)?;
        }
        _ => {}
    }
    Ok((p, e, completed, association))
}

/// The Ready remote launch `id` of this session, validated for readiness use.
fn ready(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
) -> Result<(Plan, EffectiveDeployment, Association, String), LifecycleError> {
    let (p, e, state) = load(tx, id)?;
    if state != "completed" {
        return Err(LifecycleError::Conflict);
    }
    current_admitted(tx, s, &p, true, false)?;
    let host = remote_host(tx, &p.binding_id)?;
    let association = association(tx, &p)?.ok_or(LifecycleError::CorruptStoredData)?;
    Ok((p, e, association, host))
}

impl crate::Store {
    /// Retired sessions' remote launches this session may adopt, oldest first.
    /// Observation only: nothing is transferred and nothing may be sent on it.
    pub fn retired_remote_launches(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Vec<RetiredRemoteLaunch>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let ids = tx
            .prepare(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                 JOIN runtime_bindings b ON b.id=s.binding_id AND b.state!='released'
                 JOIN remote_binding_ingress r ON r.binding_id=b.id
                 WHERE o.kind='initialize' AND s.state IN ('uncertain','completed') AND s.session_id!=?1
                 ORDER BY o.accepted_at,o.id LIMIT ?2",
            )?
            .query_map(params![s.id(), MAX_RETIRED], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut launches = Vec::new();
        for id in ids {
            // A launch that no longer validates stays with its retired session:
            // it is never released, it is only not adopted.
            let Ok((p, e, completed, _)) = retired(&tx, s, &id) else {
                continue;
            };
            let Ok(work) = worker::prepare_work(&tx, p, e) else {
                continue;
            };
            launches.push(RetiredRemoteLaunch { work, completed });
        }
        Ok(launches)
    }

    /// Transfer one retired session's remote launch to this session.
    ///
    /// SPEC §13.2: on server restart, reconcile before dispatch. Adoption changes
    /// which live session may act on the launch and nothing else: an uncertain
    /// launch stays uncertain with its reservation, claim and binding retained,
    /// and a Ready one keeps dispatch closed until a fresh model probe reopens
    /// it. Everything is revalidated as this session's own work before commit.
    pub fn adopt_retired_remote_launch(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (mut p, _, completed, association) = retired(&tx, s, id)?;
        let retired_session = std::mem::replace(&mut p.session_id, s.id().into());
        one(tx.execute(
            "UPDATE lifecycle_steps SET session_id=?2,step_json=?3 WHERE id=?1 AND session_id=?4",
            params![id, s.id(), encode(&p)?, retired_session],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET session_id=?2 WHERE operation_id=?1 AND session_id=?3",
            params![p.operation_id, s.id(), retired_session],
        )?)?;
        let associated = association.is_some();
        if let Some(mut association) = association {
            association.session_id = s.id().into();
            one(tx.execute(
                "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
                params![id, encode(&association)?],
            )?)?;
        }
        // W5: its park or restore work moves with it (never re-armed).
        super::park::adopt_residency(&tx, s, &p.deployment_id, p.instance_index)?;
        let (adopted, _, _) = load(&tx, id)?;
        current_admitted(&tx, s, &adopted, completed, false)?;
        if super::association(&tx, &adopted)?.is_some() != associated {
            return Err(LifecycleError::CorruptStoredData);
        }
        let host = remote_host(&tx, &p.binding_id)?;
        let leases = super::retired_leases::count(&tx, s, &p)?;
        journal(
            &tx,
            &host,
            &p.operation_id,
            "launch_adopted",
            &format!(
                "deployment {}: a restarted controller adopted its {} remote launch; \
                 {} before anything is released or dispatched{}",
                p.deployment_id,
                if completed { "ready" } else { "uncertain" },
                if completed {
                    "a fresh authenticated model probe must pass"
                } else {
                    "authenticated host evidence must settle it"
                },
                super::retired_leases::retained_note(leases),
            ),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// This session's Ready remote launches, for host readiness supervision.
    /// A launch that no longer validates is omitted, never assumed healthy.
    pub fn remote_ready_launches(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Vec<RemoteReadyLaunch>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let ids = tx
            .prepare(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                 JOIN runtime_bindings b ON b.id=s.binding_id AND b.state='live'
                 JOIN remote_binding_ingress r ON r.binding_id=b.id
                 JOIN instance_runtime d ON d.id=s.deployment_id AND d.instance_index=b.instance_index
                 WHERE o.kind='initialize' AND s.state='completed' AND s.session_id=?1
                   AND d.desired_state='ready' AND d.observed_state='ready' AND d.suspended=0
                   AND NOT EXISTS(SELECT 1 FROM lifecycle_runs pr JOIN operations po ON po.id=pr.operation_id
                        WHERE pr.deployment_id=s.deployment_id AND pr.instance_index=b.instance_index
                          AND po.kind IN ('park','restore') AND pr.state IN ('queued','running','uncertain'))
                 ORDER BY o.accepted_at,o.id LIMIT ?2",
            )?
            .query_map(params![s.id(), MAX_READY], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut launches = Vec::new();
        for id in ids {
            let Ok((p, e, association, host_id)) = ready(&tx, s, &id) else {
                continue;
            };
            let dispatch_enabled: bool = tx.query_row(
                "SELECT dispatch_enabled FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                params![p.deployment_id, p.instance_index],
                |r| r.get(0),
            )?;
            let host_closure_recorded = crate::switch_state::has_closure(
                &tx,
                &p.deployment_id,
                p.instance_index,
                p.generation,
                crate::switch_state::ClosureReason::HostSession,
            )?;
            launches.push(RemoteReadyLaunch {
                host_closure_recorded,
                fence: p.fence(),
                operation_id: p.operation_id.clone(),
                step_id: p.step_id.clone(),
                binding_id: p.binding_id.clone(),
                incarnation: p.incarnation.clone(),
                host_id,
                profile_fingerprint: e.profile.build_fingerprint.clone(),
                identities: members(&association.identities)?,
                dispatch_enabled,
            });
        }
        Ok(launches)
    }

    /// Close dispatch for a Ready remote launch whose readiness is no longer
    /// backed by the host session that proved it. Returns whether it changed.
    ///
    /// SPEC §13.2: control-channel loss preserves ownership and freezes
    /// transitions; nothing is released, only admission to the engine closes.
    pub fn suspend_remote_dispatch(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, _, _, host) = ready(&tx, s, id)?;
        let changed = tx.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND dispatch_enabled=1",
            params![p.deployment_id, p.revision, p.generation],
        )? == 1;
        // W10 gap (a): the reason is recorded even when a switch already
        // closed the gate, so that switch failing cannot reopen it.
        crate::switch_state::record_closure(
            &tx,
            &p.deployment_id,
            p.instance_index,
            p.generation,
            crate::switch_state::ClosureReason::HostSession,
        )?;
        if changed {
            journal(
                &tx,
                &host,
                &p.operation_id,
                "dispatch_suspended",
                &format!(
                    "deployment {}: the host session that proved readiness is gone; \
                     dispatch is closed until a fresh model probe passes",
                    p.deployment_id
                ),
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Reopen dispatch for a Ready remote launch on fresh probe evidence.
    ///
    /// SPEC §6.1: liveness of an HTTP server is not model readiness. The evidence
    /// must be fresh within the host's observation ttl and name exactly the
    /// associated group; a deployment that is stopping or closed is never reopened.
    pub fn reverify_remote_dispatch(
        &self,
        s: &CoordinatorSession,
        id: &str,
        evidence: &RemoteReadinessEvidence,
        now: i64,
    ) -> Result<(), LifecycleError> {
        crate::lifecycle::completion::nonempty_receipt(&evidence.receipt)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, e, association, host) = ready(&tx, s, id)?;
        // SPEC §§4.1, 13.3: a revoked host takes no new work, whatever evidence
        // arrives for it; its dispatch stays closed until an operator acts.
        let revoked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1 AND revoked=1)",
            [&host],
            |r| r.get(0),
        )?;
        if revoked {
            return Err(LifecycleError::Rejected("host revoked".into()));
        }
        current_admitted(&tx, s, &p, true, true)?;
        if evidence.binding_id != p.binding_id
            || evidence.incarnation != p.incarnation
            || canonical_members(&evidence.identities).ok()
                != Some(members(&association.identities)?)
        {
            return Err(LifecycleError::Conflict);
        }
        let ttl = policy(&tx, &e)?.controls.observation_ttl_ms;
        if ttl <= 0
            || evidence.observed_at_ms > now
            || now
                .checked_sub(evidence.observed_at_ms)
                .is_none_or(|age| age > ttl)
        {
            return Err(LifecycleError::Rejected("evidence freshness".into()));
        }
        // SPEC §10: dispatch stays closed while a crashed session's requests may
        // still be running on the engine.
        super::retired_leases::require_settled(&tx, s, &p)?;
        // W10 gap (a): this evidence clears the host-session closure only; a
        // switch drain or an engine exit keeps the gate closed.
        crate::switch_state::clear_closure(
            &tx,
            &p.deployment_id,
            p.instance_index,
            p.generation,
            crate::switch_state::ClosureReason::HostSession,
        )?;
        let changed = tx.execute(
            &format!(
                "UPDATE deployment_instances AS i SET dispatch_enabled=1 WHERE i.deployment_id=?1 AND i.revision=?2 AND i.generation=?3 AND i.desired_state='ready' AND i.observed_state='ready' AND i.admission_enabled=1 AND i.dispatch_enabled=0 AND NOT EXISTS(SELECT 1 FROM deployments d WHERE d.id=?1 AND d.suspended=1) AND {}",
                crate::switch_state::no_closure_clause("i")
            ),
            params![p.deployment_id, p.revision, p.generation],
        )?;
        if changed == 1 {
            journal(
                &tx,
                &host,
                &p.operation_id,
                "readiness_reverified",
                &format!(
                    "deployment {}: {} at {}; dispatch reopened",
                    p.deployment_id, evidence.receipt, evidence.observed_at_ms
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{armed_ordinary, identity};
    use super::super::*;
    use super::RemoteReadinessEvidence;
    use crate::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus;
    use mllm_domain::completion::{CleanupEvidence, ProcessIdentity};

    fn text(store: &crate::Store, sql: &str, id: &str) -> String {
        store.conn.query_row(sql, [id], |r| r.get(0)).unwrap()
    }

    /// The live U5 failure: a remote launch went uncertain before any association
    /// was written, and an operator Stop was refused as a lifecycle conflict. The
    /// Stop must be accepted and armed with exactly the identities the binding
    /// recorded, and it releases nothing until those are proven gone.
    // T32 T34 T10
    #[test]
    fn stop_of_an_uncertain_unassociated_launch_is_accepted_and_released_on_gone_evidence() {
        let (store, session, fence, execution) = armed_ordinary();
        let api = identity("api", 41);
        store
            .record_api_identity(&session, &fence, &execution.binding_id, &api)
            .unwrap();
        let step = execution.token.step_id.clone();
        let now = execution.issued_at_ms + 10;
        assert!(store
            .mark_initialize_uncertain(&session, &step, now)
            .unwrap());

        let receipt = store
            .accept_administrative_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-uncertain",
                now,
                now + 50_000,
            )
            .expect("an operator Stop of an uncertain unassociated launch is accepted");
        let (arm, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        assert!(matches!(arm, crate::lifecycle::ArmResult::New { .. }));
        let context = context.unwrap();
        // The cleanup must prove exactly what the binding recorded gone.
        assert_eq!(context.identities, vec![api.clone()]);
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let gone = |identities: Vec<ProcessIdentity>| CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities,
            observed_at_ms: now + 2,
            receipt: "authenticated host observed the owned group gone".into(),
        };
        // Evidence that omits a recorded process proves nothing.
        assert!(matches!(
            store.complete_cleanup(&session, &receipt.step_id, &gone(vec![]), now + 3, ttl),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(
            text(
                &store,
                "SELECT state FROM runtime_bindings WHERE id=?1",
                &execution.binding_id
            ),
            "uncertain"
        );
        store
            .complete_cleanup(&session, &receipt.step_id, &gone(vec![api]), now + 3, ttl)
            .unwrap();
        assert_eq!(
            store
                .ordinary_cleanup_status(&session, &receipt.step_id, now + 4)
                .unwrap(),
            OrdinaryCleanupStatus::Completed
        );
        assert_eq!(
            text(
                &store,
                "SELECT state FROM runtime_bindings WHERE id=?1",
                &execution.binding_id
            ),
            "released"
        );
        assert_eq!(
            text(
                &store,
                "SELECT observed_state FROM deployments WHERE id=?1",
                &fence.deployment_id
            ),
            "stopped"
        );
        assert_eq!(
            text(
                &store,
                "SELECT state FROM lifecycle_steps WHERE id=?1",
                &step
            ),
            "cancelled"
        );
        assert!(!store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
    }

    /// A launch whose gate never opened recorded no identity at all. Its Stop is
    /// accepted with an empty identity set, and only the host's authenticated
    /// report that nothing was ever released can satisfy it.
    // T32 T34
    #[test]
    fn stop_of_an_uncertain_launch_that_recorded_nothing_needs_empty_evidence() {
        let (store, session, fence, execution) = armed_ordinary();
        // Only an authenticated host can report a launch it never released.
        make_remote(&store, &execution.binding_id);
        let step = execution.token.step_id.clone();
        let now = execution.issued_at_ms + 10;
        store
            .mark_initialize_uncertain(&session, &step, now)
            .unwrap();
        let receipt = store
            .accept_ordinary_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-empty",
                now,
                now + 50_000,
            )
            .unwrap();
        let (_, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        assert!(context.unwrap().identities.is_empty());
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let evidence = |identities| CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities,
            observed_at_ms: now + 2,
            receipt: "authenticated host reports the launch was never released".into(),
        };
        assert!(store
            .complete_cleanup(
                &session,
                &receipt.step_id,
                &evidence(vec![identity("api", 9)]),
                now + 3,
                ttl
            )
            .is_err());
        store
            .complete_cleanup(&session, &receipt.step_id, &evidence(vec![]), now + 3, ttl)
            .unwrap();
        // Replay of the exact evidence is idempotent.
        store
            .complete_cleanup(&session, &receipt.step_id, &evidence(vec![]), now + 4, ttl)
            .unwrap();
    }

    /// SPEC §6.1, §13.2, AGENTS.md (uncertainty retains accounting): an
    /// embedded launch's arm consumed its one spawn attempt, and nothing was
    /// recorded of what it started. With no authenticated host to report that
    /// the launch was never released, gone evidence for an empty set proves
    /// nothing, so the Stop is accepted but releases nothing.
    // T32 T34
    #[test]
    fn an_embedded_stop_never_releases_on_empty_gone_evidence() {
        let (store, session, fence, execution) = armed_ordinary();
        let step = execution.token.step_id.clone();
        let now = execution.issued_at_ms + 10;
        store
            .mark_initialize_uncertain(&session, &step, now)
            .unwrap();
        let receipt = store
            .accept_ordinary_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-empty",
                now,
                now + 50_000,
            )
            .unwrap();
        store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let empty = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![],
            observed_at_ms: now + 2,
            receipt: "nothing recorded, nothing observed".into(),
        };
        assert!(matches!(
            store.complete_cleanup(&session, &receipt.step_id, &empty, now + 3, ttl),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(
            text(
                &store,
                "SELECT state FROM runtime_bindings WHERE id=?1",
                &execution.binding_id
            ),
            "uncertain"
        );
        assert!(store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
    }

    /// G3 (U5 live): status read `observed_state: "stopped"` while an uncertain
    /// engine was still running and its reservation was retained. A deployment
    /// holding an unresolved launch is reported uncertain, never stopped, and
    /// reads stopped again only once gone evidence released it.
    // T08 T32
    #[test]
    fn status_reports_an_unresolved_launch_as_uncertain_not_stopped() {
        let (store, session, fence, execution) = armed_ordinary();
        // The empty report below is an authenticated host's.
        make_remote(&store, &execution.binding_id);
        let step = execution.token.step_id.clone();
        let now = execution.issued_at_ms + 10;
        let observed = |store: &crate::Store| {
            store
                .snapshot()
                .unwrap()
                .deployments
                .into_iter()
                .find(|d| d.id == fence.deployment_id)
                .unwrap()
                .observed_state
        };
        store
            .mark_initialize_uncertain(&session, &step, now)
            .unwrap();
        assert_eq!(observed(&store), "uncertain");
        let receipt = store
            .accept_ordinary_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-status",
                now,
                now + 50_000,
            )
            .unwrap();
        // Accepting a Stop resolves nothing: the engine may still be running.
        assert_eq!(observed(&store), "uncertain");
        store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        store
            .complete_cleanup(
                &session,
                &receipt.step_id,
                &CleanupEvidence {
                    binding_id: execution.binding_id.clone(),
                    incarnation: execution.incarnation.clone(),
                    identities: vec![],
                    observed_at_ms: now + 2,
                    receipt: "authenticated host reports nothing released".into(),
                },
                now + 3,
                ttl,
            )
            .unwrap();
        assert_eq!(observed(&store), "stopped");
    }

    /// Marks the fixture's binding as served through a remote host's ingress,
    /// the way the remote execution binding records it before any send.
    fn make_remote(store: &crate::Store, binding: &str) {
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO enrolled_hosts(host_id,host_name,key_digest) VALUES('lab','lab','digest')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO remote_binding_ingress VALUES(?1,'lab','http://100.64.0.1:9443')",
                [binding],
            )
            .unwrap();
    }

    fn group() -> Vec<ProcessIdentity> {
        vec![identity("api", 51), identity("worker-0", 52)]
    }

    /// A remote launch that reached Ready in the fixture's session.
    fn ready_remote() -> (
        crate::Store,
        CoordinatorSession,
        DeploymentFence,
        StepExecutionContext,
    ) {
        let (store, session, fence, execution) = armed_ordinary();
        make_remote(&store, &execution.binding_id);
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
                    receipt: "authenticated host native model probe".into(),
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
                    control_receipt: Some("authenticated host native model probe".into()),
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

    fn dispatch(store: &crate::Store, deployment: &str) -> bool {
        store
            .conn
            .query_row(
                "SELECT dispatch_enabled FROM deployments WHERE id=?1",
                [deployment],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// G1 durability: a controller restart retires the session that owned an
    /// uncertain remote launch, which left it unstoppable and its reservation
    /// stranded. The new session adopts it, and only it: the adopted launch is
    /// still uncertain, still holds its reservation, and a Stop now reaches it.
    // T33 T34 T32
    #[test]
    fn a_restarted_controller_adopts_an_uncertain_remote_launch_for_settlement() {
        let (store, old, fence, execution) = armed_ordinary();
        make_remote(&store, &execution.binding_id);
        let step = execution.token.step_id.clone();
        let now = execution.issued_at_ms + 10;
        let session = store.begin_coordinator_session().unwrap();
        assert_ne!(old.id(), session.id());
        // Before adoption the retired session's launch is nobody's to stop.
        assert!(matches!(
            store.accept_ordinary_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-before",
                now,
                now + 50_000,
            ),
            Err(LifecycleError::Stale)
        ));
        let retired = store.retired_remote_launches(&session).unwrap();
        assert_eq!(retired.len(), 1);
        assert!(!retired[0].completed);
        assert_eq!(retired[0].work.step_id(), step);
        store.adopt_retired_remote_launch(&session, &step).unwrap();
        // Adoption resolves nothing and releases nothing.
        assert_eq!(
            store.initialize_status(&session, &step, now).unwrap(),
            worker::InitializeStatus::Uncertain
        );
        assert!(store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
        assert!(store.retired_remote_launches(&session).unwrap().is_empty());
        // Adoption is not repeatable into the same session.
        assert!(store.adopt_retired_remote_launch(&session, &step).is_err());
        // A failed-launch release now works against the host's gone evidence.
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        store
            .release_failed_launch(
                &session,
                &step,
                &CleanupEvidence {
                    binding_id: execution.binding_id.clone(),
                    incarnation: execution.incarnation.clone(),
                    identities: vec![],
                    observed_at_ms: now + 1,
                    receipt: "authenticated host reports the launch was never released".into(),
                },
                now + 2,
                ttl,
            )
            .unwrap();
        assert!(!store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
    }

    /// Only remote launches are adopted: an embedded launch's processes are
    /// this controller's own, and nothing here proves anything about them.
    // T33
    #[test]
    fn a_restarted_controller_does_not_adopt_an_embedded_launch() {
        let (store, _old, _fence, _execution) = armed_ordinary();
        let session = store.begin_coordinator_session().unwrap();
        assert!(store.retired_remote_launches(&session).unwrap().is_empty());
    }

    /// G2 (host restart while Ready): dispatch closes when the host session that
    /// proved readiness is gone, and reopens only on fresh authenticated evidence
    /// naming exactly the associated group. Stale or mismatched evidence reopens
    /// nothing. The same holds after a controller restart adopts the launch.
    // T32 T33 T38
    #[test]
    fn remote_dispatch_reopens_only_on_fresh_matching_readiness_evidence() {
        let (store, old, fence, execution) = ready_remote();
        let step = execution.token.step_id.clone();
        assert!(dispatch(&store, &fence.deployment_id));
        let listed = store.remote_ready_launches(&old).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].step_id, step);
        assert_eq!(listed[0].host_id, "lab");
        assert_eq!(listed[0].identities, group());
        assert!(store.suspend_remote_dispatch(&old, &step).unwrap());
        assert!(!dispatch(&store, &fence.deployment_id));
        assert!(!store.suspend_remote_dispatch(&old, &step).unwrap());

        // A controller restart closes every deployment's dispatch; the Ready
        // remote launch belongs to a retired session until it is adopted.
        let session = store.begin_coordinator_session().unwrap();
        assert!(store.remote_ready_launches(&session).unwrap().is_empty());
        let retired = store.retired_remote_launches(&session).unwrap();
        assert_eq!(retired.len(), 1);
        assert!(retired[0].completed);
        store.adopt_retired_remote_launch(&session, &step).unwrap();
        let listed = store.remote_ready_launches(&session).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].dispatch_enabled);

        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10_000;
        let evidence =
            |identities: Vec<ProcessIdentity>, observed_at_ms: i64| RemoteReadinessEvidence {
                binding_id: execution.binding_id.clone(),
                incarnation: execution.incarnation.clone(),
                identities,
                observed_at_ms,
                receipt: "authenticated host fresh native model probe".into(),
            };
        // A different group is not the launch that was associated.
        assert!(matches!(
            store.reverify_remote_dispatch(
                &session,
                &step,
                &evidence(vec![identity("api", 51), identity("worker-0", 99)], now),
                now,
            ),
            Err(LifecycleError::Conflict)
        ));
        // A probe older than the host's observation ttl is not fresh.
        assert!(store
            .reverify_remote_dispatch(&session, &step, &evidence(group(), now - ttl - 1), now)
            .is_err());
        assert!(!dispatch(&store, &fence.deployment_id));
        store
            .reverify_remote_dispatch(&session, &step, &evidence(group(), now - 1), now)
            .unwrap();
        assert!(dispatch(&store, &fence.deployment_id));
        // The adopted Ready deployment can still be stopped by the new session.
        // SPEC §4.3 (ADR 0013 §5): the stop a host drain issues names the
        // instance on the drained host.
        store
            .accept_instance_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                0,
                fence.revision,
                "stop-after",
                now,
                now + 50_000,
            )
            .unwrap()
            .unwrap();
        assert!(!dispatch(&store, &fence.deployment_id));
        // A stopping deployment is never reopened by a late probe.
        assert!(store
            .reverify_remote_dispatch(&session, &step, &evidence(group(), now), now)
            .is_err());
    }

    /// W12 (remote): a controller crash with requests in flight leaves leases
    /// that used to refuse adoption forever. The Ready remote launch is adopted
    /// with them charged; a fresh probe alone does not reopen dispatch, and the
    /// leases close only on quiescence observed after this session's probe.
    // T33 T38 T32
    #[test]
    fn a_ready_remote_launch_with_crashed_leases_is_adopted_and_settled_on_quiescence() {
        use super::super::retired_leases::QuiescenceEvidence;
        let (store, old, fence, execution) = ready_remote();
        let step = execution.token.step_id.clone();
        store
            .conn
            .execute(
                "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('lease-r',?1,?2,?3,?4,'inflight')",
                rusqlite::params![fence.deployment_id, fence.revision, fence.generation, old.id()],
            )
            .unwrap();
        let session = store.begin_coordinator_session().unwrap();
        let retired = store.retired_remote_launches(&session).unwrap();
        assert_eq!(retired.len(), 1);
        store.adopt_retired_remote_launch(&session, &step).unwrap();
        assert_eq!(store.retired_request_leases(&session, &step).unwrap(), 1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            + 5;
        let ready = RemoteReadinessEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: group(),
            observed_at_ms: now - 2,
            receipt: "authenticated host fresh native model probe".into(),
        };
        assert!(matches!(
            store.reverify_remote_dispatch(&session, &step, &ready, now),
            Err(LifecycleError::Rejected(_))
        ));
        assert!(!dispatch(&store, &fence.deployment_id));
        let quiet = QuiescenceEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: group(),
            readiness_observed_at_ms: now - 2,
            quiescent_at_ms: now - 1,
            receipt: "host load sample: engine running+waiting 0, ingress in flight 0".into(),
        };
        assert_eq!(
            store
                .abandon_retired_request_leases(&session, &step, &quiet, now)
                .unwrap(),
            1
        );
        let host: String = store
            .conn
            .query_row(
                "SELECT host_id FROM journal_entries WHERE state='request_leases_abandoned'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(host, "lab");
        store
            .reverify_remote_dispatch(&session, &step, &ready, now)
            .unwrap();
        assert!(dispatch(&store, &fence.deployment_id));
    }

    /// An armed launch is still in the hands of its effect. Only the uncertain
    /// state, which the worker records once that effect has exited, lets a Stop
    /// take over an unassociated launch.
    // T34
    #[test]
    fn stop_of_an_armed_unassociated_launch_is_still_refused() {
        let (store, session, fence, execution) = armed_ordinary();
        let now = execution.issued_at_ms + 10;
        assert!(matches!(
            store.accept_ordinary_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-armed",
                now,
                now + 50_000,
            ),
            Err(LifecycleError::Conflict)
        ));
    }
}
