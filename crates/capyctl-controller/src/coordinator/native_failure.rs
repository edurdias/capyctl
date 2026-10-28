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
    /// ADR 0028 §11: the launch is a group's, settled by stopping every
    /// member ([`settle_failed_group`]) rather than through one driver.
    pub(super) group: bool,
}
impl LaunchRef {
    pub(super) fn of(work: &InitializeWork) -> Self {
        Self {
            fence: work.fence().clone(),
            operation_id: work.operation_id().to_owned(),
            step_id: work.step_id().to_owned(),
            binding_id: work.binding_id().to_owned(),
            incarnation: work.incarnation().to_owned(),
            group: work.group().is_some(),
        }
    }
}

/// ADR 0028 §11, SPEC §11: settle a group launch that failed after its step
/// armed. Every member is stopped and settled on its own host's evidence
/// ([`crate::group_settlement::stop_group`]); only once every member has
/// settled (`GroupSettlement::Complete`, the rendezvous port freed with the
/// last) is the launch released, in the transaction that cancels its step,
/// exactly as a single launch is. A member left uncertain keeps its charge
/// and the step `Uncertain`; a paused retry asks its host again.
pub(super) async fn settle_failed_group(
    shared: &Arc<Shared>,
    launch: &LaunchRef,
    reason: &str,
    report: bool,
) -> Result<InitializeStatus, CoordinatorError> {
    use crate::group_settlement::{recorded_reason, stop_group, GroupTarget};
    let hosts = shared
        .groups
        .clone()
        .ok_or_else(|| CoordinatorError::Service("no group transport".into()))?;
    let binding = launch.binding_id.clone();
    let bound = shared
        .with_owner(move |owner| {
            owner
                .store()
                .group_of_binding(&binding)
                .map_err(|error| CoordinatorError::Service(error.to_string()))
        })
        .await?;
    let settled = match bound {
        // Armed but never reserved: nothing of the group exists to stop.
        None => Err("the group's plan is not recorded".to_owned()),
        Some(bound) => {
            let ctx = crate::group_activation::GroupCtx {
                owner: shared.owner.clone(),
                hosts,
                observations: shared.observations.clone(),
                clock: shared.clock.clone(),
            };
            let reason = recorded_reason(
                &ctx,
                &bound.deployment_id,
                bound.instance_index,
                bound.plan.generation(),
            );
            let target = GroupTarget {
                deployment_id: bound.deployment_id,
                instance_index: bound.instance_index,
                revision: launch.fence.revision,
                operation_id: launch.operation_id.clone(),
                plan: bound.plan,
            };
            match stop_group(&ctx, &target, reason).await {
                capyctl_store::groups::GroupSettlement::Complete => Ok(()),
                capyctl_store::groups::GroupSettlement::Partial { unsettled } => Err(format!(
                    "group_member_uncertain: ranks {unsettled:?} are not proven gone"
                )),
            }
        }
    };
    let proven = match settled {
        // The head's binding records what its readiness recorded (nothing,
        // for a group that never reached READY); every member, the head
        // included, was proven gone by its own host.
        Ok(()) => {
            let binding = launch.binding_id.clone();
            let identities = shared
                .read(move |owner, _| owner.store().runtime_binding_identities(&binding))
                .await?;
            Ok(CleanupEvidence {
                binding_id: launch.binding_id.clone(),
                incarnation: launch.incarnation.clone(),
                identities,
                observed_at_ms: (shared.clock)()?,
                receipt: "every group member settled on its own host's gone evidence".into(),
            })
        }
        Err(error) => Err(error),
    };
    conclude(shared, launch, reason, report, proven, None).await
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

    // This read is outside the release transaction, so an association still
    // running on the launcher's blocking thread may write the api identity after
    // it. Two store guards make both orderings safe, and this code depends on
    // them: `release_failed_launch` refuses evidence whose identity set differs
    // from the recorded one, so a release built on a stale empty read cannot
    // commit; and `record_launch_identity` refuses a released binding, so an
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
    // ADR 0033 §8 (lifecycle design §3): what Initialize reported about the
    // launch's LMCache decides how it is released.
    let lmcache = driver.lmcache.as_ref().and_then(|state| state.report());
    conclude(shared, launch, reason, report, proven, lmcache).await
}

/// ADR 0033 §9 (lifecycle design §3): the closed code a `lmcache: required`
/// launch failed with for a launch-time reason, as its adapter's error names
/// it (`lmcache_unavailable:<reason>`).
fn lmcache_unavailable(reason: &str) -> Option<String> {
    const PREFIX: &str = "lmcache_unavailable:";
    let start = reason.find(PREFIX)? + PREFIX.len();
    let code: String = reason[start..]
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || *c == '_')
        .collect();
    capyctl_config::lmcache::LmcacheOffReason::parse(&code).map(|_| format!("{PREFIX}{code}"))
}

/// ADR 0035 §13: the closed code a launch on several GPUs failed with at
/// readiness, as its reason names it (embedded, or the host's report):
/// `tp_rank_unobserved` or `engine_listener_exposed` (ADR 0038 §9: a
/// TensorRT-LLM launch's listeners are checked on one GPU too).
fn local_tp_failure(reason: &str) -> Option<String> {
    use capyctl_domain::local_tp::{ENGINE_LISTENER_EXPOSED, TP_RANK_UNOBSERVED};
    [TP_RANK_UNOBSERVED, ENGINE_LISTENER_EXPOSED]
        .into_iter()
        .find(|code| reason.contains(&format!("{code}:")))
        .map(str::to_owned)
}

/// ADR 0038 §16: the closed code a TensorRT-LLM launch failed with at
/// readiness, as its reason names it (embedded, or the host's report):
/// `kv_cache_short` or `engine_process_escaped`.
fn tensorrt_llm_failure(reason: &str) -> Option<String> {
    use capyctl_config::tensorrt_llm::{ENGINE_PROCESS_ESCAPED, KV_CACHE_SHORT};
    [KV_CACHE_SHORT, ENGINE_PROCESS_ESCAPED]
        .into_iter()
        .find(|code| reason.contains(&format!("{code}:")))
        .map(str::to_owned)
}

/// Release the failed launch against `proven`, or retain it `Uncertain`.
/// ADR 0033 §8: a launch whose engine failed with the LMCache connector
/// (`lmcache`) is released with the sticky fallback and started again
/// without LMCache once; a required launch-time refusal keeps its code.
async fn conclude(
    shared: &Arc<Shared>,
    launch: &LaunchRef,
    reason: &str,
    report: bool,
    proven: Result<CleanupEvidence, String>,
    lmcache: Option<capyctl_adapters::lmcache::LmcacheReport>,
) -> Result<InitializeStatus, CoordinatorError> {
    let binding_id = launch.binding_id.clone();
    let step_id = launch.step_id.clone();
    let operation_id = launch.operation_id.clone();
    let deployment_id = launch.fence.deployment_id.clone();
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
    // ADR 0033 §8: the engine failed before Ready with the connector; the
    // redacted log tail is the detail of its `engine_start_failed`.
    let fallback = match &lmcache {
        Some(capyctl_adapters::lmcache::LmcacheReport::Off {
            reason: capyctl_config::lmcache::LmcacheOffReason::EngineStartFailed,
            detail,
            connector_failed: true,
        }) => Some(detail.clone()),
        _ => None,
    };
    let other_off = match &lmcache {
        Some(report @ capyctl_adapters::lmcache::LmcacheReport::Off { .. })
            if fallback.is_none() =>
        {
            Some(lmcache_outcome(report))
        }
        _ => None,
    };
    let code = lmcache_unavailable(reason)
        .or_else(|| local_tp_failure(reason))
        .or_else(|| tensorrt_llm_failure(reason))
        .unwrap_or_else(|| "launch_failed".to_owned());
    let fallback_deployment = deployment_id.clone();
    let relaunched = shared
        .with_owner(move |owner| {
            let now = clock()?;
            fn service(error: impl std::fmt::Display) -> CoordinatorError {
                CoordinatorError::Service(error.to_string())
            }
            let ttl = owner
                .store()
                .observation_ttl_for_step(&step)
                .map_err(service)?;
            // Read before the release: the instance this launch realizes.
            let lane = owner
                .store()
                .binding_lane(&readmit_binding)
                .map_err(service)?
                .map(|(_, instance)| instance);
            let relaunched = match &fallback {
                // ADR 0033 §8 (lifecycle design §3): released with the sticky
                // fallback, the `lmcache_off` event and the instance's
                // admission closed, then (unless `required`) started again
                // once without LMCache, all in one transaction.
                Some(detail) => Some(matches!(
                    owner
                        .store()
                        .release_failed_launch_lmcache(
                            owner.session(),
                            &step,
                            &evidence,
                            now,
                            ttl,
                            detail,
                        )
                        .map_err(service)?,
                    capyctl_store::ordinary_lifecycle::lmcache::LmcacheFallback::Relaunched(_)
                )),
                None => {
                    // ADR 0033 §§3, 9: LMCache off for a launch-time reason is
                    // recorded (and `lmcache_off` journaled) before the
                    // release; evidence only, it never decides the release.
                    if let Some(outcome) = &other_off {
                        let _ = owner.store().record_launch_lmcache(
                            owner.session(),
                            &readmit_binding,
                            outcome,
                            now,
                        );
                    }
                    owner
                        .store()
                        .release_failed_launch_coded(
                            owner.session(),
                            &step,
                            &evidence,
                            now,
                            ttl,
                            &code,
                        )
                        .map_err(service)?;
                    None
                }
            };
            owner
                .store()
                .record_journal(
                    None,
                    Some(&journal_operation),
                    Some("launch_failed"),
                    &entry,
                )
                .map_err(service)?;
            if relaunched == Some(true) {
                owner
                    .store()
                    .record_journal(
                        None,
                        Some(&journal_operation),
                        Some("lmcache_fallback"),
                        &format!(
                            "deployment {fallback_deployment}: the engine did not start with \
                             LMCache; it starts again without LMCache, and later starts of \
                             this revision on this host go without it"
                        ),
                    )
                    .map_err(service)?;
            }
            // ADR 0011 decision 4: a deployment that is given up on closes its own
            // admission, not the host's. Every other deployment keeps being served.
            // ADR 0013 §6 (review finding): only the failed launch's own
            // instance closes, keyed by (deployment, instance index) and fenced
            // by its generation; its siblings keep being admitted. ADR 0033 §8:
            // the fallback release closed it itself, before its relaunch.
            if let (Some(instance), None) = (lane, relaunched) {
                owner
                    .store()
                    .close_instance_admission_at(
                        &closed_deployment,
                        i64::from(instance),
                        closed_generation,
                    )
                    .map_err(service)?;
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
            Ok(relaunched == Some(true))
        })
        .await?;
    if relaunched {
        // ADR 0033 §8: said once in the role log; the planned start is driven
        // like any accepted one.
        capyctl_domain::role_log::notice(
            capyctl_domain::role_log::Level::Warning,
            &format!(
                "deployment {deployment_id}: the engine did not start with LMCache; \
                 starting it again without LMCache"
            ),
        );
        shared.changed.notify_waiters();
        shared.wake.notify_one();
    }
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
        // ADR 0028 §11 (R38): a group head's launch is adopted without a
        // single-host runtime: its members are reached through the group
        // transport, by the handles and identities the store recorded.
        let group = reference.group && shared.groups.is_some();
        let driver = if group {
            None
        } else {
            match factory(&work) {
                Ok(driver) => Some(driver),
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
        if let Some(driver) = &driver {
            let mut retained = shared
                .retained
                .lock()
                .map_err(|_| shared.fail("runtime registry poisoned"))?;
            if retained.contains_key(&reference.binding_id) {
                return Err(shared.fail("immutable runtime binding already retained"));
            }
            retained.insert(reference.binding_id.clone(), driver.clone());
        }
        // A Ready launch keeps its dispatch closed until the readiness
        // supervisor re-proves it (a group head by its completion probe).
        if launch.completed || driver.as_ref().is_some_and(|d| d.settle.is_none()) {
            continue;
        }
        let reason = "the controller restarted before the launch outcome was observed";
        let settled = match &driver {
            Some(driver) => settle_failed_launch(shared, driver, &reference, reason, true).await,
            // ADR 0028 §11: every member settles on its own host's evidence;
            // one whose host is away stays uncertain and charged.
            None => settle_failed_group(shared, &reference, reason, true).await,
        };
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

#[cfg(test)]
mod lmcache_tests {
    // T45 T36: a `required` launch-time refusal the adapter names keeps its
    // closed code on the failed operation; any other failure is
    // `launch_failed`.
    #[test]
    fn a_required_launch_time_refusal_keeps_its_code() {
        assert_eq!(
            super::lmcache_unavailable(
                "launch failed: lmcache_unavailable:server_start_failed: no /status in 30 s"
            )
            .as_deref(),
            Some("lmcache_unavailable:server_start_failed")
        );
        assert_eq!(
            super::lmcache_unavailable("lmcache_unavailable:listener_exposed").as_deref(),
            Some("lmcache_unavailable:listener_exposed")
        );
        assert_eq!(
            super::lmcache_unavailable("lmcache_unavailable:bogus"),
            None
        );
        assert_eq!(super::lmcache_unavailable("engine exited"), None);
    }

    // T49 (ADR 0038 §16): a TensorRT-LLM launch that failed at readiness
    // keeps its closed code, named by its reason (embedded or the host's
    // report); a listener off loopback keeps `engine_listener_exposed` on one
    // GPU too; anything else is `launch_failed`.
    #[test]
    fn a_tensorrt_llm_readiness_failure_keeps_its_code() {
        assert_eq!(
            super::tensorrt_llm_failure(
                "launch failed: kv_cache_short: TensorRT-LLM built a KV pool of 3200 tokens, \
                 less than one request's window of 16384 tokens (max_seq_len)"
            )
            .as_deref(),
            Some("kv_cache_short")
        );
        assert_eq!(
            super::tensorrt_llm_failure(
                "engine_process_escaped: process 102 (a descendant of the engine's api process \
                 100) runs in process group 102, outside the launch's process group 100"
            )
            .as_deref(),
            Some("engine_process_escaped")
        );
        assert_eq!(
            super::local_tp_failure(
                "engine_listener_exposed: the api process 100 listens on address 0.0.0.0 port \
                 8101, not on loopback; the engine was stopped"
            )
            .as_deref(),
            Some("engine_listener_exposed")
        );
        // A code named in prose without its colon is not the code.
        assert_eq!(super::tensorrt_llm_failure("the kv_cache_short check"), None);
        assert_eq!(super::tensorrt_llm_failure("engine exited"), None);
    }
}
