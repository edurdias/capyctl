use super::*;
use mllm_store::candidate_creation::cleanup::CandidateCleanupReceipt;

impl CoordinatorCommands {
    pub fn cleanup_candidate(
        &self,
        principal: &str,
        run: &str,
        expected_revision: i64,
        key: &str,
        deadline_ms: i64,
    ) -> Result<CandidateCleanupReceipt, CoordinatorCommandError> {
        let _permit = self
            .shared
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoordinatorError::Busy)?;
        let _poll = self
            .shared
            .candidate_poll
            .lock()
            .map_err(|_| CoordinatorError::Service("candidate poll gate poisoned".into()))?;
        let owner = self
            .shared
            .owner
            .lock()
            .map_err(|_| CoordinatorError::Service("ownership mutex poisoned".into()))?;
        let text = serde_json::json!({"expected_revision":expected_revision,"action":"cleanup","deadline_ms":deadline_ms}).to_string();
        let store_error = |error: LifecycleError| {
            if matches!(
                error,
                LifecycleError::Sql(_) | LifecycleError::CorruptStoredData
            ) {
                self.shared.fail_locked(&owner, error.to_string());
            }
            CoordinatorCommandError::Lifecycle(error)
        };
        if let Some(receipt) = owner
            .store()
            .candidate_cleanup_command_receipt(owner.session(), principal, run, key, &text)
            .map_err(store_error)?
        {
            return Ok(receipt);
        }
        if !self.shared.cleanup_accepting.load(Ordering::Acquire)
            || self.shared.shutdown_requested.load(Ordering::Acquire)
        {
            return Err(CoordinatorError::Stopped(
                "worker is not admitting candidate Cleanup".into(),
            )
            .into());
        }
        let binding = owner
            .store()
            .candidate_cleanup_owned_binding(owner.session(), principal, run)
            .map_err(store_error)?;
        if !self
            .shared
            .retained_candidates
            .lock()
            .map_err(|_| CoordinatorError::Service("candidate runtime registry poisoned".into()))?
            .contains_key(&binding)
        {
            return Err(LifecycleError::Unsupported.into());
        }
        let receipt = owner
            .store()
            .accept_candidate_cleanup_with_clock(
                owner.session(),
                principal,
                run,
                key,
                &text,
                || (self.shared.clock)().map_err(|_| LifecycleError::Stale),
            )
            .map_err(store_error)?;
        if let Some(active) = self
            .shared
            .active_candidate
            .lock()
            .map_err(|_| {
                CoordinatorError::Service("candidate cancellation registry poisoned".into())
            })?
            .as_ref()
            .filter(|active| active.run == run)
        {
            active.cancelled.store(true, Ordering::Release);
            active.wake.notify_one();
        }
        self.shared.wake.notify_one();
        Ok(receipt)
    }
}

// Called only by the single owner after the original candidate future has exited.
pub(in super::super) async fn next(
    shared: &Arc<Shared>,
    stop: &mut watch::Receiver<bool>,
) -> Result<bool, WorkerStatus> {
    let work = shared
        .read_without_clock(|owner| owner.store().next_candidate_cleanup(owner.session()))
        .await
        .map_err(|error| WorkerStatus::Failed(error.to_string()))?;
    let Some(work) = work else {
        return Ok(false);
    };
    match AssertUnwindSafe(drive(shared, &work, stop))
        .catch_unwind()
        .await
    {
        Ok(Ok(())) => {}
        failure => {
            return Err(WorkerStatus::Uncertain {
                operation_id: work.operation_id().into(),
                reason: match failure {
                    Ok(Err(error)) => error.to_string(),
                    _ => "candidate Cleanup panicked; authority retained".into(),
                },
            });
        }
    }
    Ok(true)
}

pub(in super::super) async fn recover(
    shared: &Arc<Shared>,
    stop: &mut watch::Receiver<bool>,
    status_tx: &watch::Sender<WorkerStatus>,
    mut status: WorkerStatus,
) -> WorkerStatus {
    // Normal admission stays closed. This live lane can only execute explicit
    // associated Cleanup, never rediscover or replay the uncertain candidate.
    loop {
        if *stop.borrow() || !shared.cleanup_accepting.load(Ordering::Acquire) {
            return status;
        }
        match AssertUnwindSafe(next(shared, stop)).catch_unwind().await {
            Ok(Ok(true)) => {
                shared.changed.notify_waiters();
            }
            Ok(Ok(false)) => {}
            result => {
                status = match result {
                    Ok(Err(failure)) => failure,
                    _ => WorkerStatus::Failed(
                        "candidate Cleanup discovery panicked; authority retained".into(),
                    ),
                };
                status_tx.send_replace(status.clone());
                // A failed planned arm must not turn into automatic retries.
                return status;
            }
        }
        tokio::select! { _ = stop.changed() => {}, _ = shared.wake.notified() => {}, _ = tokio::time::sleep(shared.options.poll_interval) => {} }
    }
}

async fn drive(
    shared: &Arc<Shared>,
    work: &CandidateCleanupReceipt,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let driver = shared
        .retained_candidates
        .lock()
        .map_err(|_| CoordinatorError::Service("candidate runtime registry poisoned".into()))?
        .get(work.binding_id())
        .cloned()
        .ok_or_else(|| {
            CoordinatorError::Stopped("original candidate runtime not retained".into())
        })?;
    let step = work.step_id().to_owned();
    let admitting = shared.clone();
    let (arm, context) = shared
        .read(move |owner, now| {
            if !admitting.cleanup_accepting.load(Ordering::Acquire)
                || admitting.shutdown_requested.load(Ordering::Acquire)
            {
                return Err(LifecycleError::ReconciliationRequired);
            }
            owner
                .store()
                .arm_candidate_cleanup_with_context(owner.session(), &step, now)
        })
        .await?;
    if !permits_send(&arm) {
        return Err(CoordinatorError::Service(
            "recorded candidate Cleanup arm is not replay permission".into(),
        ));
    }
    let context =
        context.ok_or_else(|| shared.fail("new candidate Cleanup arm missing context"))?;
    if context.binding_id != work.binding_id()
        || context.incarnation != work.incarnation()
        || context.operation_id != work.operation_id()
        || context.step_id != work.step_id()
        || context.fence.deployment_id != work.deployment_id()
        || context.fence.revision != work.revision()
        || context.fence.generation != work.generation()
        || context.deadline_ms != work.deadline_ms()
    {
        return Err(shared.fail("candidate Cleanup binding mismatch"));
    }
    let expected = context.clone();
    let step = work.step_id().to_owned();
    let admitting = shared.clone();
    let ttl = shared
        .read(move |owner, now| {
            if !admitting.cleanup_accepting.load(Ordering::Acquire)
                || admitting.shutdown_requested.load(Ordering::Acquire)
            {
                return Err(LifecycleError::ReconciliationRequired);
            }
            owner
                .store()
                .revalidate_candidate_cleanup_send(owner.session(), &step, &expected, now)
        })
        .await?;
    let now = (shared.clock)()?;
    if now < context.issued_at_ms
        || now >= context.deadline_ms
        || *stop.borrow()
        || shared.shutdown_requested.load(Ordering::Acquire)
        || !shared.cleanup_accepting.load(Ordering::Acquire)
    {
        return Err(CoordinatorError::Stopped(
            "candidate Cleanup pre-send fence".into(),
        ));
    }
    let bound = Duration::from_millis((context.deadline_ms - now) as u64)
        .min(shared.options.protocol_timeout);
    let evidence = tokio::select! {
        biased;
        _ = stop.changed() =>return Err(CoordinatorError::Stopped("shutdown during candidate Cleanup".into())),
        result = tokio::time::timeout(bound,(driver.cleanup)(context)) =>result.map_err(|_|CoordinatorError::Service("candidate Cleanup timeout".into()))??,
    };
    let step = work.step_id().to_owned();
    let admitting = shared.clone();
    shared
        .read(move |owner, _| {
            if !admitting.cleanup_accepting.load(Ordering::Acquire)
                || admitting.shutdown_requested.load(Ordering::Acquire)
            {
                return Err(LifecycleError::ReconciliationRequired);
            }
            owner.store().complete_cleanup_with_clock(
                owner.session(),
                &step,
                &evidence,
                ttl,
                || (admitting.clock)().map_err(|_| LifecycleError::Stale),
            )
        })
        .await?;
    shared
        .retained_candidates
        .lock()
        .map_err(|_| CoordinatorError::Service("candidate runtime registry poisoned".into()))?
        .remove(work.binding_id());
    Ok(())
}
