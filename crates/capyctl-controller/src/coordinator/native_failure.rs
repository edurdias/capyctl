//! What the coordinator does when a launch fails after its step armed.
//!
//! Spec §6: once a step arms, a reservation, a port lease, a binding and an engine
//! key exist, and an engine may be holding device memory. Before this, any error
//! after arm left the step `Uncertain` and waited for an operator, so a vLLM that
//! died on a bad model path would pause the deployment until a human sent Stop.
//!
//! The coordinator classifies by proof instead, never by the builder's opinion of
//! what happened. What the store recorded is terminated, proven gone, and released
//! with that proof in one transaction; the deployment closes its own admission and
//! reads `Closed`. Anything that cannot be proven still pauses `Uncertain` with the
//! reservation retained, because an unprovable process is one that may still be
//! holding the device.
//!
//! SPEC §13.2: a remote launch has no local process tools. Its proof is the
//! authenticated host's evidence, obtained through the binding's settlement
//! Terminate; an unreachable host proves nothing and the launch stays uncertain.
//! A restarted remote worker adopts its retired session's remote launches here
//! too, so neither an uncertain nor a Ready one is stranded by a restart.

use super::*;

/// The frozen identity of one armed launch, enough to settle it without the
/// `InitializeWork` it was driven from (a paused or adopted launch has none).
#[derive(Clone, Debug)]
pub(super) struct LaunchRef {
    fence: DeploymentFence,
    operation_id: String,
    step_id: String,
    pub(super) binding_id: String,
    incarnation: String,
}
impl LaunchRef {
    pub(super) fn of(work: &InitializeWork) -> Self {
        Self {
            fence: work.fence().clone(),
            operation_id: work.operation_id().to_owned(),
            step_id: work.step_id().to_owned(),
            binding_id: work.binding_id().to_owned(),
            incarnation: work.incarnation().to_owned(),
        }
    }
}

/// Terminate what the launch recorded, prove it gone, release against that proof
/// and close the deployment. Returns the status the step now has: `Closed` when the
/// release committed, `Uncertain` when the processes could not be proven gone.
///
/// Spec §6: an empty recorded identity set is not a missing record here. The gate
/// never opened, the launcher disposed of the gated child before returning, and the
/// never-released spawn outcome is itself the evidence — so that case releases
/// without asking `verify_gone`, which answers `Indeterminate` on an empty set by
/// design. A remote launch always asks its host instead: the controller may not
/// have learned an identity the host journaled.
///
/// `report` journals the uncertain outcome; a paused retry does not repeat it.
pub(super) async fn settle_failed_launch(
    shared: &Arc<Shared>,
    driver: &Driver,
    launch: &LaunchRef,
    reason: &str,
    report: bool,
) -> Result<InitializeStatus, CoordinatorError> {
    let binding_id = launch.binding_id.clone();
    let incarnation = launch.incarnation.clone();
    let step_id = launch.step_id.clone();
    let operation_id = launch.operation_id.clone();
    let deployment_id = launch.fence.deployment_id.clone();

    // This read is outside the release transaction, so an association still
    // running on the launcher's blocking thread may write the api identity after
    // it. Two store guards make both orderings safe, and this code depends on
    // them: `release_failed_launch` refuses evidence whose identity set differs
    // from the recorded one, so a release built on a stale empty read cannot
    // commit; and `record_api_identity` refuses a released binding, so an
    // association arriving after the release is refused and the launcher disposes
    // of the gated child. Neither guard is optional for what follows.
    let recorded = {
        let binding = binding_id.clone();
        shared
            .read(move |owner, _| owner.store().runtime_binding_identities(&binding))
            .await?
    };

    let proven: Result<CleanupEvidence, String> = if let Some(settle) = driver.settle.clone() {
        // SPEC §§6.1, 13.2: only the authenticated host can prove a remote launch
        // gone. The settlement is bounded like any other control, and anything
        // short of evidence for exactly the recorded identities retains it all.
        let bound = shared.options.protocol_timeout;
        let deadline_ms =
            (shared.clock)()?.saturating_add(i64::try_from(bound.as_millis()).unwrap_or(i64::MAX));
        let context = SettlementContext {
            fence: launch.fence.clone(),
            operation_id: operation_id.clone(),
            step_id: step_id.clone(),
            binding_id: binding_id.clone(),
            incarnation: incarnation.clone(),
            identities: recorded.clone(),
            deadline_ms,
        };
        match tokio::time::timeout(bound, settle(context)).await {
            Ok(Ok(evidence))
                if evidence.binding_id == binding_id
                    && evidence.incarnation == incarnation
                    && evidence.identities == recorded =>
            {
                Ok(evidence)
            }
            Ok(Ok(_)) => Err("host evidence does not name the recorded launch".into()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("the host did not answer the settlement in time".into()),
        }
    } else if recorded.is_empty() {
        Ok(CleanupEvidence {
            binding_id: binding_id.clone(),
            incarnation: incarnation.clone(),
            identities: Vec::new(),
            observed_at_ms: (shared.clock)()?,
            receipt: "gate never opened; gated child disposed of by the launcher".into(),
        })
    } else {
        let tools = driver.tools.clone().ok_or_else(|| {
            CoordinatorError::Service(
                "a native launch failed without the process tools that made it".into(),
            )
        })?;
        let identities = recorded.clone();
        let grace = shared.options.terminate_grace;
        // Spec §5: the signal and its grace period are blocking work; keeping them
        // off the async threads is what lets a slow stop be interrupted.
        let terminated =
            tokio::task::spawn_blocking(move || tools.terminate_owned(&identities, grace))
                .await
                .map_err(|_| CoordinatorError::Service("terminate task failed".into()))?;
        match terminated {
            Ok(()) => Ok(CleanupEvidence {
                binding_id: binding_id.clone(),
                incarnation: incarnation.clone(),
                identities: recorded,
                observed_at_ms: (shared.clock)()?,
                receipt: "every recorded process observed gone after termination".into(),
            }),
            Err(error) => Err(error.to_string()),
        }
    };
    let evidence = match proven {
        Ok(evidence) => evidence,
        // Spec §6 step 4: nothing here is provable, so nothing is released.
        // The reservation is retained and an operator's Stop retries the
        // termination; a remote launch is also retried while paused.
        Err(error) => {
            let step = step_id.clone();
            shared
                .read(move |owner, now| {
                    owner
                        .store()
                        .mark_initialize_uncertain(owner.session(), &step, now)
                })
                .await?;
            if report {
                journal(
                    shared,
                    &deployment_id,
                    &operation_id,
                    "launch_uncertain",
                    &format!(
                        "launch failed: {reason}; termination did not prove the \
                         recorded processes gone: {error}"
                    ),
                )
                .await;
            }
            return Ok(InitializeStatus::Uncertain);
        }
    };

    // SPEC §17: failures are recorded. The evidence, the journal entry and the
    // closed admission commit together, so a released launch is never left without
    // the reason it was released or with its deployment still admitting.
    let clock = shared.clock.clone();
    let step = step_id.clone();
    let closed_deployment = deployment_id.clone();
    let closed_generation = launch.fence.generation;
    let journal_operation = operation_id.clone();
    // SPEC §13.2: the reason comes from a builder that saw the engine's own output,
    // so it is redacted before it is written anywhere the owner-only log is not.
    let entry = capyctl_adapters::vllm::args::redact_text(&format!(
        "deployment {deployment_id}: launch failed: {reason}"
    ));
    let readmit = shared.clone();
    let readmit_binding = binding_id.clone();
    shared
        .with_owner(move |owner| {
            let now = clock()?;
            let ttl = owner
                .store()
                .observation_ttl_for_step(&step)
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            // Read before the release: the instance this launch realizes.
            let lane = owner
                .store()
                .binding_lane(&readmit_binding)
                .map_err(|error| CoordinatorError::Service(error.to_string()))?
                .map(|(_, instance)| instance);
            owner
                .store()
                .release_failed_launch(owner.session(), &step, &evidence, now, ttl)
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            owner
                .store()
                .record_journal(
                    None,
                    Some(&journal_operation),
                    Some("launch_failed"),
                    &entry,
                )
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            // ADR 0011 decision 4: a deployment that is given up on closes its own
            // admission, not the host's. Every other deployment keeps being served.
            // ADR 0013 §6 (review finding): only the failed launch's own
            // instance closes, keyed by (deployment, instance index) and fenced
            // by its generation; its siblings keep being admitted.
            if let Some(instance) = lane {
                owner
                    .store()
                    .close_instance_admission_at(
                        &closed_deployment,
                        i64::from(instance),
                        closed_generation,
                    )
                    .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            }
            // ADR 0011 decision 4: only this deployment closed. Starts for every
            // other deployment are admitted again here, under the same lock that
            // made the release observable, so no caller can see this launch closed
            // and still be refused a start for a healthy one. Admission reads the
            // flag under this lock; found live on host-a (L7), where the next
            // start arrived in the gap between the commit and the worker's own
            // re-admission and was refused as "not admitting Initialize".
            // ADR 0015: re-admission lifts this binding's own pause, if it had
            // one, and admits starts again only when no other launch is paused.
            readmit.unpause_locked(&readmit_binding);
            Ok(())
        })
        .await?;
    // The binding was released, so the runtime built for it is no longer this
    // worker's to hand to a cleanup. Leaving it retained would refuse the next
    // launch that derives the same binding id.
    shared
        .retained
        .lock()
        .map_err(|_| shared.fail("runtime registry poisoned"))?
        .remove(&binding_id);
    Ok(InitializeStatus::Closed)
}

/// SPEC §13.2 (G1/G2 durability): adopt every remote launch a retired session
/// left, rebuild its runtime so Stop can reach it, and settle the uncertain ones
/// through their hosts. A Ready launch is adopted as it stands: its dispatch
/// stays closed until the readiness supervisor's fresh probe reopens it, and it
/// is never restarted. Returns a pause for every launch still uncertain
/// (ADR 0015 invariant 5: several launches may be paused at once, and each is
/// asked again until its own evidence settles it).
///
/// A launch whose runtime cannot be rebuilt, or that no longer validates, is
/// left with its retired session: nothing of it is released or dispatched.
pub(super) async fn adopt_retired_remote_launches(
    shared: &Arc<Shared>,
    factory: &DriverFactory,
) -> Result<Vec<Paused>, CoordinatorError> {
    let retired = shared
        .read(|owner, _| owner.store().retired_remote_launches(owner.session()))
        .await?;
    let mut paused = Vec::new();
    for launch in retired {
        let work = launch.work;
        let reference = LaunchRef::of(&work);
        let deployment = reference.fence.deployment_id.clone();
        let driver = match factory(&work) {
            Ok(driver) => driver,
            Err(error) => {
                journal(
                    shared,
                    &deployment,
                    &reference.operation_id,
                    "adoption_refused",
                    &format!("its remote runtime could not be rebuilt: {error}"),
                )
                .await;
                continue;
            }
        };
        let step = reference.step_id.clone();
        if let Err(error) = shared
            .read(move |owner, _| {
                owner
                    .store()
                    .adopt_retired_remote_launch(owner.session(), &step)
            })
            .await
        {
            if !shared.accepting.load(Ordering::Acquire) {
                return Err(error);
            }
            continue;
        }
        {
            let mut retained = shared
                .retained
                .lock()
                .map_err(|_| shared.fail("runtime registry poisoned"))?;
            if retained.contains_key(&reference.binding_id) {
                return Err(shared.fail("immutable runtime binding already retained"));
            }
            retained.insert(reference.binding_id.clone(), driver.clone());
        }
        if launch.completed || driver.settle.is_none() {
            continue;
        }
        let settled = settle_failed_launch(
            shared,
            &driver,
            &reference,
            "the controller restarted before the launch outcome was observed",
            true,
        )
        .await;
        if !shared.accepting.load(Ordering::Acquire) {
            return Err(settled.err().unwrap_or_else(|| {
                CoordinatorError::Stopped("worker stopped during adoption".into())
            }));
        }
        if matches!(settled, Ok(InitializeStatus::Closed)) {
            continue;
        }
        paused.push(Paused {
            binding: reference.binding_id.clone(),
            status: WorkerStatus::Uncertain {
                operation_id: reference.operation_id.clone(),
                reason: "an adopted remote launch awaits its host's evidence".into(),
            },
            retry: Some((
                reference,
                tokio::time::Instant::now() + shared.options.retry_cooldown,
            )),
        });
    }
    Ok(paused)
}

/// One journal entry naming the deployment and what happened to its launch.
///
/// SPEC §17: failures are recorded. The journal is evidence only, so a journal that
/// cannot be written must not turn a recorded outcome into a different one.
async fn journal(
    shared: &Arc<Shared>,
    deployment_id: &str,
    operation_id: &str,
    state: &str,
    reason: &str,
) {
    let entry =
        capyctl_adapters::vllm::args::redact_text(&format!("deployment {deployment_id}: {reason}"));
    let operation = operation_id.to_owned();
    let state = state.to_owned();
    let _ = shared
        .with_owner(move |owner| {
            owner
                .store()
                .record_journal(None, Some(&operation), Some(&state), &entry)
                .map_err(|error| CoordinatorError::Service(error.to_string()))
        })
        .await;
}
