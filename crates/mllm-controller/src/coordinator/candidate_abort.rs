use super::*;

pub(in super::super) struct Cancellation {
    pub(super) run: String,
    pub(super) cancelled: AtomicBool,
    pub(super) wake: Notify,
}

struct ActiveRegistration<'a>(&'a Shared);
impl Drop for ActiveRegistration<'_> {
    fn drop(&mut self) {
        // Clearing observation state is safe during unwinding; it grants no work.
        self.0
            .active_candidate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

/// Polling is serialized with Abort's commit. Only a poll (never an await or a
/// blocking Store job) holds this gate. Dropping the future exits the original
/// child; the separate retained driver registry is deliberately untouched.
pub(in super::super) async fn scoped(
    shared: &Arc<Shared>,
    principal: &str,
    run: &str,
    future: impl Future<Output = Result<(), CoordinatorError>>,
) -> Result<(), CoordinatorError> {
    let cancellation = Arc::new(Cancellation {
        run: run.into(),
        cancelled: AtomicBool::new(false),
        wake: Notify::new(),
    });
    *shared.active_candidate.lock().map_err(|_| {
        CoordinatorError::Service("candidate cancellation registry poisoned".into())
    })? = Some(cancellation.clone());
    let _registration = ActiveRegistration(shared);
    let principal = principal.to_owned();
    let run = run.to_owned();
    let closed = shared
        .read(move |owner, _| {
            let snapshot = owner
                .store()
                .candidate_run_snapshot(&principal, &run)
                .map_err(
                    mllm_store::candidate_creation::initialize::CandidateInitializeError::from,
                )?
                .ok_or(LifecycleError::NotFound)?;
            Ok(snapshot.state() == mllm_store::candidate_creation::CandidateRunState::Aborted)
        })
        .await?;
    let result = if closed {
        Ok(())
    } else {
        tokio::pin!(future);
        loop {
            let notified = cancellation.wake.notified();
            let poll = std::future::poll_fn(|cx| {
                let guard = match shared.candidate_poll.try_lock() {
                    Ok(guard) => guard,
                    Err(std::sync::TryLockError::WouldBlock) => return std::task::Poll::Pending,
                    Err(_) => {
                        return std::task::Poll::Ready(Err(CoordinatorError::Service(
                            "candidate poll gate poisoned".into(),
                        )))
                    }
                };
                if cancellation.cancelled.load(Ordering::Acquire) {
                    return std::task::Poll::Ready(Ok(()));
                }
                // A child panic must not poison the admission serialization gate.
                let result =
                    std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)));
                drop(guard);
                match result {
                    Ok(result) => result,
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            });
            tokio::select! {
                biased;
                result = poll => break result,
                _ = notified => {},
                _ = tokio::time::sleep(shared.options.poll_interval) => {},
            }
        }
    };
    // `future` is dropped before the worker discovers another candidate/Cleanup.
    result
}

impl CoordinatorCommands {
    pub fn abort_candidate(
        &self,
        principal: &str,
        run: &str,
        expected_revision: i64,
        key: &str,
        deadline_ms: i64,
    ) -> Result<mllm_store::candidate_creation::abort::CandidateAbortReceipt, CoordinatorCommandError>
    {
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
        let text = serde_json::json!({"expected_revision":expected_revision,"action":"abort","deadline_ms":deadline_ms}).to_string();
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
            .candidate_abort_command_receipt(owner.session(), principal, run, key, &text)
            .map_err(store_error)?
        {
            return Ok(receipt);
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(CoordinatorError::Stopped("worker is not admitting Abort".into()).into());
        }
        let receipt = owner
            .store()
            .abort_candidate_run_with_clock(owner.session(), principal, run, key, &text, || {
                (self.shared.clock)().map_err(|_| LifecycleError::Stale)
            })
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
