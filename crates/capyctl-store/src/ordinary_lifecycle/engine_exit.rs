//! An owned engine process that exited without a Terminate (W13, G08).
//!
//! SPEC §13.2: an engine that dies while Ready must stop receiving requests at
//! once, and its reservation stays charged until cleanup proves the whole
//! recorded group gone. This module is the store half of that: it validates an
//! exit observation against exactly the Ready launch it names (this session,
//! current generation, the recorded group, the host that serves it), closes that
//! instance's dispatch and journals `engine_exited`, in one transaction. The
//! settlement itself is an ordinary stop of the instance, accepted separately by
//! the caller; nothing here releases anything.
use super::*;
use capyctl_domain::completion::ProcessIdentity;

/// One observed exit of an owned engine process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineExit {
    pub deployment_id: String,
    /// The generation the launch was fenced at. An exit for any other is stale.
    pub generation: i64,
    /// The launch's completed Initialize step, which is the owned handle.
    pub step_id: String,
    /// The recorded member that exited: PID, boot and start ticks.
    pub process: ProcessIdentity,
    /// A closed phrase for the journal, such as `exit code 1` or `signal 9`.
    pub status: String,
    pub observed_at_ms: i64,
}

/// The Ready launch an accepted exit closed, for its settlement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitedLaunch {
    pub fence: DeploymentFence,
    pub instance_index: u32,
    pub operation_id: String,
    pub binding_id: String,
    /// True the first time this launch's exit was recorded.
    pub first: bool,
    /// ADR 0028 §11: the rank of the group member that exited, when the
    /// launch is a group's; `None` for a single-host launch.
    pub group_rank: Option<u32>,
}

/// Which evidence source is reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitSource<'a> {
    /// The authenticated host that serves the launch.
    Host(&'a str),
    /// The embedded role's own process observation.
    Embedded,
}

impl crate::Store {
    /// Record an exit of a member of this session's Ready launch `exit.step_id`
    /// and close that instance's dispatch. `None` when the exit names no such
    /// launch now: another generation (T34), a launch that is stopping,
    /// settled or never Ready, a process outside its recorded group, a helper
    /// of it (ADR 0027), or a source that does not serve it. Nothing changes
    /// then.
    pub fn record_engine_exit(
        &self,
        s: &CoordinatorSession,
        source: ExitSource<'_>,
        exit: &EngineExit,
    ) -> Result<Option<ExitedLaunch>, LifecycleError> {
        if exit.status.trim().is_empty() || exit.status.len() > 64 || exit.observed_at_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        // ADR 0028 §11: a group member's exit names its own Launch on its own
        // host; the group's Ready step is the head's Initialize. Only a member
        // whose identities were recorded can be matched.
        let member: Option<(String, u32, String)> = match source {
            ExitSource::Host(reporter) => tx
                .query_row(
                    "SELECT s.id,m.rank,m.identities_json FROM group_members m
                       JOIN group_plans g ON g.deployment_id=m.deployment_id AND g.instance_index=m.instance_index AND g.generation=m.generation
                       JOIN lifecycle_runs r ON r.deployment_id=m.deployment_id AND r.instance_index=m.instance_index AND r.generation=m.generation
                       JOIN operations o ON o.id=r.operation_id AND o.kind='initialize'
                       JOIN lifecycle_steps s ON s.operation_id=o.id AND s.state='completed'
                      WHERE m.deployment_id=?1 AND m.generation=?2 AND m.host_id=?3 AND m.launch_handle=?4
                        AND m.state='launched' AND g.state='active'",
                    params![exit.deployment_id, exit.generation, reporter, exit.step_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?,
            ExitSource::Embedded => None,
        };
        let step_id = member
            .as_ref()
            .map_or(exit.step_id.as_str(), |(step, ..)| step.as_str());
        let known: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='initialize')",
            [step_id],
            |r| r.get(0),
        )?;
        if !known {
            return Ok(None);
        }
        let (p, _, state) = load(&tx, step_id)?;
        if state != "completed"
            || p.deployment_id != exit.deployment_id
            || p.generation != exit.generation
        {
            return Ok(None);
        }
        match current_admitted(&tx, s, &p, true, true) {
            Ok(()) => {}
            Err(LifecycleError::Stale) => return Ok(None),
            Err(error) => return Err(error),
        }
        let binding_live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND state='live')",
            [&p.binding_id],
            |r| r.get(0),
        )?;
        let host: Option<String> = tx
            .query_row(
                "SELECT host_id FROM remote_binding_ingress WHERE binding_id=?1",
                [&p.binding_id],
                |r| r.get(0),
            )
            .optional()?;
        let served = match source {
            // A group member reports from the host its plan placed it on,
            // which the match above already required.
            ExitSource::Host(_) if member.is_some() => host.is_some(),
            ExitSource::Host(reporter) => host.as_deref() == Some(reporter),
            ExitSource::Embedded => host.is_none(),
        };
        if !binding_live || !served {
            return Ok(None);
        }
        let Some(association) = association(&tx, &p)? else {
            return Ok(None);
        };
        let recorded = match &member {
            Some((_, _, identities)) => {
                let rows: Vec<(String, u32, String, u64)> = serde_json::from_str(identities)
                    .map_err(|_| LifecycleError::CorruptStoredData)?;
                rows.into_iter()
                    .map(|(role, pid, boot_id, start_ticks)| ProcessIdentity {
                        role,
                        pid,
                        boot_id,
                        start_ticks,
                    })
                    .collect()
            }
            None => members(&association.identities)?,
        };
        // ADR 0027: a helper's exit is not the engine's. Cleanup still proves
        // it gone with the rest of the recorded group.
        if exit.process.is_helper() || !recorded.contains(&exit.process) {
            return Ok(None);
        }
        // SPEC §13.2: dispatch to this incarnation closes before anything else;
        // ownership, the reservation and request leases all stay.
        tx.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![p.deployment_id, p.instance_index, p.generation],
        )?;
        // W10 gap (a): recorded, so no switch or re-proof reopens it.
        crate::switch_state::record_closure(
            &tx,
            &p.deployment_id,
            p.instance_index,
            p.generation,
            crate::switch_state::ClosureReason::EngineExit,
        )?;
        if let Some((_, rank, _)) = &member {
            // ADR 0028 §11: any member's exit fails the whole group
            // (`group_member_failed`); its stop terminates every member.
            crate::groups::record_failure(
                &tx,
                &p.deployment_id,
                p.instance_index,
                p.generation,
                *rank,
            )
            .map_err(|_| LifecycleError::CorruptStoredData)?;
        }
        let first: bool = tx.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM journal_entries WHERE operation_id=?1 AND state='engine_exited')",
            [&p.operation_id],
            |r| r.get(0),
        )?;
        if first {
            tx.execute(
                "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,'engine_exited',?4)",
                params![
                    ulid::Ulid::new().to_string(),
                    host,
                    p.operation_id,
                    format!(
                        "deployment {} instance {}: owned {} process {} exited ({}) at {}; \
                         dispatch closed, the reservation stays until cleanup proves the \
                         recorded group gone",
                        p.deployment_id,
                        p.instance_index,
                        exit.process.role,
                        exit.process.pid,
                        exit.status,
                        exit.observed_at_ms,
                    ),
                ],
            )?;
        }
        tx.commit()?;
        Ok(Some(ExitedLaunch {
            fence: p.fence(),
            instance_index: p.instance_index,
            operation_id: p.operation_id,
            binding_id: p.binding_id,
            first,
            group_rank: member.map(|(_, rank, _)| rank),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{armed_ordinary, identity};
    use super::super::*;
    use super::{EngineExit, ExitSource};
    use capyctl_domain::completion::{CleanupEvidence, ProcessIdentity};

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
        ready_local_with(group())
    }

    /// An embedded launch that reached Ready with `group` recorded.
    fn ready_local_with(
        group: Vec<ProcessIdentity>,
    ) -> (
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
                    identities: group.clone(),
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
                    identities: group.clone(),
                    observed_at_ms: now,
                    control_receipt: Some("model list names the route".into()),
                    milestones: vec![
                        capyctl_domain::completion::Milestone::AllocationsRestored,
                        capyctl_domain::completion::Milestone::WeightsUsable,
                        capyctl_domain::completion::Milestone::CacheValid,
                        capyctl_domain::completion::Milestone::ModelUsable,
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

    fn exited(store: &crate::Store, operation: &str) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM journal_entries WHERE operation_id=?1 AND state='engine_exited'",
                [operation],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn observed(store: &crate::Store, deployment: &str) -> String {
        store
            .snapshot()
            .unwrap()
            .deployments
            .into_iter()
            .find(|d| d.id == deployment)
            .unwrap()
            .observed_state
    }

    /// SPEC §13.2 (W13): an exit of a recorded member closes dispatch at once and
    /// is journaled once; an exit naming another generation, a process outside
    /// the recorded group, an unknown launch or a source that does not serve it
    /// changes nothing. The settlement stop under the exit principal takes it
    /// over, after which repeats are stale; verified cleanup of the whole group
    /// releases everything and status reads `failed`, not `stopped`.
    // T20 T32 T34 T38
    #[test]
    fn an_engine_exit_closes_dispatch_and_settles_only_on_gone_evidence() {
        let (store, session, fence, execution) = ready_local();
        let step = execution.token.step_id.clone();
        assert!(dispatch(&store, &fence.deployment_id));
        let exit = |generation: i64, process: ProcessIdentity, step: &str| EngineExit {
            deployment_id: fence.deployment_id.clone(),
            generation,
            step_id: step.to_owned(),
            process,
            status: "signal 9".into(),
            observed_at_ms: execution.issued_at_ms + 100,
        };
        // T34: a stale generation, a foreign process, an unknown step and a
        // remote reporter for an embedded launch are all refused unchanged.
        for (source, report) in [
            (
                ExitSource::Embedded,
                exit(fence.generation + 1, identity("api", 61), &step),
            ),
            (
                ExitSource::Embedded,
                exit(fence.generation, identity("api", 99), &step),
            ),
            (
                ExitSource::Embedded,
                exit(
                    fence.generation,
                    identity("api", 61),
                    &ulid::Ulid::new().to_string(),
                ),
            ),
            (
                ExitSource::Host("lab"),
                exit(fence.generation, identity("api", 61), &step),
            ),
        ] {
            assert_eq!(
                store.record_engine_exit(&session, source, &report).unwrap(),
                None
            );
        }
        assert!(dispatch(&store, &fence.deployment_id));
        assert_eq!(exited(&store, &execution.token.operation_id), 0);

        // The recorded API process exited: dispatch closes, journaled once.
        let report = exit(fence.generation, identity("api", 61), &step);
        let first = store
            .record_engine_exit(&session, ExitSource::Embedded, &report)
            .unwrap()
            .expect("the Ready launch's own member");
        assert!(first.first);
        assert_eq!(first.fence, fence);
        assert_eq!(first.binding_id, execution.binding_id);
        assert!(!dispatch(&store, &fence.deployment_id));
        assert_eq!(observed(&store, &fence.deployment_id), "reconciling");
        let again = store
            .record_engine_exit(&session, ExitSource::Embedded, &report)
            .unwrap()
            .unwrap();
        assert!(!again.first, "a repeated report is the same exit");
        assert_eq!(exited(&store, &execution.token.operation_id), 1);
        // Nothing was released by the exit itself.
        assert!(store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));

        // The settlement: an ordinary stop of the instance, never an operator's.
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 200;
        let receipt = store
            .accept_instance_stop_command(
                &session,
                "system:engine_exit",
                &fence.deployment_id,
                0,
                fence.revision,
                "engine-exit:binding",
                now,
                now + 50_000,
            )
            .unwrap()
            .expect("the instance holds a runtime");
        assert_eq!(
            store
                .record_engine_exit(&session, ExitSource::Embedded, &report)
                .unwrap(),
            None,
            "a stopping launch is no longer Ready"
        );
        assert_eq!(observed(&store, &fence.deployment_id), "stopping");
        let (_, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        assert_eq!(
            context.unwrap().identities,
            group(),
            "the whole recorded group"
        );
        let gone = |identities: Vec<ProcessIdentity>| CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities,
            observed_at_ms: now + 2,
            receipt: "every recorded process observed gone after termination".into(),
        };
        // T32: the exited API alone is not the group; nothing is released.
        assert!(store
            .complete_cleanup(
                &session,
                &receipt.step_id,
                &gone(vec![identity("api", 61)]),
                now + 3,
                ttl
            )
            .is_err());
        assert!(store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
        store
            .complete_cleanup(&session, &receipt.step_id, &gone(group()), now + 3, ttl)
            .unwrap();
        assert!(!store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
        assert_eq!(observed(&store, &fence.deployment_id), "failed");
    }

    /// ADR 0027: a helper of a Ready launch (here a compile worker its
    /// scheduler started) exiting is not the engine exiting: nothing closes and
    /// nothing is journaled. The engine's own process exiting still closes
    /// dispatch, and the settlement's cleanup still needs the helper proven gone
    /// with the rest of the recorded group.
    #[test]
    fn a_helper_exit_is_not_an_engine_exit() {
        let recorded = vec![
            identity("api", 61),
            identity("worker-0", 62),
            identity("helper-0", 63),
        ];
        let (store, session, fence, execution) = ready_local_with(recorded.clone());
        let step = execution.token.step_id.clone();
        let exit = |process: ProcessIdentity| EngineExit {
            deployment_id: fence.deployment_id.clone(),
            generation: fence.generation,
            step_id: step.clone(),
            process,
            status: "exit status unobserved".into(),
            observed_at_ms: execution.issued_at_ms + 100,
        };
        assert_eq!(
            store
                .record_engine_exit(
                    &session,
                    ExitSource::Embedded,
                    &exit(identity("helper-0", 63))
                )
                .unwrap(),
            None,
            "a helper's exit names no engine exit"
        );
        assert!(dispatch(&store, &fence.deployment_id));
        assert_eq!(observed(&store, &fence.deployment_id), "ready");
        assert_eq!(exited(&store, &execution.token.operation_id), 0);

        let closed = store
            .record_engine_exit(
                &session,
                ExitSource::Embedded,
                &exit(identity("worker-0", 62)),
            )
            .unwrap()
            .expect("an engine process exiting is the engine exiting");
        assert!(closed.first);
        assert!(!dispatch(&store, &fence.deployment_id));

        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 200;
        let receipt = store
            .accept_instance_stop_command(
                &session,
                "system:engine_exit",
                &fence.deployment_id,
                0,
                fence.revision,
                "engine-exit:binding",
                now,
                now + 50_000,
            )
            .unwrap()
            .expect("the instance holds a runtime");
        let (_, context) = store
            .arm_ordinary_cleanup_with_context(&session, &receipt.step_id, now + 1)
            .unwrap();
        assert_eq!(
            context.unwrap().identities,
            sorted(recorded.clone()),
            "cleanup terminates the helper too"
        );
        let gone = |identities: Vec<ProcessIdentity>| CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities,
            observed_at_ms: now + 2,
            receipt: "every recorded process observed gone after termination".into(),
        };
        assert!(
            store
                .complete_cleanup(
                    &session,
                    &receipt.step_id,
                    &gone(recorded[..2].to_vec()),
                    now + 3,
                    ttl
                )
                .is_err(),
            "the helper must be proven gone as well"
        );
        store
            .complete_cleanup(&session, &receipt.step_id, &gone(recorded), now + 3, ttl)
            .unwrap();
        assert_eq!(observed(&store, &fence.deployment_id), "failed");
    }

    fn sorted(mut identities: Vec<ProcessIdentity>) -> Vec<ProcessIdentity> {
        identities.sort();
        identities
    }
}
