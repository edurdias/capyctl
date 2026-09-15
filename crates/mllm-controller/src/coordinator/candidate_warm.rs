use super::*;
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::candidate_creation::progression::{worker::CandidateWarmWork, PersistedEffectKind};

fn fence(
    shared: &Shared,
    work: &CandidateWarmWork,
    observed: &[MemoryObservation],
    issued: i64,
    stop: &watch::Receiver<bool>,
) -> Result<Duration, CoordinatorError> {
    let now = (shared.clock)()?;
    if now < issued
        || now >= work.deadline_ms
        || !fresh(observed, now, work.policy.controls.observation_ttl_ms)
        || *stop.borrow()
        || !shared.accepting.load(Ordering::Acquire)
    {
        return Err(CoordinatorError::Service(
            "candidate warm pre-send fence".into(),
        ));
    }
    Ok(Duration::from_millis((work.deadline_ms - now) as u64).min(shared.options.protocol_timeout))
}

pub(in super::super) async fn drive_warm(
    shared: &Arc<Shared>,
    work: &CandidateWarmWork,
    source: &dyn ServiceObservation,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let driver = shared
        .retained_candidates
        .lock()
        .map_err(|_| CoordinatorError::Service("candidate runtime registry poisoned".into()))?
        .get(&work.binding_id)
        .cloned()
        .ok_or_else(|| {
            CoordinatorError::Service("original candidate runtime unavailable".into())
        })?;
    let controls = &work.policy.controls;
    let limits: Vec<_> = controls
        .domains
        .iter()
        .map(|(domain, d)| MemoryLimit {
            domain: domain.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    let ttl = controls.observation_ttl_ms;
    let max_parked = controls.max_parked as usize;
    for (child, kind) in &work.effects {
        let bound = remaining(shared, work.deadline_ms)?;
        let observed = tokio::select! {
            biased;
            _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown before warm observation".into())),
            result=tokio::time::timeout(bound,source.observe(work.host_id.clone()))=>result.map_err(|_|CoordinatorError::Service("warm observation timeout".into()))??,
        };
        if observed.is_empty()
            || observed.len() > 1024
            || observed
                .iter()
                .any(|o| !controls.domains.contains_key(&o.domain))
        {
            return Err(CoordinatorError::Invalid);
        }
        fence(shared, work, &observed, 0, stop)?;
        let arm_observed = observed.clone();
        let arm_limits = limits.clone();
        if *kind == PersistedEffectKind::Probe {
            let parent = work.parent_step_id.clone();
            let admitting = shared.clone();
            let dispatch = shared
                .read(move |owner, now| {
                    if !admitting.accepting.load(Ordering::Acquire) {
                        return Err(LifecycleError::ReconciliationRequired);
                    }
                    owner.store().arm_candidate_probe(
                        owner.session(),
                        &parent,
                        AdmissionContext::new(&arm_observed, &arm_limits, now, ttl, max_parked),
                    )
                })
                .await?;
            let CandidateDispatchResult::New(dispatch) = dispatch else {
                return Err(CoordinatorError::Service(
                    "recorded warm probe is not replay permission".into(),
                ));
            };
            let dispatch = shared
                .read(move |owner, now| {
                    owner.store().revalidate_candidate_probe_send(
                        owner.session(),
                        &dispatch,
                        now,
                    )?;
                    Ok(dispatch)
                })
                .await?;
            let bound = fence(
                shared,
                work,
                &observed,
                dispatch.context().issued_at_ms,
                stop,
            )?;
            let observation = tokio::select! {
                biased;
                _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown during warm probe".into())),
                result=tokio::time::timeout(bound,(driver.probe)(*dispatch))=>result.map_err(|_|CoordinatorError::Service("warm probe timeout".into()))??,
            };
            let principal = work.principal.clone();
            let run = work.run_id.clone();
            shared
                .read(move |owner, now| {
                    let collector =
                        owner
                            .store()
                            .candidate_collector(owner.session(), &principal, &run)?;
                    owner.store().record_candidate_result(
                        owner.session(),
                        &collector,
                        &observation,
                        now,
                    )
                })
                .await?;
            return Ok(());
        }
        let child_id = child.clone();
        let admitting = shared.clone();
        let context = shared
            .read(move |owner, now| {
                if !admitting.accepting.load(Ordering::Acquire) {
                    return Err(LifecycleError::ReconciliationRequired);
                }
                let arm = owner.store().arm_candidate_effect(
                    owner.session(),
                    &child_id,
                    AdmissionContext::new(&arm_observed, &arm_limits, now, ttl, max_parked),
                )?;
                if !permits_send(&arm) {
                    return Err(LifecycleError::ReconciliationRequired);
                }
                owner
                    .store()
                    .candidate_effect_execution(owner.session(), &child_id)
            })
            .await?;
        if context.0 != *kind
            || context.1.binding_id != work.binding_id
            || context.1.incarnation != work.incarnation
            || context.1.token.operation_id != work.operation_id
            || context.1.token.step_id != *child
            || context.1.deadline_ms != work.deadline_ms
        {
            return Err(CoordinatorError::Service(
                "warm child immutable context mismatch".into(),
            ));
        }
        let context = context.1;
        let expected = context.clone();
        let child_id = child.clone();
        let send_observed = observed.clone();
        let send_limits = limits.clone();
        shared
            .read(move |owner, now| {
                owner.store().revalidate_candidate_warm_send(
                    owner.session(),
                    &child_id,
                    &expected,
                    AdmissionContext::new(&send_observed, &send_limits, now, ttl, max_parked),
                )
            })
            .await?;
        let bound = fence(shared, work, &observed, context.issued_at_ms, stop)?;
        let action = match kind {
            PersistedEffectKind::Drain => RuntimeAction::Drain,
            PersistedEffectKind::Park => RuntimeAction::Park,
            PersistedEffectKind::Restore => RuntimeAction::Restore,
            PersistedEffectKind::ReloadWeights => RuntimeAction::ReloadWeights,
            PersistedEffectKind::InvalidateCache => RuntimeAction::InvalidateCache,
            _ => return Err(CoordinatorError::Invalid),
        };
        let command = RuntimeCommand { action, context };
        let observation = tokio::select! {
            biased;
            _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown during warm effect".into())),
            result=tokio::time::timeout(bound,driver.engine.execute_persisted(&command))=>result.map_err(|_|CoordinatorError::Service("warm effect timeout".into()))?.map_err(|e|CoordinatorError::Service(e.to_string()))?,
        };
        let principal = work.principal.clone();
        let run = work.run_id.clone();
        let child_id = child.clone();
        shared
            .read(move |owner, now| {
                let collector =
                    owner
                        .store()
                        .candidate_collector(owner.session(), &principal, &run)?;
                owner.store().record_candidate_effect(
                    owner.session(),
                    &collector,
                    &child_id,
                    &observation,
                    now,
                )
            })
            .await?;
    }
    // Park ends with a trusted local read. There is no control send or wake here.
    let parent = work.parent_step_id.clone();
    let context = shared
        .read(move |owner, now| {
            owner
                .store()
                .candidate_parked_status_execution(owner.session(), &parent, now)
        })
        .await?;
    if *stop.borrow() || !shared.accepting.load(Ordering::Acquire) {
        return Err(CoordinatorError::Stopped(
            "shutdown before parked status".into(),
        ));
    }
    let observation = (driver.parked_status)(context)?;
    let principal = work.principal.clone();
    let run = work.run_id.clone();
    shared
        .read(move |owner, now| {
            let collector = owner
                .store()
                .candidate_collector(owner.session(), &principal, &run)?;
            owner.store().record_candidate_parked_status(
                owner.session(),
                &collector,
                &observation,
                now,
            )
        })
        .await
}
