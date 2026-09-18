//! What the coordinator does when a native launch fails after its step armed.
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

use super::*;

/// Terminate what the launch recorded, prove it gone, release against that proof
/// and close the deployment. Returns the status the step now has: `Closed` when the
/// release committed, `Uncertain` when the processes could not be proven gone.
///
/// Spec §6: an empty recorded identity set is not a missing record here. The gate
/// never opened, the launcher disposed of the gated child before returning, and the
/// never-released spawn outcome is itself the evidence — so that case releases
/// without asking `verify_gone`, which answers `Indeterminate` on an empty set by
/// design.
pub(super) async fn settle_failed_native_launch(
    shared: &Arc<Shared>,
    driver: &Driver,
    work: &InitializeWork,
    reason: &str,
) -> Result<InitializeStatus, CoordinatorError> {
    let binding_id = work.binding_id().to_owned();
    let incarnation = work.incarnation().to_owned();
    let step_id = work.step_id().to_owned();
    let operation_id = work.operation_id().to_owned();
    let deployment_id = work.fence().deployment_id.clone();

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

    let evidence = if recorded.is_empty() {
        CleanupEvidence {
            binding_id: binding_id.clone(),
            incarnation: incarnation.clone(),
            identities: Vec::new(),
            observed_at_ms: (shared.clock)()?,
            receipt: "gate never opened; gated child disposed of by the launcher".into(),
        }
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
            Ok(()) => CleanupEvidence {
                binding_id: binding_id.clone(),
                incarnation: incarnation.clone(),
                identities: recorded,
                observed_at_ms: (shared.clock)()?,
                receipt: "every recorded process observed gone after termination".into(),
            },
            // Spec §6 step 4: nothing here is provable, so nothing is released.
            // The reservation is retained and an operator's Stop retries the
            // termination.
            Err(error) => {
                let step = step_id.clone();
                shared
                    .read(move |owner, now| {
                        owner
                            .store()
                            .mark_initialize_uncertain(owner.session(), &step, now)
                    })
                    .await?;
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
                return Ok(InitializeStatus::Uncertain);
            }
        }
    };

    // SPEC §17: failures are recorded. The evidence, the journal entry and the
    // closed admission commit together, so a released launch is never left without
    // the reason it was released or with its deployment still admitting.
    let clock = shared.clock.clone();
    let step = step_id.clone();
    let closed_deployment = deployment_id.clone();
    let journal_operation = operation_id.clone();
    // SPEC §13.2: the reason comes from a builder that saw the engine's own output,
    // so it is redacted before it is written anywhere the owner-only log is not.
    let entry = mllm_adapters::vllm::args::redact_text(&format!(
        "deployment {deployment_id}: launch failed: {reason}"
    ));
    let readmit = shared.clone();
    shared
        .with_owner(move |owner| {
            let now = clock()?;
            let ttl = owner
                .store()
                .observation_ttl_for_step(&step)
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
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
            owner
                .store()
                .set_admission_enabled(&closed_deployment, false)
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            // ADR 0011 decision 4: only this deployment closed. Starts for every
            // other deployment are admitted again here, under the same lock that
            // made the release observable, so no caller can see this launch closed
            // and still be refused a start for a healthy one. Admission reads the
            // flag under this lock; found live on host-a (L7), where the next
            // start arrived in the gap between the commit and the worker's own
            // re-admission and was refused as "not admitting Initialize".
            readmit.initializing.store(true, Ordering::Release);
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
        mllm_adapters::vllm::args::redact_text(&format!("deployment {deployment_id}: {reason}"));
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
