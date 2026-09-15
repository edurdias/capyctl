use super::*;
use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;

pub(in super::super) async fn drive_security(
    shared: &Arc<Shared>,
    work: &CandidateInferenceWork,
    source: &dyn ServiceObservation,
    stop: &mut watch::Receiver<bool>,
    operation_id: &mut Option<String>,
) -> Result<(), CoordinatorError> {
    let driver = shared
        .retained_candidates
        .lock()
        .map_err(|_| CoordinatorError::Service("candidate runtime registry poisoned".into()))?
        .get(&work.binding_id)
        .cloned()
        .ok_or_else(|| {
            CoordinatorError::Service("original candidate instance unavailable".into())
        })?;
    // One worker-local chain. Only a fresh New result below authorizes each send.
    for _ in 0..4 {
        let bound = remaining(shared, work.deadline_ms)?;
        let observed = tokio::select! {
            biased;
            _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown before Security arm".into())),
            result=tokio::time::timeout(bound,source.observe(work.host_id.clone()))=>result.map_err(|_|CoordinatorError::Service("Security observation timeout".into()))??,
        };
        let controls = &work.policy.controls;
        if observed.is_empty()
            || observed.len() > 1024
            || observed
                .iter()
                .any(|o| !controls.domains.contains_key(&o.domain))
        {
            return Err(CoordinatorError::Invalid);
        }
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
        let arm_work = work.clone();
        let arm_observed = observed.clone();
        let arm_limits = limits.clone();
        let admitting = shared.clone();
        let dispatch = shared
            .read(move |owner, now| {
                if !admitting.accepting.load(Ordering::Acquire) {
                    return Err(LifecycleError::ReconciliationRequired);
                }
                let collector = owner.store().candidate_collector(
                    owner.session(),
                    &arm_work.principal,
                    &arm_work.run_id,
                )?;
                owner.store().advance_candidate_security(
                    owner.session(),
                    &collector,
                    mllm_scheduler::residency::AdmissionContext::new(
                        &arm_observed,
                        &arm_limits,
                        now,
                        ttl,
                        max_parked,
                    ),
                )
            })
            .await?;
        let context = match &dispatch {
            CandidateSecurityDispatch::AlreadyRecorded { complete: true, .. } => return Ok(()),
            CandidateSecurityDispatch::AlreadyRecorded {
                operation_id: id, ..
            } => {
                *operation_id = Some(id.clone());
                return Err(CoordinatorError::Service(
                    "Security already armed; no replay permission".into(),
                ));
            }
            CandidateSecurityDispatch::NewControl(d) => d.context(),
            CandidateSecurityDispatch::NewRequest(d) => d.context(),
        };
        *operation_id = Some(context.token.operation_id.clone());
        let issued = context.issued_at_ms;
        let send_work = work.clone();
        let send_observed = observed.clone();
        let dispatch = shared
            .read(move |owner, now| {
                owner.store().revalidate_candidate_security_send(
                    owner.session(),
                    &send_work,
                    &dispatch,
                    mllm_scheduler::residency::AdmissionContext::new(
                        &send_observed,
                        &limits,
                        now,
                        ttl,
                        max_parked,
                    ),
                )?;
                Ok(dispatch)
            })
            .await?;
        let now = (shared.clock)()?;
        if now < issued
            || now >= work.deadline_ms
            || !fresh(&observed, now, ttl)
            || *stop.borrow()
            || !shared.accepting.load(Ordering::Acquire)
        {
            return Err(CoordinatorError::Service("Security pre-send fence".into()));
        }
        let bound = Duration::from_millis((work.deadline_ms - now) as u64)
            .min(shared.options.protocol_timeout);
        let work = work.clone();
        match dispatch {
            CandidateSecurityDispatch::NewControl(d) => {
                let observation = tokio::select! {
                    biased;
                    _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown during Security control".into())),
                    result=tokio::time::timeout(bound,(driver.security_control)(*d))=>result.map_err(|_|CoordinatorError::Service("Security control timeout".into()))??,
                };
                shared
                    .read(move |owner, now| {
                        let collector = owner.store().candidate_collector(
                            owner.session(),
                            &work.principal,
                            &work.run_id,
                        )?;
                        owner.store().record_candidate_security_control(
                            owner.session(),
                            &collector,
                            &observation,
                            now,
                        )
                    })
                    .await?;
            }
            CandidateSecurityDispatch::NewRequest(d) => {
                let observation = tokio::select! {
                    biased;
                    _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown during Security request".into())),
                    result=tokio::time::timeout(bound,(driver.probe)(*d))=>result.map_err(|_|CoordinatorError::Service("Security request timeout".into()))??,
                };
                shared
                    .read(move |owner, now| {
                        let collector = owner.store().candidate_collector(
                            owner.session(),
                            &work.principal,
                            &work.run_id,
                        )?;
                        owner.store().record_candidate_result(
                            owner.session(),
                            &collector,
                            &observation,
                            now,
                        )
                    })
                    .await?;
            }
            CandidateSecurityDispatch::AlreadyRecorded { .. } => unreachable!(),
        }
    }
    Err(CoordinatorError::Service(
        "Security progression exceeded fixed program".into(),
    ))
}
