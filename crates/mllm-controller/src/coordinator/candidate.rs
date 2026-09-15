use super::*;
use mllm_domain::qualification::CandidateRequestObservation;
use mllm_store::candidate_creation::progression::{
    worker::CandidateInitializeWork, CandidateDispatchResult, CandidateInferenceReceipt,
    CandidateInferenceWork, CandidateProbeDispatch,
};

#[path = "candidate_security.rs"]
mod security;
pub(super) use security::drive_security;
#[path = "candidate_warm.rs"]
mod warm;
pub(super) use warm::drive_warm;

pub(super) struct InferenceCommand {
    pub(super) work: CandidateInferenceWork,
    pub(super) expected_revision: i64,
    pub(super) key: String,
    pub(super) body: String,
    pub(super) operation_id: Option<String>,
    pub(super) reply: Option<
        std::sync::mpsc::SyncSender<Result<CandidateInferenceReceipt, CoordinatorCommandError>>,
    >,
    pub(super) _permit: OwnedSemaphorePermit,
}
impl InferenceCommand {
    fn respond(&mut self, response: Result<CandidateInferenceReceipt, CoordinatorCommandError>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(response);
        }
    }
}

pub(super) async fn drive_inference(
    shared: &Arc<Shared>,
    command: &mut InferenceCommand,
    source: &dyn ServiceObservation,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let work = command.work.clone();
    let driver = shared
        .retained_candidates
        .lock()
        .map_err(|_| CoordinatorError::Service("candidate runtime registry poisoned".into()))?
        .get(&work.binding_id)
        .cloned();
    let Some(driver) = driver else {
        command.respond(Err(LifecycleError::RuntimeRetained.into()));
        return Ok(());
    };
    let bound = remaining(shared, work.deadline_ms)?;
    let observed = tokio::select! {
        biased;
        _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown before candidate request grant".into())),
        result=tokio::time::timeout(bound,source.observe(work.host_id.clone()))=>result.map_err(|_|CoordinatorError::Service("candidate request observation timeout".into()))??,
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
    let principal = work.principal.clone();
    let run = work.run_id.clone();
    let key = command.key.clone();
    let body = command.body.clone();
    let revision = command.expected_revision;
    let grant_observed = observed.clone();
    let grant_limits = limits.clone();
    let grant_work = work.clone();
    let admitting = shared.clone();
    let accepted = shared
        .read(move |owner, now| {
            let result = (|| {
                if let Some(receipt) = owner.store().candidate_inference_command_receipt(
                    owner.session(),
                    &principal,
                    &run,
                    revision,
                    &key,
                    &body,
                )? {
                    return Ok((receipt, None));
                }
                if !admitting.accepting.load(Ordering::Acquire) {
                    return Err(LifecycleError::ReconciliationRequired);
                }
                let grant = owner.store().grant_candidate_inference_work(
                    owner.session(),
                    &grant_work,
                    &key,
                    &body,
                    mllm_scheduler::residency::AdmissionContext::new(
                        &grant_observed,
                        &grant_limits,
                        now,
                        ttl,
                        max_parked,
                    ),
                )?;
                match grant {
                    CandidateDispatchResult::New(dispatch) => {
                        // No fallible history read may discard this fresh send
                        // permission and let the worker continue as if unarmed.
                        let receipt = CandidateInferenceReceipt {
                            operation_id: dispatch.request_operation_id().into(),
                            deployment_id: dispatch.ticket().deployment_id().into(),
                            run_id: grant_work.run_id,
                            revision: dispatch.ticket().revision(),
                        };
                        Ok((receipt, Some(dispatch)))
                    }
                    CandidateDispatchResult::AlreadyRecorded { .. } => {
                        let receipt = owner
                            .store()
                            .candidate_inference_command_receipt(
                                owner.session(),
                                &principal,
                                &run,
                                revision,
                                &key,
                                &body,
                            )?
                            .ok_or(LifecycleError::CorruptStoredData)?;
                        Ok((receipt, None))
                    }
                }
            })();
            if let Err(error @ (LifecycleError::Sql(_) | LifecycleError::CorruptStoredData)) =
                &result
            {
                admitting.fail_locked(owner, error.to_string());
            }
            Ok(result)
        })
        .await?;
    let (receipt, dispatch) = match accepted {
        Ok(result) => result,
        Err(error) => {
            command.respond(Err(error.into()));
            return if shared.accepting.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(CoordinatorError::Stopped(
                    "candidate acceptance Store failure".into(),
                ))
            };
        }
    };
    command.operation_id = Some(receipt.operation_id.clone());
    command.respond(Ok(receipt));
    let Some(dispatch) = dispatch else {
        return Ok(());
    };
    if dispatch.context().binding_id != work.binding_id
        || dispatch.context().incarnation != work.incarnation
        || dispatch.context().deadline_ms != work.deadline_ms
    {
        return Err(CoordinatorError::Service(
            "candidate request immutable binding mismatch".into(),
        ));
    }
    let send_work = work.clone();
    let check_observed = observed.clone();
    let dispatch = shared
        .read(move |owner, now| {
            owner.store().revalidate_candidate_inference_send(
                owner.session(),
                &send_work,
                &dispatch,
                mllm_scheduler::residency::AdmissionContext::new(
                    &check_observed,
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
    if now < dispatch.context().issued_at_ms
        || now >= work.deadline_ms
        || !fresh(&observed, now, ttl)
        || *stop.borrow()
        || !shared.accepting.load(Ordering::Acquire)
    {
        return Err(CoordinatorError::Service(
            "candidate request pre-send fence".into(),
        ));
    }
    let bound =
        Duration::from_millis((work.deadline_ms - now) as u64).min(shared.options.protocol_timeout);
    let observation = tokio::select! {
        biased;
        _=stop.changed()=>return Err(CoordinatorError::Stopped("shutdown during candidate request".into())),
        result=tokio::time::timeout(bound,(driver.probe)(*dispatch))=>result.map_err(|_|CoordinatorError::Service("candidate request timeout".into()))??,
    };
    let uncertain =
        observation.terminal == mllm_domain::qualification::CandidateTerminal::Uncertain;
    shared
        .read(move |owner, now| {
            let collector = owner.store().candidate_collector(
                owner.session(),
                &work.principal,
                &work.run_id,
            )?;
            owner
                .store()
                .record_candidate_result(owner.session(), &collector, &observation, now)
        })
        .await?;
    if uncertain {
        return Err(CoordinatorError::Service(
            "candidate request uncertain; lease retained".into(),
        ));
    }
    Ok(())
}

type ProbeFuture =
    Pin<Box<dyn Future<Output = Result<CandidateRequestObservation, CoordinatorError>> + Send>>;
type SecurityControlFuture = Pin<
    Box<
        dyn Future<
                Output = Result<
                    mllm_domain::qualification::CandidateSecurityControlObservation,
                    CoordinatorError,
                >,
            > + Send,
    >,
>;
pub(super) struct CandidateDriver {
    pub(super) engine: Arc<dyn EngineAdapter>,
    pub(super) parked_status: Arc<dyn Fn(mllm_domain::completion::StepExecutionContext) -> Result<mllm_domain::qualification::CandidateParkedStatusObservation, CoordinatorError> + Send + Sync>,
    pub(super) probe: Arc<dyn Fn(CandidateProbeDispatch) -> ProbeFuture + Send + Sync>,
    pub(super) security_control: Arc<
        dyn Fn(
                mllm_store::candidate_creation::progression::CandidateSecurityControlDispatch,
            ) -> SecurityControlFuture
            + Send
            + Sync,
    >,
}
pub(super) type CandidateFactory =
    Arc<dyn Fn() -> Result<Arc<CandidateDriver>, CoordinatorError> + Send + Sync>;
impl CandidateDriver {
    pub(super) fn fake(clock: ServiceClock) -> Arc<Self> {
        let effect_clock = clock.clone();
        let engine = Arc::new(FakeEngine::for_qualification_with_clock(Arc::new(
            move || {
                effect_clock().map_err(|_| {
                    mllm_adapters::traits::RuntimeError::Uncertain(
                        "service observation clock failed".into(),
                    )
                })
            },
        )));
        let probe_engine = engine.clone();
        let control_engine = engine.clone();
        let control_clock = clock.clone();
        let status_engine = engine.clone();
        let status_clock = clock.clone();
        Arc::new(Self {
            engine,
            parked_status: Arc::new(move |context| {
                crate::qualification::collect_parked_status_with_clock(&status_engine, &context, &|| status_clock().map_err(|_| LifecycleError::Invalid))
                    .map_err(|error| CoordinatorError::Service(error.to_string()))
            }),
            security_control: Arc::new(move |dispatch| {
                let engine = control_engine.clone();
                let clock = control_clock.clone();
                Box::pin(async move {
                    crate::qualification::collect_security_control_with_clock(
                        &engine,
                        dispatch,
                        &move || clock().map_err(|_| LifecycleError::Invalid),
                    )
                    .await
                    .map_err(|error| CoordinatorError::Service(error.to_string()))
                })
            }),
            probe: Arc::new(move |dispatch| {
                let engine = probe_engine.clone();
                let clock = clock.clone();
                Box::pin(async move {
                    crate::qualification::collect_probe_with_clock(&engine, dispatch, &move || {
                        clock().map_err(|_| LifecycleError::Invalid)
                    })
                    .await
                    .map_err(|error| CoordinatorError::Service(error.to_string()))
                })
            }),
        })
    }
}

pub(super) async fn drive(
    shared: &Arc<Shared>,
    work: &CandidateInitializeWork,
    source: &dyn ServiceObservation,
    factory: &CandidateFactory,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let bound = remaining(shared, work.deadline_ms)?;
    let observed = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown before candidate arm".into())),
        result = tokio::time::timeout(bound, source.observe(work.host_id.clone())) => result.map_err(|_| CoordinatorError::Service("candidate observation timeout".into()))??,
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
    let driver = factory()?;
    fence(shared, work, &observed, ttl, 0, stop)?;
    let child = work.initialize_step_id.clone();
    let arm_observed = observed.clone();
    let arm_limits = limits.clone();
    let (arm, context) = shared
        .read(move |owner, now| {
            let arm = owner.store().arm_candidate_effect(
                owner.session(),
                &child,
                mllm_scheduler::residency::AdmissionContext::new(
                    &arm_observed,
                    &arm_limits,
                    now,
                    ttl,
                    max_parked,
                ),
            )?;
            let context = if permits_send(&arm) {
                Some(
                    owner
                        .store()
                        .candidate_effect_execution(owner.session(), &child)?
                        .1,
                )
            } else {
                None
            };
            Ok((arm, context))
        })
        .await?;
    if !permits_send(&arm) {
        return Err(CoordinatorError::Service(
            "recorded candidate arm is not replay permission".into(),
        ));
    }
    let context = context.ok_or_else(|| shared.fail("new candidate arm missing context"))?;
    if context.binding_id != work.binding_id
        || context.incarnation != work.incarnation
        || context.token.operation_id != work.operation_id
        || context.token.step_id != work.initialize_step_id
        || context.deadline_ms != work.deadline_ms
    {
        return Err(CoordinatorError::Service(
            "candidate frozen context mismatch".into(),
        ));
    }
    {
        use std::collections::btree_map::Entry;
        let mut retained = shared
            .retained_candidates
            .lock()
            .map_err(|_| shared.fail("candidate runtime registry poisoned"))?;
        match retained.entry(work.binding_id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(driver.clone());
            }
            Entry::Occupied(_) => {
                return Err(shared.fail("immutable candidate binding already retained"))
            }
        }
    }
    let child = work.initialize_step_id.clone();
    let expected = context.clone();
    let ttl = shared
        .read(move |owner, now| {
            owner.store().revalidate_candidate_initialize_send(
                owner.session(),
                &child,
                &expected,
                now,
            )
        })
        .await?;
    let bound = fence(shared, work, &observed, ttl, context.issued_at_ms, stop)?;
    let command = RuntimeCommand {
        action: RuntimeAction::Initialize,
        context,
    };
    let observation = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown during candidate Initialize".into())),
        result = tokio::time::timeout(bound, driver.engine.execute_persisted(&command)) => result.map_err(|_| CoordinatorError::Service("candidate Initialize timeout".into()))?.map_err(|e| CoordinatorError::Service(e.to_string()))?,
    };
    let owned = OwnedLaunchReceipt {
        binding_id: observation.binding_id.clone(),
        incarnation: observation.incarnation.clone(),
        identities: observation.identities.clone(),
        observed_at_ms: observation.observed_at_ms,
        receipt: observation.receipt.clone(),
    };
    let parent = work.parent_step_id.clone();
    let child = work.initialize_step_id.clone();
    let principal = work.principal.clone();
    let run = work.run_id.clone();
    shared
        .read(move |owner, now| {
            owner
                .store()
                .record_owned_launch(owner.session(), &parent, &owned, now)?;
            let collector = owner
                .store()
                .candidate_collector(owner.session(), &principal, &run)?;
            owner.store().record_candidate_effect(
                owner.session(),
                &collector,
                &child,
                &observation,
                now,
            )
        })
        .await?;
    fence(
        shared,
        work,
        &observed,
        ttl,
        command.context.issued_at_ms,
        stop,
    )?;
    let parent = work.parent_step_id.clone();
    let probe_observed = observed.clone();
    let dispatch = shared
        .read(move |owner, now| {
            owner.store().arm_candidate_probe(
                owner.session(),
                &parent,
                mllm_scheduler::residency::AdmissionContext::new(
                    &probe_observed,
                    &limits,
                    now,
                    ttl,
                    max_parked,
                ),
            )
        })
        .await?;
    let CandidateDispatchResult::New(dispatch) = dispatch else {
        return Err(CoordinatorError::Service(
            "recorded candidate probe is not replay permission".into(),
        ));
    };
    let dispatch = shared
        .read(move |owner, now| {
            owner
                .store()
                .revalidate_candidate_probe_send(owner.session(), &dispatch, now)?;
            Ok(dispatch)
        })
        .await?;
    let bound = fence(
        shared,
        work,
        &observed,
        ttl,
        dispatch.context().issued_at_ms,
        stop,
    )?;
    let observation = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown during candidate probe".into())),
        result = tokio::time::timeout(bound, (driver.probe)(*dispatch)) => result.map_err(|_| CoordinatorError::Service("candidate probe timeout".into()))??,
    };
    let principal = work.principal.clone();
    let run = work.run_id.clone();
    shared
        .read(move |owner, now| {
            let collector = owner
                .store()
                .candidate_collector(owner.session(), &principal, &run)?;
            owner
                .store()
                .record_candidate_result(owner.session(), &collector, &observation, now)
        })
        .await
}

fn fence(
    shared: &Shared,
    work: &CandidateInitializeWork,
    observed: &[MemoryObservation],
    ttl: i64,
    issued_at_ms: i64,
    stop: &watch::Receiver<bool>,
) -> Result<Duration, CoordinatorError> {
    let now = (shared.clock)()?;
    if now < issued_at_ms
        || now >= work.deadline_ms
        || !fresh(observed, now, ttl)
        || *stop.borrow()
        || !shared.accepting.load(Ordering::Acquire)
    {
        return Err(CoordinatorError::Service(
            "candidate pre-send clock, observation, or shutdown fence".into(),
        ));
    }
    Ok(Duration::from_millis((work.deadline_ms - now) as u64).min(shared.options.protocol_timeout))
}
