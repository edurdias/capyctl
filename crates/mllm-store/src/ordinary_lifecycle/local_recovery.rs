//! Re-attachment of embedded (standalone) engines across a role restart.
//!
//! SPEC §4.3 (owner decision P3, 2026-09-22): stopping or signalling the
//! standalone role is a service restart. Its engines keep running, owned, and the
//! next start re-attaches them instead of treating them as orphans. SPEC §13.2:
//! reconcile before dispatch. This module is the store half of that, mirroring
//! the remote path in `recovery.rs` for launches that have no remote host:
//!
//! - a restarted coordinator adopts the Ready embedded launches its retired
//!   session left, so a later Stop can still terminate exactly what was recorded;
//! - dispatch stays closed (session start closes it for every deployment) until
//!   fresh local evidence — every recorded process alive with the same start
//!   ticks and boot, and an authenticated model probe — reopens it.
//!
//! Only launches that reached Ready are adopted. An embedded launch the retired
//! session left uncertain is not transferred here: its outcome needs the
//! failed-launch settlement, which stays with the operator Stop path.
use super::recovery::RemoteReadinessEvidence;
use super::*;
use mllm_domain::completion::ProcessIdentity;

/// The most retired embedded launches one discovery pass returns.
const MAX_RETIRED: i64 = 64;
/// The most Ready embedded launches one readiness pass returns.
const MAX_READY: i64 = 256;

/// A retired coordinator session's Ready embedded launch that the current
/// session may adopt. Discovery is observation only.
pub struct RetiredLocalLaunch {
    pub work: worker::InitializeWork,
}

/// One Ready embedded launch of this session, with the group its association
/// recorded. Readiness re-verification must name exactly this group again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalReadyLaunch {
    pub fence: DeploymentFence,
    pub operation_id: String,
    pub step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub dispatch_enabled: bool,
}

/// Embedded launches have no remote ingress row; a remote one is never handled
/// here, because its evidence must come from its authenticated host.
fn embedded(tx: &Transaction<'_>, binding_id: &str) -> Result<(), LifecycleError> {
    let remote: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_binding_ingress WHERE binding_id=?1)",
        [binding_id],
        |r| r.get(0),
    )?;
    if remote {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}

fn journal(
    tx: &Transaction<'_>,
    operation: &str,
    state: &str,
    evidence: &str,
) -> Result<(), LifecycleError> {
    tx.execute(
        "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,NULL,?2,?3,?4)",
        params![ulid::Ulid::new().to_string(), operation, state, evidence],
    )?;
    Ok(())
}

/// Validate one retired session's Ready embedded launch exactly as it was left.
///
/// SPEC §13.2: adoption is safe only because the process lock admits one live
/// coordinator and the retired session can no longer act. The deployment must
/// still be Ready with a live binding and no other unfinished work. Request
/// leases the retired session left on this exact fence are adopted with it and
/// stay charged (SPEC §10) until `retired_leases` settles them on quiescence
/// evidence; any other lease is its own reconciliation.
fn retired(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
) -> Result<(Plan, EffectiveDeployment, Association), LifecycleError> {
    let (p, e, state) = load(tx, id)?;
    if state != "completed" || p.session_id == s.id() {
        return Err(LifecycleError::Conflict);
    }
    embedded(tx, &p.binding_id)?;
    let exact: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3 AND kind='model' AND desired_state='ready' AND suspended=0 AND observed_state IN ('ready','parked'))
         AND EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?4 AND deployment_id=?1 AND state='live')
         AND NOT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.deployment_id=?1 AND b.instance_index=?8 AND s.id!=?5 AND s.state IN ('planned','armed','uncertain') AND NOT EXISTS(SELECT 1 FROM operations po WHERE po.id=s.operation_id AND po.kind IN ('park','restore')))
         AND NOT EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND instance_index=?8 AND operation_id!=?6 AND state NOT IN ('succeeded','failed') AND operation_id NOT IN (SELECT id FROM operations WHERE kind IN ('park','restore')))
         AND NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?8 AND (session_id=?7 OR revision!=?2 OR generation!=?3))",
        params![p.deployment_id, p.revision, p.generation, p.binding_id, p.step_id, p.operation_id, s.id(), p.instance_index],
        |r| r.get(0),
    )?;
    if !exact {
        return Err(LifecycleError::Conflict);
    }
    let association = association(tx, &p)?.ok_or(LifecycleError::CorruptStoredData)?;
    Ok((p, e, association))
}

/// The Ready embedded launch `id` of this session, validated for readiness use.
fn ready(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
) -> Result<(Plan, EffectiveDeployment, Association), LifecycleError> {
    let (p, e, state) = load(tx, id)?;
    if state != "completed" {
        return Err(LifecycleError::Conflict);
    }
    current_admitted(tx, s, &p, true, false)?;
    embedded(tx, &p.binding_id)?;
    let association = association(tx, &p)?.ok_or(LifecycleError::CorruptStoredData)?;
    Ok((p, e, association))
}

impl crate::Store {
    /// Retired sessions' Ready embedded launches this session may adopt, oldest
    /// first. Observation only: nothing is transferred and nothing may be sent.
    pub fn retired_local_launches(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Vec<RetiredLocalLaunch>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let ids = tx
            .prepare(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                 JOIN runtime_bindings b ON b.id=s.binding_id AND b.state='live'
                 WHERE o.kind='initialize' AND s.state='completed' AND s.session_id!=?1
                   AND NOT EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id)
                 ORDER BY o.accepted_at,o.id LIMIT ?2",
            )?
            .query_map(params![s.id(), MAX_RETIRED], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut launches = Vec::new();
        for id in ids {
            // A launch that no longer validates stays with its retired session:
            // it is never released, it is only not adopted.
            let Ok((p, e, _)) = retired(&tx, s, &id) else {
                continue;
            };
            let Ok(work) = worker::prepare_work(&tx, p, e) else {
                continue;
            };
            launches.push(RetiredLocalLaunch { work });
        }
        Ok(launches)
    }

    /// Transfer one retired session's Ready embedded launch to this session.
    ///
    /// SPEC §13.2: adoption changes which live session may act on the launch
    /// and nothing else. Its reservation, binding and recorded identities are
    /// kept exactly, and dispatch stays closed until fresh local evidence
    /// reopens it.
    pub fn adopt_retired_local_launch(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (mut p, _, mut association) = retired(&tx, s, id)?;
        let retired_session = std::mem::replace(&mut p.session_id, s.id().into());
        one(tx.execute(
            "UPDATE lifecycle_steps SET session_id=?2,step_json=?3 WHERE id=?1 AND session_id=?4",
            params![id, s.id(), encode(&p)?, retired_session],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET session_id=?2 WHERE operation_id=?1 AND session_id=?3",
            params![p.operation_id, s.id(), retired_session],
        )?)?;
        association.session_id = s.id().into();
        one(tx.execute(
            "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
            params![id, encode(&association)?],
        )?)?;
        // W5: its park or restore work moves with it (never re-armed).
        super::park::adopt_residency(&tx, s, &p.deployment_id, p.instance_index)?;
        let (adopted, _, _) = load(&tx, id)?;
        current_admitted(&tx, s, &adopted, true, false)?;
        super::association(&tx, &adopted)?.ok_or(LifecycleError::CorruptStoredData)?;
        let leases = super::retired_leases::count(&tx, s, &p)?;
        journal(
            &tx,
            &p.operation_id,
            "launch_adopted",
            &format!(
                "deployment {}: a restarted standalone role adopted its ready embedded \
                 launch; dispatch stays closed until its recorded processes and a fresh \
                 authenticated model probe are verified{}",
                p.deployment_id,
                super::retired_leases::retained_note(leases),
            ),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// This session's Ready embedded launches, for local readiness supervision.
    /// A launch that no longer validates is omitted, never assumed healthy.
    pub fn local_ready_launches(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Vec<LocalReadyLaunch>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let ids = tx
            .prepare(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                 JOIN runtime_bindings b ON b.id=s.binding_id AND b.state='live'
                 JOIN instance_runtime d ON d.id=s.deployment_id AND d.instance_index=b.instance_index
                 WHERE o.kind='initialize' AND s.state='completed' AND s.session_id=?1
                   AND d.desired_state='ready' AND d.observed_state='ready' AND d.suspended=0
                   AND NOT EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id)
                   AND NOT EXISTS(SELECT 1 FROM lifecycle_runs pr JOIN operations po ON po.id=pr.operation_id
                        WHERE pr.deployment_id=s.deployment_id AND pr.instance_index=b.instance_index
                          AND po.kind IN ('park','restore') AND pr.state IN ('queued','running','uncertain'))
                 ORDER BY o.accepted_at,o.id LIMIT ?2",
            )?
            .query_map(params![s.id(), MAX_READY], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut launches = Vec::new();
        for id in ids {
            let Ok((p, _, association)) = ready(&tx, s, &id) else {
                continue;
            };
            let dispatch_enabled: bool = tx.query_row(
                "SELECT dispatch_enabled FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                params![p.deployment_id, p.instance_index],
                |r| r.get(0),
            )?;
            launches.push(LocalReadyLaunch {
                fence: p.fence(),
                operation_id: p.operation_id.clone(),
                step_id: p.step_id.clone(),
                binding_id: p.binding_id.clone(),
                incarnation: p.incarnation.clone(),
                identities: members(&association.identities)?,
                dispatch_enabled,
            });
        }
        Ok(launches)
    }

    /// Reopen dispatch for a Ready embedded launch on fresh local evidence.
    ///
    /// SPEC §6.1: liveness of an HTTP server is not model readiness. The evidence
    /// must be fresh within the host's observation ttl and name exactly the
    /// associated group; a deployment that is stopping or closed is never
    /// reopened.
    pub fn reverify_local_dispatch(
        &self,
        s: &CoordinatorSession,
        id: &str,
        evidence: &RemoteReadinessEvidence,
        now: i64,
    ) -> Result<(), LifecycleError> {
        crate::lifecycle::completion::nonempty_receipt(&evidence.receipt)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, e, association) = ready(&tx, s, id)?;
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
        // W10 gap (a): a gate a switch or an engine exit closed stays closed.
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
    use crate::host_drain::DrainHost;
    use mllm_domain::completion::ProcessIdentity;

    fn group() -> Vec<ProcessIdentity> {
        vec![identity("api", 61), identity("worker-0", 62)]
    }

    /// An embedded launch that reached Ready in the fixture's session.
    fn ready_local() -> (
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

    /// SPEC §4.3 (P3), §13.2: a standalone restart retires the session that
    /// owned a Ready embedded launch. The new session adopts it with dispatch
    /// closed, reopens dispatch only on fresh evidence naming exactly the
    /// associated group, and can still stop it. Nothing is released on the way.
    // T33 T38
    #[test]
    fn a_restarted_standalone_adopts_and_reproves_its_ready_embedded_launch() {
        let (store, old, fence, execution) = ready_local();
        let step = execution.token.step_id.clone();
        assert!(dispatch(&store, &fence.deployment_id));
        assert_eq!(store.local_ready_launches(&old).unwrap().len(), 1);
        // The remote path never takes an embedded launch.
        let session = store.begin_coordinator_session().unwrap();
        assert!(store.retired_remote_launches(&session).unwrap().is_empty());
        assert!(!dispatch(&store, &fence.deployment_id));
        assert!(store.local_ready_launches(&session).unwrap().is_empty());
        let retired = store.retired_local_launches(&session).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].work.step_id(), step);
        store.adopt_retired_local_launch(&session, &step).unwrap();
        // Adoption transfers the launch once; it is now this session's own.
        assert!(store.retired_local_launches(&session).unwrap().is_empty());
        assert!(store.adopt_retired_local_launch(&session, &step).is_err());
        let listed = store.local_ready_launches(&session).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].identities, group());
        assert!(!listed[0].dispatch_enabled);
        // The reservation is kept exactly.
        assert!(store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));

        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10_000;
        let evidence =
            |identities: Vec<ProcessIdentity>, observed_at_ms: i64| RemoteReadinessEvidence {
                binding_id: execution.binding_id.clone(),
                incarnation: execution.incarnation.clone(),
                identities,
                observed_at_ms,
                receipt: "recorded processes alive; authenticated model list".into(),
            };
        assert!(matches!(
            store.reverify_local_dispatch(
                &session,
                &step,
                &evidence(vec![identity("api", 61), identity("worker-0", 99)], now),
                now,
            ),
            Err(LifecycleError::Conflict)
        ));
        assert!(store
            .reverify_local_dispatch(&session, &step, &evidence(group(), now - ttl - 1), now)
            .is_err());
        assert!(!dispatch(&store, &fence.deployment_id));
        store
            .reverify_local_dispatch(&session, &step, &evidence(group(), now - 1), now)
            .unwrap();
        assert!(dispatch(&store, &fence.deployment_id));

        // SPEC §4.3: the embedded host's drain names it; a remote host's does not.
        assert_eq!(
            store
                .drain_candidates(DrainHost::Embedded)
                .unwrap()
                .iter()
                .map(|candidate| candidate.deployment_id.as_str())
                .collect::<Vec<_>>(),
            vec![fence.deployment_id.as_str()]
        );
        assert!(store
            .drain_candidates(DrainHost::Remote("lab"))
            .unwrap()
            .is_empty());

        // The adopted deployment can be stopped by the new session, and a
        // stopping deployment is never reopened by a late proof.
        store
            .accept_ordinary_stop_command(
                &session,
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop-after-adoption",
                now,
                now + 50_000,
            )
            .unwrap();
        assert!(!dispatch(&store, &fence.deployment_id));
        assert!(store
            .reverify_local_dispatch(&session, &step, &evidence(group(), now), now)
            .is_err());
    }

    fn lease(store: &crate::Store, id: &str, fence: &DeploymentFence, generation: i64, session: &str) {
        store
            .conn
            .execute(
                "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES(?1,?2,?3,?4,?5,'inflight')",
                rusqlite::params![id, fence.deployment_id, fence.revision, generation, session],
            )
            .unwrap();
    }

    fn leases(store: &crate::Store, deployment: &str) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
                [deployment],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn wall_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// W12: a crash with requests in flight left leases that used to block
    /// re-attachment forever. The launch is adopted with those leases charged
    /// and dispatch closed; they close as abandoned only on this session's fresh
    /// probe plus a later quiescence observation, never on a timer, and only
    /// then may dispatch reopen (SPEC §10, §13.2).
    // T33 T38 T32
    #[test]
    fn an_embedded_launch_with_crashed_request_leases_is_adopted_and_settled_on_quiescence() {
        use super::super::retired_leases::QuiescenceEvidence;
        let (store, old, fence, execution) = ready_local();
        let step = execution.token.step_id.clone();
        lease(&store, "lease-1", &fence, fence.generation, old.id());
        lease(&store, "lease-2", &fence, fence.generation, old.id());
        let session = store.begin_coordinator_session().unwrap();
        assert_eq!(store.retired_local_launches(&session).unwrap().len(), 1);
        store.adopt_retired_local_launch(&session, &step).unwrap();
        // Adoption carries the leases; nothing is released.
        assert_eq!(leases(&store, &fence.deployment_id), 2);
        assert_eq!(store.retired_request_leases(&session, &step).unwrap(), 2);
        let adopted: String = store
            .conn
            .query_row(
                "SELECT evidence FROM journal_entries WHERE state='launch_adopted' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(adopted.contains("2 request lease(s) of the retired session stay charged"));

        // A fresh readiness proof alone does not reopen dispatch.
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = wall_ms() + 5;
        let ready = RemoteReadinessEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: group(),
            observed_at_ms: now - 1,
            receipt: "recorded processes alive; authenticated model list".into(),
        };
        assert!(matches!(
            store.reverify_local_dispatch(&session, &step, &ready, now),
            Err(LifecycleError::Rejected(_))
        ));
        assert!(!dispatch(&store, &fence.deployment_id));

        let evidence = |probe: i64, quiet: i64, identities: Vec<ProcessIdentity>| QuiescenceEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities,
            readiness_observed_at_ms: probe,
            quiescent_at_ms: quiet,
            receipt: "engine running+waiting 0 on a fresh scrape".into(),
        };
        let started = ulid::Ulid::from_string(session.id()).unwrap().timestamp_ms() as i64;
        // A different group is not the launch that was associated.
        assert!(matches!(
            store.abandon_retired_request_leases(
                &session,
                &step,
                &evidence(now - 2, now - 1, vec![identity("api", 61), identity("worker-0", 99)]),
                now,
            ),
            Err(LifecycleError::Conflict)
        ));
        // A probe from before this session began proves nothing about it.
        assert!(store
            .abandon_retired_request_leases(&session, &step, &evidence(started - 1, now - 1, group()), now)
            .is_err());
        // Quiescence observed before the probe does not follow it.
        assert!(store
            .abandon_retired_request_leases(&session, &step, &evidence(now - 1, now - 2, group()), now)
            .is_err());
        // Evidence older than the observation ttl is not fresh; a future one is refused.
        assert!(store
            .abandon_retired_request_leases(&session, &step, &evidence(now - 2, now - 1, group()), now + ttl + 10)
            .is_err());
        assert!(store
            .abandon_retired_request_leases(&session, &step, &evidence(now - 2, now + 1, group()), now)
            .is_err());
        assert_eq!(leases(&store, &fence.deployment_id), 2);

        assert_eq!(
            store
                .abandon_retired_request_leases(&session, &step, &evidence(now - 2, now - 1, group()), now)
                .unwrap(),
            2
        );
        assert_eq!(leases(&store, &fence.deployment_id), 0);
        let abandoned: String = store
            .conn
            .query_row(
                "SELECT evidence FROM journal_entries WHERE state='request_leases_abandoned'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(abandoned.contains("2 request lease(s)"));
        // The reservation is untouched; dispatch now reopens on readiness.
        assert!(store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
        store
            .reverify_local_dispatch(&session, &step, &ready, now)
            .unwrap();
        assert!(dispatch(&store, &fence.deployment_id));
    }

    /// A lease the retired session held on another fence is not this launch's
    /// to carry: the launch stays with its retired session (SPEC §13.2).
    // T33 T18
    #[test]
    fn a_lease_on_another_fence_still_refuses_adoption() {
        let (store, old, fence, _execution) = ready_local();
        lease(&store, "lease-old", &fence, fence.generation + 1, old.id());
        let session = store.begin_coordinator_session().unwrap();
        assert!(store.retired_local_launches(&session).unwrap().is_empty());
    }

    /// W12: an operator Stop of an adopted launch that still carries a crashed
    /// session's leases completes on gone evidence, which is controlled cleanup
    /// and releases those leases with the reservation (SPEC §10).
    // T33 T32 T10
    #[test]
    fn a_stop_of_an_adopted_launch_releases_its_crashed_leases_on_gone_evidence() {
        use mllm_domain::completion::CleanupEvidence;
        let (store, old, fence, execution) = ready_local();
        let step = execution.token.step_id.clone();
        lease(&store, "lease-1", &fence, fence.generation, old.id());
        let session = store.begin_coordinator_session().unwrap();
        store.adopt_retired_local_launch(&session, &step).unwrap();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10_000;
        let receipt = store
            .accept_ordinary_stop_command(
                &session, "owner", &fence.deployment_id, fence.revision, "stop-leases", now, now + 50_000,
            )
            .unwrap();
        let (_, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        assert_eq!(context.unwrap().identities, group());
        store
            .complete_cleanup(
                &session,
                &receipt.step_id,
                &CleanupEvidence {
                    binding_id: execution.binding_id.clone(),
                    incarnation: execution.incarnation.clone(),
                    identities: group(),
                    observed_at_ms: now + 2,
                    receipt: "every recorded process gone".into(),
                },
                now + 3,
                ttl,
            )
            .unwrap();
        assert_eq!(leases(&store, &fence.deployment_id), 0);
        assert!(!store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
    }
}
