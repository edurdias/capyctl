use super::permits_send;
use crate::ownership::SharedCoordinatorState;
use futures::FutureExt;
use mllm_adapters::{
    fake::FakeEngine,
    traits::{EngineAdapter, RuntimeAction, RuntimeCommand},
};
use mllm_domain::{
    completion::{CleanupEvidence, CompletionEvidence, OwnedLaunchReceipt},
    resources::{MemoryLimit, MemoryObservation},
};
use mllm_store::{
    candidate_creation::cleanup::CleanupExecutionContext,
    lifecycle::{DeploymentFence, LifecycleError},
    ordinary_lifecycle::cleanup::{OrdinaryCleanupReceipt, OrdinaryCleanupStatus},
    ordinary_lifecycle::unarmed_stop::OrdinaryStopReceipt,
    ordinary_lifecycle::worker::{
        QualifiedInitializePoll, QualifiedInitializeStatus, QualifiedInitializeWork,
    },
    ordinary_lifecycle::QualifiedStartReceipt,
};
use std::{
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{watch, Notify, OwnedSemaphorePermit, Semaphore};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[path = "candidate.rs"]
mod candidate;

#[derive(Clone, Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("coordinator stopped: {0}")]
    Stopped(String),
    #[error("coordinator observer capacity exhausted")]
    Busy,
    #[error("caller observation deadline elapsed")]
    CallerTimeout,
    #[error("coordinator service failed: {0}")]
    Service(String),
    #[error("invalid coordinator options")]
    Invalid,
}

pub type ObservationFuture =
    Pin<Box<dyn Future<Output = Result<Vec<MemoryObservation>, CoordinatorError>> + Send>>;

/// Service-owned observation source. Implementations perform observations only;
/// they cannot provide owner residency credit or choose resource limits.
pub trait ServiceObservation: Send + Sync + 'static {
    fn observe(&self, host_id: String) -> ObservationFuture;
}

pub type ServiceClock = Arc<dyn Fn() -> Result<i64, CoordinatorError> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct CoordinatorOptions {
    pub poll_interval: Duration,
    pub protocol_timeout: Duration,
    pub max_observers: usize,
}
impl Default for CoordinatorOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            protocol_timeout: Duration::from_secs(30),
            max_observers: 256,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerStatus {
    Running,
    Stopped,
    Blocked {
        operation_id: String,
        reason: String,
    },
    Uncertain {
        operation_id: String,
        reason: String,
    },
    Failed(String),
}

struct Shared {
    // Ordering: candidate_poll, then owner. Store jobs never take candidate_poll.
    candidate_poll: Mutex<()>,
    active_candidate: Mutex<Option<Arc<candidate::Cancellation>>>,
    clock: ServiceClock,
    wake: Notify,
    changed: Notify,
    accepting: AtomicBool,
    cleanup_accepting: AtomicBool,
    shutdown_requested: AtomicBool,
    initializing: AtomicBool,
    observers: Arc<Semaphore>,
    store_jobs: Arc<Semaphore>,
    retained: Mutex<BTreeMap<String, Arc<Driver>>>,
    retained_candidates: Mutex<BTreeMap<String, Arc<candidate::CandidateDriver>>>,
    candidate_requests: Mutex<std::collections::VecDeque<candidate::InferenceCommand>>,
    options: CoordinatorOptions,
    // Drop retained adapters and queues before releasing process ownership.
    owner: SharedCoordinatorState,
}

/// Application-owned task. Dropping this owner requests shutdown; the task keeps
/// the process lock until its in-flight future has exited and uncertainty has
/// been recorded. Use shutdown() to explicitly join that work before restart.
pub struct OwnedCoordinator {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    status: watch::Receiver<WorkerStatus>,
    task: Option<tokio::task::JoinHandle<WorkerStatus>>,
}

/// Cloneable admission handle for the existing application-owned worker.
/// Handles retain its Store and process lock, but cannot shut down or restart it.
#[derive(Clone)]
pub struct CoordinatorCommands {
    shared: Arc<Shared>,
}

#[derive(Clone, Copy)]
pub enum CandidateLifecycleAction {
    Initialize,
    Park,
    Restore,
}

/// Preserve Store rejection categories for the service's command boundary.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorCommandError {
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
}

impl CoordinatorCommands {
    /// Blocking acceptance observer. The owned queue keeps its capacity permit
    /// and command after a caller timeout; only the worker may obtain a New grant.
    pub fn candidate_inference(
        &self,
        principal: &str,
        run: &str,
        expected_revision: i64,
        key: &str,
        body: &str,
    ) -> Result<
        mllm_store::candidate_creation::progression::CandidateInferenceReceipt,
        CoordinatorCommandError,
    > {
        let permit = self
            .shared
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoordinatorError::Busy)?;
        let owner = self
            .shared
            .owner
            .lock()
            .map_err(|_| CoordinatorError::Service("ownership mutex poisoned".into()))?;
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
            .candidate_inference_command_receipt(
                owner.session(),
                principal,
                run,
                expected_revision,
                key,
                body,
            )
            .map_err(store_error)?
        {
            return Ok(receipt);
        }
        if !self.shared.accepting.load(Ordering::Acquire)
            || !self.shared.initializing.load(Ordering::Acquire)
        {
            return Err(CoordinatorError::Stopped(
                "worker is not admitting candidate inference".into(),
            )
            .into());
        }
        let work = owner
            .store()
            .candidate_inference_work(
                owner.session(),
                principal,
                run,
                expected_revision,
                (self.shared.clock)()?,
            )
            .map_err(store_error)?;
        let (reply, receive) = std::sync::mpsc::sync_channel(1);
        self.shared
            .candidate_requests
            .lock()
            .map_err(|_| CoordinatorError::Service("candidate command queue poisoned".into()))?
            .push_back(candidate::InferenceCommand {
                work,
                expected_revision,
                key: key.into(),
                body: body.into(),
                operation_id: None,
                reply: Some(reply),
                _permit: permit,
            });
        drop(owner);
        self.shared.wake.notify_one();
        receive
            .recv_timeout(self.shared.options.protocol_timeout)
            .map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => CoordinatorError::CallerTimeout,
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    CoordinatorError::Stopped("candidate acceptance observer closed".into())
                }
            })?
    }
    /// Accept only the closed V3 Fake Initialize action for an authenticated run.
    pub fn initialize_candidate(
        &self,
        principal: &str,
        run: &str,
        expected_revision: i64,
        key: &str,
        deadline_ms: i64,
    ) -> Result<
        mllm_store::candidate_creation::progression::CandidateActionReceipt,
        CoordinatorCommandError,
    > {
        self.candidate_action(principal, run, expected_revision, key, deadline_ms, CandidateLifecycleAction::Initialize)
    }

    pub fn candidate_action(
        &self,
        principal: &str,
        run: &str,
        expected_revision: i64,
        key: &str,
        deadline_ms: i64,
        action: CandidateLifecycleAction,
    ) -> Result<mllm_store::candidate_creation::progression::CandidateActionReceipt, CoordinatorCommandError> {
        let _permit = self
            .shared
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoordinatorError::Busy)?;
        let owner = self.shared.owner.lock().map_err(|error| {
            drop(error);
            self.shared.fail("ownership mutex poisoned")
        })?;
        let store_error = |error: LifecycleError| {
            if matches!(
                error,
                LifecycleError::Sql(_) | LifecycleError::CorruptStoredData
            ) {
                self.shared.fail_locked(&owner, error.to_string());
            }
            CoordinatorCommandError::Lifecycle(error)
        };
        let action = match action {
            CandidateLifecycleAction::Initialize => "initialize",
            CandidateLifecycleAction::Park => "park",
            CandidateLifecycleAction::Restore => "restore",
        };
        let text = serde_json::json!({"expected_revision":expected_revision,"action":action,"deadline_ms":deadline_ms}).to_string();
        if let Some(receipt) = owner
            .store()
            .candidate_action_command_receipt(owner.session(), principal, run, key, &text)
            .map_err(store_error)?
        {
            return Ok(receipt);
        }
        if !self.shared.accepting.load(Ordering::Acquire)
            || !self.shared.initializing.load(Ordering::Acquire)
        {
            return Err(
                CoordinatorError::Stopped("worker is not admitting Initialize".into()).into(),
            );
        }
        let snapshot = owner.store().candidate_run_snapshot(principal, run)
            .map_err(|error| store_error(mllm_store::candidate_creation::initialize::CandidateInitializeError::from(error).into()))?
            .ok_or(LifecycleError::NotFound)?;
        if snapshot.receipt().revision() != expected_revision {
            return Err(LifecycleError::RevisionConflict.into());
        }
        let receipt = owner
            .store()
            .accept_candidate_action(
                owner.session(),
                principal,
                run,
                key,
                &text,
                (self.shared.clock)()?,
            )
            .map_err(store_error)?;
        drop(owner);
        self.shared.wake.notify_one();
        Ok(receipt)
    }

    /// Verify composition uses the worker's exact owned Store and session.
    pub fn shares_state(&self, state: &SharedCoordinatorState) -> bool {
        Arc::ptr_eq(&self.shared.owner, state)
    }

    /// Commit Stop using a service-resolved generation. History is observation only.
    pub fn stop(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<OrdinaryStopReceipt, CoordinatorCommandError> {
        let _permit = self
            .shared
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoordinatorError::Busy)?;
        let owner = self.shared.owner.lock().map_err(|error| {
            drop(error);
            self.shared.fail("ownership mutex poisoned")
        })?;
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
            .ordinary_stop_command_receipt(
                owner.session(),
                principal,
                deployment_id,
                expected_revision,
                key,
                requested_deadline_ms,
            )
            .map_err(store_error)?
        {
            return Ok(receipt);
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(CoordinatorError::Stopped("worker is not admitting cleanup".into()).into());
        }
        let receipt = owner
            .store()
            .accept_ordinary_stop_command(
                owner.session(),
                principal,
                deployment_id,
                expected_revision,
                key,
                (self.shared.clock)()?,
                requested_deadline_ms,
            )
            .map_err(store_error)?;
        drop(owner);
        self.shared.wake.notify_one();
        Ok(receipt)
    }

    /// Accept a scoped command and return observation-only committed history.
    /// Principal authentication belongs to the trusted service caller.
    pub fn start(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<QualifiedStartReceipt, CoordinatorCommandError> {
        // Synchronous admission holds one existing observer slot only until the
        // receipt returns. There is no task or unbounded queue per command.
        let _permit = self
            .shared
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoordinatorError::Busy)?;
        let owner = self.shared.owner.lock().map_err(|error| {
            // PoisonError owns the guard; release it before fail reacquires.
            drop(error);
            self.shared.fail("ownership mutex poisoned")
        })?;
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
            .qualified_start_command_receipt(
                owner.session(),
                principal,
                deployment_id,
                expected_revision,
                key,
                requested_deadline_ms,
            )
            .map_err(store_error)?
        {
            return Ok(receipt);
        }
        if !self.shared.accepting.load(Ordering::Acquire)
            || !self.shared.initializing.load(Ordering::Acquire)
        {
            return Err(
                CoordinatorError::Stopped("worker is not admitting Initialize".into()).into(),
            );
        }
        let receipt = owner
            .store()
            .accept_qualified_start_command(
                owner.session(),
                principal,
                deployment_id,
                expected_revision,
                key,
                (self.shared.clock)()?,
                requested_deadline_ms,
            )
            .map_err(store_error)?;
        drop(owner);
        self.shared.wake.notify_one();
        Ok(receipt)
    }
}

type CleanupFuture =
    Pin<Box<dyn Future<Output = Result<CleanupEvidence, CoordinatorError>> + Send>>;
// Private bundle: both callbacks capture the same immutable Fake instance.
// Public construction remains Fake-only; there is no native launch extension.
struct Driver {
    engine: Arc<dyn EngineAdapter>,
    cleanup: Arc<dyn Fn(CleanupExecutionContext) -> CleanupFuture + Send + Sync>,
}
type DriverFactory =
    Arc<dyn Fn(&QualifiedInitializeWork) -> Result<Arc<Driver>, CoordinatorError> + Send + Sync>;

impl OwnedCoordinator {
    pub fn commands(&self) -> CoordinatorCommands {
        CoordinatorCommands {
            shared: self.shared.clone(),
        }
    }

    /// First bounded lane: qualified Fake only. Construction has no engine I/O,
    /// and uses the immutable validated binding, never a profile lookup.
    pub fn spawn_fake(
        owner: SharedCoordinatorState,
        observations: Arc<dyn ServiceObservation>,
        clock: ServiceClock,
        options: CoordinatorOptions,
    ) -> Result<Self, CoordinatorError> {
        let observation_clock = clock.clone();
        Self::spawn(
            owner,
            observations,
            clock,
            options,
            Arc::new(move |work| {
                if work.effective().profile.engine != mllm_config::engine_policy::Engine::Fake
                    || !matches!(
                        work.effective().profile.launch_settings,
                        mllm_domain::launch::ProfileLaunchSettings::Fake(_)
                    )
                    || work.endpoint().is_empty()
                    || work.credential_ref().is_empty()
                {
                    return Err(CoordinatorError::Service(
                        "unsupported frozen Fake binding".into(),
                    ));
                }
                let clock = observation_clock.clone();
                let engine = Arc::new(FakeEngine::for_qualification_with_clock(Arc::new(
                    move || {
                        clock().map_err(|_| {
                            mllm_adapters::traits::RuntimeError::Uncertain(
                                "service observation clock failed".into(),
                            )
                        })
                    },
                )));
                let cleanup = engine.clone();
                Ok(Arc::new(Driver {
                    engine,
                    cleanup: Arc::new(move |context| {
                        let engine = cleanup.clone();
                        Box::pin(async move {
                            engine
                                .qualification_cleanup_observed(
                                    &context.binding_id,
                                    &context.incarnation,
                                    &context.identities,
                                )
                                .map_err(|e| CoordinatorError::Service(e.to_string()))
                        })
                    }),
                }))
            }),
        )
    }

    fn spawn(
        owner: SharedCoordinatorState,
        observations: Arc<dyn ServiceObservation>,
        clock: ServiceClock,
        options: CoordinatorOptions,
        factory: DriverFactory,
    ) -> Result<Self, CoordinatorError> {
        let candidate_clock = clock.clone();
        Self::spawn_with_candidate_factory(
            owner,
            observations,
            clock,
            options,
            factory,
            Arc::new(move || Ok(candidate::CandidateDriver::fake(candidate_clock.clone()))),
        )
    }

    fn spawn_with_candidate_factory(
        owner: SharedCoordinatorState,
        observations: Arc<dyn ServiceObservation>,
        clock: ServiceClock,
        options: CoordinatorOptions,
        factory: DriverFactory,
        candidate_factory: candidate::CandidateFactory,
    ) -> Result<Self, CoordinatorError> {
        if options.poll_interval.is_zero()
            || options.poll_interval > Duration::from_secs(1)
            || options.protocol_timeout.is_zero()
            || options.protocol_timeout > Duration::from_secs(3600)
            || options.max_observers == 0
            || options.max_observers > 16384
        {
            return Err(CoordinatorError::Invalid);
        }
        if !owner
            .lock()
            .map_err(|_| CoordinatorError::Service("ownership mutex poisoned".into()))?
            .claim_worker()
        {
            return Err(CoordinatorError::Stopped(
                "session already has an owned worker".into(),
            ));
        }
        let shared = Arc::new(Shared {
            candidate_poll: Mutex::new(()),
            active_candidate: Mutex::new(None),
            owner,
            clock,
            wake: Notify::new(),
            changed: Notify::new(),
            accepting: AtomicBool::new(true),
            cleanup_accepting: AtomicBool::new(true),
            shutdown_requested: AtomicBool::new(false),
            initializing: AtomicBool::new(true),
            observers: Arc::new(Semaphore::new(options.max_observers)),
            store_jobs: Arc::new(Semaphore::new(options.max_observers + 1)),
            retained: Mutex::new(BTreeMap::new()),
            retained_candidates: Mutex::new(BTreeMap::new()),
            candidate_requests: Mutex::new(std::collections::VecDeque::new()),
            options,
        });
        let (stop, stop_rx) = watch::channel(false);
        let (status_tx, status) = watch::channel(WorkerStatus::Running);
        let task_shared = shared.clone();
        let task = tokio::spawn(async move {
            let mut cleanup_stop = stop_rx.clone();
            let result = AssertUnwindSafe(run(
                task_shared.clone(),
                observations,
                factory,
                candidate_factory,
                stop_rx,
                &status_tx,
            ))
            .catch_unwind()
            .await;
            let mut status = result.unwrap_or_else(|_| {
                WorkerStatus::Failed("worker panicked; durable arm retained".into())
            });
            if matches!(status, WorkerStatus::Uncertain { .. }) && task_shared.cleanup_accepting.load(Ordering::Acquire) && !*cleanup_stop.borrow() {
                task_shared.close_normal_admission();
                status_tx.send_replace(status.clone());
                task_shared.changed.notify_waiters();
                status = candidate::cleanup::recover(&task_shared, &mut cleanup_stop, &status_tx, status).await;
            }
            task_shared.close_admission();
            status_tx.send_replace(status.clone());
            task_shared.changed.notify_waiters();
            status
        });
        Ok(Self {
            shared,
            stop,
            status,
            task: Some(task),
        })
    }

    /// Acceptance is synchronous and serialized with other Store transactions.
    /// Dropping the returned observer cannot cancel the accepted operation.
    pub fn start(
        &self,
        fence: &DeploymentFence,
        deadline_ms: i64,
    ) -> Result<InitializeObserver, CoordinatorError> {
        let permit = self
            .shared
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoordinatorError::Busy)?;
        let owner = self.shared.owner.lock().map_err(|error| {
            // PoisonError owns the guard; release it before fail reacquires.
            drop(error);
            self.shared.fail("ownership mutex poisoned")
        })?;
        if !self.shared.accepting.load(Ordering::Acquire)
            || !self.shared.initializing.load(Ordering::Acquire)
        {
            return Err(CoordinatorError::Stopped(format!("{:?}", self.status())));
        }
        let accepted = owner
            .store()
            .accept_qualified_start(owner.session(), fence, (self.shared.clock)()?, deadline_ms)
            .map_err(|error| self.shared.store_error(&owner, error))?;
        drop(owner);
        self.shared.wake.notify_one();
        Ok(InitializeObserver {
            operation_id: accepted.operation_id,
            step_id: accepted.step_id,
            shared: self.shared.clone(),
            _permit: permit,
        })
    }

    pub fn status(&self) -> WorkerStatus {
        self.status.borrow().clone()
    }

    /// The service supplies an authenticated principal and the exact accepted
    /// fence. A receipt retry observes its original operation only.
    pub fn stop(
        &self,
        principal: &str,
        fence: &DeploymentFence,
        key: &str,
        deadline_ms: i64,
    ) -> Result<CleanupObserver, CoordinatorError> {
        let permit = self
            .shared
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoordinatorError::Busy)?;
        let owner = self.shared.owner.lock().map_err(|error| {
            // PoisonError owns the guard; release it before fail reacquires.
            drop(error);
            self.shared.fail("ownership mutex poisoned")
        })?;
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(CoordinatorError::Stopped(format!("{:?}", self.status())));
        }
        let receipt = owner
            .store()
            .accept_ordinary_cleanup(
                owner.session(),
                principal,
                fence,
                key,
                (self.shared.clock)()?,
                deadline_ms,
            )
            .map_err(|e| self.shared.store_error(&owner, e))?;
        drop(owner);
        self.shared.wake.notify_one();
        Ok(CleanupObserver {
            receipt,
            shared: self.shared.clone(),
            _permit: permit,
        })
    }

    pub async fn shutdown(mut self) -> Result<WorkerStatus, CoordinatorError> {
        self.shared.shutdown_requested.store(true, Ordering::Release);
        self.shared.close_admission();
        self.stop.send_replace(true);
        self.task
            .take()
            .expect("owned task")
            .await
            .map_err(|_| CoordinatorError::Stopped("worker task failed".into()))
    }
}
impl Drop for OwnedCoordinator {
    fn drop(&mut self) {
        self.shared.shutdown_requested.store(true, Ordering::Release);
        self.shared.close_admission();
        self.stop.send_replace(true);
    }
}

pub struct InitializeObserver {
    operation_id: String,
    step_id: String,
    shared: Arc<Shared>,
    _permit: OwnedSemaphorePermit,
}
impl InitializeObserver {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    /// Caller timeout affects only this read. The durable operation deadline is
    /// fixed at acceptance and is neither extended nor cancelled by this future.
    pub async fn wait(
        &self,
        caller_timeout: Duration,
    ) -> Result<QualifiedInitializeStatus, CoordinatorError> {
        tokio::time::timeout(caller_timeout, async {
            loop {
                let step = self.step_id.clone();
                let status = self
                    .shared
                    .read(move |owner, now| {
                        owner
                            .store()
                            .qualified_initialize_status(owner.session(), &step, now)
                    })
                    .await?;
                if !matches!(
                    status,
                    QualifiedInitializeStatus::Planned
                        | QualifiedInitializeStatus::Armed
                        | QualifiedInitializeStatus::Expired
                ) {
                    return Ok(status);
                }
                if !self.shared.accepting.load(Ordering::Acquire) {
                    return Err(CoordinatorError::Stopped(
                        "worker halted; durable work retained".into(),
                    ));
                }
                tokio::select! {
                    _ = self.shared.changed.notified() => {},
                    _ = tokio::time::sleep(self.shared.options.poll_interval) => {},
                }
            }
        })
        .await
        .map_err(|_| CoordinatorError::CallerTimeout)?
    }
}

pub struct CleanupObserver {
    receipt: OrdinaryCleanupReceipt,
    shared: Arc<Shared>,
    _permit: OwnedSemaphorePermit,
}
impl CleanupObserver {
    pub fn receipt(&self) -> &OrdinaryCleanupReceipt {
        &self.receipt
    }
    pub fn operation_id(&self) -> &str {
        &self.receipt.operation_id
    }
    pub fn step_id(&self) -> &str {
        &self.receipt.step_id
    }

    /// Cancellation and caller deadlines affect observation only.
    pub async fn wait(
        &self,
        caller_timeout: Duration,
    ) -> Result<OrdinaryCleanupStatus, CoordinatorError> {
        tokio::time::timeout(caller_timeout, async {
            loop {
                let step = self.receipt.step_id.clone();
                let status = self
                    .shared
                    .read(move |owner, now| {
                        owner
                            .store()
                            .ordinary_cleanup_status(owner.session(), &step, now)
                    })
                    .await?;
                if !matches!(
                    status,
                    OrdinaryCleanupStatus::Planned | OrdinaryCleanupStatus::Armed
                ) {
                    return Ok(status);
                }
                if !self.shared.accepting.load(Ordering::Acquire) {
                    return Err(CoordinatorError::Stopped(
                        "worker halted; cleanup arm retained".into(),
                    ));
                }
                tokio::select! {
                    _ = self.shared.changed.notified() => {},
                    _ = tokio::time::sleep(self.shared.options.poll_interval) => {},
                }
            }
        })
        .await
        .map_err(|_| CoordinatorError::CallerTimeout)?
    }
}

impl Shared {
    fn close_normal_admission(&self) {
        let _owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        self.accepting.store(false, Ordering::Release);
        self.candidate_requests.lock().unwrap_or_else(|error|error.into_inner()).clear();
    }
    fn close_admission(&self) {
        // Serialize closure with the entire command lookup/check/commit boundary.
        // Recover a poisoned guard only to close admission, never to access Store.
        let _owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        self.accepting.store(false, Ordering::Release);
        self.cleanup_accepting.store(false, Ordering::Release);
        self.candidate_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    fn set_initializing(&self, initializing: bool) {
        let _owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        self.initializing.store(initializing, Ordering::Release);
    }

    fn fail(&self, message: impl Into<String>) -> CoordinatorError {
        let owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        self.fail_locked(&owner, message)
    }

    // Callers already holding the owned Store mutex must not acquire it again.
    fn fail_locked(
        &self,
        _owner: &crate::ownership::OwnedCoordinatorState,
        message: impl Into<String>,
    ) -> CoordinatorError {
        self.accepting.store(false, Ordering::Release);
        self.cleanup_accepting.store(false, Ordering::Release);
        self.wake.notify_one();
        CoordinatorError::Service(message.into())
    }
    fn store_error(
        &self,
        owner: &crate::ownership::OwnedCoordinatorState,
        error: LifecycleError,
    ) -> CoordinatorError {
        if matches!(
            error,
            LifecycleError::Sql(_) | LifecycleError::CorruptStoredData
        ) {
            self.fail_locked(owner, error.to_string())
        } else {
            CoordinatorError::Service(error.to_string())
        }
    }
    async fn read<T: Send + 'static>(
        self: &Arc<Self>,
        action: impl FnOnce(&crate::ownership::OwnedCoordinatorState, i64) -> Result<T, LifecycleError>
            + Send
            + 'static,
    ) -> Result<T, CoordinatorError> {
        let shared = self.clone();
        self.with_owner(move |owner| {
            let now = (shared.clock)()?;
            action(owner, now).map_err(|error| shared.store_error(owner, error))
        })
        .await
    }

    async fn read_without_clock<T: Send + 'static>(
        self: &Arc<Self>,
        action: impl FnOnce(&crate::ownership::OwnedCoordinatorState) -> Result<T, LifecycleError>
            + Send
            + 'static,
    ) -> Result<T, CoordinatorError> {
        let shared = self.clone();
        self.with_owner(move |owner| {
            action(owner).map_err(|error| shared.store_error(owner, error))
        })
        .await
    }

    async fn with_owner<T: Send + 'static>(
        self: &Arc<Self>,
        action: impl FnOnce(&crate::ownership::OwnedCoordinatorState) -> Result<T, CoordinatorError>
            + Send
            + 'static,
    ) -> Result<T, CoordinatorError> {
        // The blocking job keeps its permit even if the awaiting observer is
        // cancelled. Repeated tiny caller timeouts cannot enqueue unlimited jobs.
        let permit = self
            .store_jobs
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| self.fail("Store job queue closed"))?;
        let shared = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let owner = shared.owner.lock().map_err(|error| {
                drop(error);
                shared.fail("ownership mutex poisoned")
            })?;
            action(&owner)
        })
        .await
        .map_err(|_| self.fail("Store task panicked"))?
    }
}

async fn run(
    shared: Arc<Shared>,
    observations: Arc<dyn ServiceObservation>,
    factory: DriverFactory,
    candidate_factory: candidate::CandidateFactory,
    mut stop: watch::Receiver<bool>,
    status_tx: &watch::Sender<WorkerStatus>,
) -> WorkerStatus {
    let mut paused: Option<(String, WorkerStatus)> = None;
    loop {
        if *stop.borrow() {
            return paused.map_or(WorkerStatus::Stopped, |(_, status)| status);
        }
        if !shared.accepting.load(Ordering::Acquire) {
            return WorkerStatus::Failed("service stopped accepting work".into());
        }
        match candidate::cleanup::next(&shared, &mut stop).await {
            Ok(true) => { shared.changed.notify_waiters(); continue; }
            Ok(false) => {},
            Err(status) => return status,
        }
        // This loop owns the sole Initialize task. Reaching discovery means
        // that task has exited, including any pre-arm observation future.
        let unarmed = shared
            .read(|owner, _| {
                let Some(work) = owner.store().next_unarmed_stop(owner.session())? else {
                    return Ok(None);
                };
                owner
                    .store()
                    .complete_unarmed_stop(owner.session(), &work.step_id)?;
                Ok(Some(work))
            })
            .await;
        match unarmed {
            Ok(Some(work)) => {
                if paused
                    .as_ref()
                    .is_some_and(|(binding, _)| binding == &work.binding_id)
                {
                    paused = None;
                    shared.set_initializing(true);
                    status_tx.send_replace(WorkerStatus::Running);
                }
                shared.changed.notify_waiters();
                continue;
            }
            Ok(None) => {}
            Err(error) => return WorkerStatus::Failed(error.to_string()),
        }
        let cleanup = match shared
            .read(|owner, _| owner.store().next_ordinary_cleanup(owner.session()))
            .await
        {
            Ok(work) => work,
            Err(error) => return WorkerStatus::Failed(error.to_string()),
        };
        if let Some(cleanup) = cleanup {
            let result = AssertUnwindSafe(drive_cleanup(&shared, &cleanup, &mut stop))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(())) => {
                    if paused
                        .as_ref()
                        .is_some_and(|(binding, _)| binding == &cleanup.binding_id)
                    {
                        paused = None;
                        shared.set_initializing(true);
                        status_tx.send_replace(WorkerStatus::Running);
                    }
                    shared.changed.notify_waiters();
                }
                failure => {
                    let shutdown = matches!(&failure, Ok(Err(CoordinatorError::Stopped(_))));
                    let reason = match failure {
                        Ok(Err(error)) => error.to_string(),
                        _ => "cleanup panicked; durable arm retained".into(),
                    };
                    return if shared.accepting.load(Ordering::Acquire) || shutdown {
                        WorkerStatus::Uncertain {
                            operation_id: cleanup.operation_id,
                            reason,
                        }
                    } else {
                        WorkerStatus::Failed(format!("{reason}; cleanup authority retained"))
                    };
                }
            }
            continue;
        }
        if paused.is_some() {
            tokio::select! {
                _ = stop.changed() => {},
                _ = shared.wake.notified() => {},
                _ = tokio::time::sleep(shared.options.poll_interval) => {},
            }
            continue;
        }
        let request = match shared.candidate_requests.lock() {
            Ok(mut queue) => queue.pop_front(),
            Err(_) => return WorkerStatus::Failed("candidate command queue poisoned".into()),
        };
        if let Some(mut request) = request {
            let request_run = request.work.run_id.clone();
            let request_principal = request.work.principal.clone();
            let outcome = AssertUnwindSafe(candidate::scoped(&shared, &request_principal, &request_run, candidate::drive_inference(
                &shared,
                &mut request,
                observations.as_ref(),
                &mut stop,
            )))
            .catch_unwind()
            .await;
            match outcome {
                Ok(Ok(())) => {
                    // Only cancelled admission lacks a response on a successful
                    // drive. Failures retain their existing service error path.
                    request.respond(Err(LifecycleError::Conflict.into()));
                    shared.changed.notify_waiters();
                    continue;
                }
                failure => {
                    let reason = match failure {
                        Ok(Err(e)) => e.to_string(),
                        _ => "candidate request panicked; durable lease retained".into(),
                    };
                    return match request.operation_id {
                        Some(operation_id) => WorkerStatus::Uncertain {
                            operation_id,
                            reason,
                        },
                        None => WorkerStatus::Failed(reason),
                    };
                }
            }
        }
        let security = match shared
            .read(|owner, now| owner.store().next_candidate_security(owner.session(), now))
            .await
        {
            Ok(work) => work,
            Err(error) => return WorkerStatus::Failed(error.to_string()),
        };
        if let Some(work) = security {
            let mut operation_id = None;
            let result = AssertUnwindSafe(candidate::scoped(&shared, &work.principal, &work.run_id, candidate::drive_security(
                &shared,
                &work,
                observations.as_ref(),
                &mut stop,
                &mut operation_id,
            )))
            .catch_unwind()
            .await;
            match result {
                Ok(Ok(())) => {
                    shared.changed.notify_waiters();
                    continue;
                }
                failure => {
                    let reason = match failure {
                        Ok(Err(error)) => error.to_string(),
                        _ => "candidate Security panicked; durable authority retained".into(),
                    };
                    return match operation_id {
                        Some(operation_id) => WorkerStatus::Uncertain { operation_id, reason },
                        None => WorkerStatus::Failed(reason),
                    };
                }
            }
        }
        let warm = match shared.read(|owner, now| owner.store().next_candidate_warm(owner.session(), now)).await {
            Ok(work) => work,
            Err(error) => return WorkerStatus::Failed(error.to_string()),
        };
        if let Some(work) = warm {
            let result = AssertUnwindSafe(candidate::scoped(&shared, &work.principal, &work.run_id, candidate::drive_warm(&shared, &work, observations.as_ref(), &mut stop))).catch_unwind().await;
            match result {
                Ok(Ok(())) => { shared.changed.notify_waiters(); continue; }
                failure => return WorkerStatus::Uncertain {
                    operation_id: work.operation_id,
                    reason: match failure { Ok(Err(error)) => error.to_string(), _ => "candidate warm child panicked; durable arm retained".into() },
                },
            }
        }
        let candidate = match shared
            .read(|owner, now| {
                owner
                    .store()
                    .next_candidate_initialize(owner.session(), now)
            })
            .await
        {
            Ok(work) => work,
            Err(error) => return WorkerStatus::Failed(error.to_string()),
        };
        if let Some(work) = candidate {
            let result = AssertUnwindSafe(candidate::scoped(&shared, &work.principal, &work.run_id, candidate::drive(
                &shared,
                &work,
                observations.as_ref(),
                &candidate_factory,
                &mut stop,
            )))
            .catch_unwind()
            .await;
            match result {
                Ok(Ok(())) => {
                    shared.changed.notify_waiters();
                    continue;
                }
                failure => {
                    let reason = match failure {
                        Ok(Err(error)) => error.to_string(),
                        _ => "candidate effect panicked; durable arm retained".into(),
                    };
                    // No automatic retry or later child follows a lost result.
                    return WorkerStatus::Uncertain {
                        operation_id: work.operation_id,
                        reason,
                    };
                }
            }
        }
        let work = match shared
            .read(|owner, now| {
                owner
                    .store()
                    .next_qualified_initialize_or_expire(owner.session(), now)
            })
            .await
        {
            Ok(QualifiedInitializePoll::Work(work)) => work,
            Ok(QualifiedInitializePoll::ExpiredUnarmed) => {
                shared.changed.notify_waiters();
                continue;
            }
            Ok(QualifiedInitializePoll::Idle) => {
                tokio::select! {
                    _ = stop.changed() => {},
                    _ = shared.wake.notified() => {},
                    _ = tokio::time::sleep(shared.options.poll_interval) => {},
                }
                continue;
            }
            Err(error) => return WorkerStatus::Failed(error.to_string()),
        };
        let operation_id = work.operation_id().to_owned();
        let step_id = work.step_id().to_owned();
        // Catch both service and adapter panics. If an arm exists, the fenced
        // annotation below preserves it; otherwise no effect has been allowed.
        let result = AssertUnwindSafe(drive(&shared, &work, &*observations, &factory, &mut stop))
            .catch_unwind()
            .await;
        match result {
            Ok(Ok(())) => shared.changed.notify_waiters(),
            failure => {
                shared.set_initializing(false);
                let reason = match failure {
                    Ok(Err(error)) => error.to_string(),
                    _ => "worker step panicked".into(),
                };
                let status_step = step_id.clone();
                let status = shared
                    .read(move |owner, now| {
                        // Keep observation and annotation under the exact owned
                        // lock. Stop cannot transfer the claim between these
                        // transactions and strand its frozen predecessor.
                        let status = owner.store().qualified_initialize_status(
                            owner.session(),
                            &status_step,
                            now,
                        )?;
                        if status == QualifiedInitializeStatus::Expired {
                            owner.store().expire_unarmed_qualified_initialize(
                                owner.session(),
                                &status_step,
                                now,
                            )?;
                            Ok(QualifiedInitializeStatus::ExpiredUnarmed)
                        } else if status == QualifiedInitializeStatus::Armed {
                            owner.store().mark_qualified_initialize_uncertain(
                                owner.session(),
                                &status_step,
                                now,
                            )?;
                            Ok(QualifiedInitializeStatus::Uncertain)
                        } else {
                            Ok(status)
                        }
                    })
                    .await;
                let outcome = match status {
                    Ok(QualifiedInitializeStatus::ExpiredUnarmed) => {
                        shared.set_initializing(true);
                        shared.changed.notify_waiters();
                        continue;
                    }
                    Ok(QualifiedInitializeStatus::Uncertain) => WorkerStatus::Uncertain {
                        operation_id,
                        reason,
                    },
                    Ok(QualifiedInitializeStatus::Superseded) => {
                        // Stop can transfer the claim while Initialize is still
                        // running. Its frozen predecessor must remain unchanged;
                        // validate the actual successor instead of annotating
                        // through the now-stale Initialize fence.
                        let predecessor = work.step_id().to_owned();
                        match shared
                            .read(move |owner, _| {
                                if let Some(receipt) = owner
                                    .store()
                                    .unarmed_stop_for_predecessor(owner.session(), &predecessor)?
                                {
                                    return Ok(Some(receipt));
                                }
                                owner
                                    .store()
                                    .ordinary_cleanup_for_predecessor(owner.session(), &predecessor)
                                    .map(|r| r.map(Into::into))
                            })
                            .await
                        {
                            Ok(Some(cleanup))
                                if cleanup.binding_id == work.binding_id()
                                    && cleanup.incarnation == work.incarnation() =>
                            {
                                WorkerStatus::Uncertain {
                                    operation_id,
                                    reason,
                                }
                            }
                            Ok(_) => {
                                WorkerStatus::Failed(format!("{reason}; superseded work retained"))
                            }
                            Err(error) => WorkerStatus::Failed(format!(
                                "{reason}; cleanup validation failed: {error}"
                            )),
                        }
                    }
                    Ok(QualifiedInitializeStatus::Planned | QualifiedInitializeStatus::Expired) => {
                        WorkerStatus::Blocked {
                            operation_id,
                            reason,
                        }
                    }
                    Ok(_) => WorkerStatus::Failed(format!("{reason}; superseded work retained")),
                    Err(error) => WorkerStatus::Failed(format!(
                        "{reason}; durable arm retained if recorded: {error}"
                    )),
                };
                if !shared.accepting.load(Ordering::Acquire)
                    || *stop.borrow()
                    || !matches!(outcome, WorkerStatus::Uncertain { .. })
                {
                    return outcome;
                }
                // Keep the same session and retained instance for explicit Stop.
                // No further Initialize is admitted or discovered until this
                // exact retained binding has a committed verified cleanup.
                status_tx.send_replace(outcome.clone());
                paused = Some((work.binding_id().into(), outcome));
                shared.changed.notify_waiters();
            }
        }
    }
}

fn remaining(shared: &Shared, deadline: i64) -> Result<Duration, CoordinatorError> {
    let now = (shared.clock)()?;
    if now < 0 || now >= deadline {
        return Err(CoordinatorError::Service(
            "accepted deadline elapsed".into(),
        ));
    }
    Ok(Duration::from_millis((deadline - now) as u64).min(shared.options.protocol_timeout))
}

fn fresh(observations: &[MemoryObservation], now: i64, ttl: i64) -> bool {
    !observations.is_empty()
        && observations.len() <= 1024
        && ttl > 0
        && observations
            .iter()
            .all(|o| o.sampled_at_ms >= 0 && o.sampled_at_ms <= now && now - o.sampled_at_ms <= ttl)
}

async fn drive(
    shared: &Arc<Shared>,
    work: &QualifiedInitializeWork,
    source: &dyn ServiceObservation,
    factory: &DriverFactory,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let bound = remaining(shared, work.deadline_ms())?;
    let observed = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown before arm".into())),
        result = tokio::time::timeout(bound, source.observe(work.effective().host.name.clone())) => result.map_err(|_| CoordinatorError::Service("observation timeout".into()))??,
    };
    if observed.is_empty()
        || observed.len() > 1024
        || observed
            .iter()
            .any(|o| !work.policy().controls.domains.contains_key(&o.domain))
    {
        return Err(CoordinatorError::Service(
            "invalid bounded service observation".into(),
        ));
    }
    remaining(shared, work.deadline_ms())?;
    let driver = factory(work)?;
    let controls = &work.policy().controls;
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
    let step = work.step_id().to_owned();
    let ttl = controls.observation_ttl_ms;
    let max_parked = controls.max_parked as usize;
    let arm_observed = observed.clone();
    if *stop.borrow() || !shared.accepting.load(Ordering::Acquire) {
        return Err(CoordinatorError::Stopped("shutdown before arm".into()));
    }
    remaining(shared, work.deadline_ms())?;
    let (result, context) = shared
        .read(move |owner, now| {
            owner.store().arm_qualified_initialize_with_context(
                owner.session(),
                &step,
                mllm_scheduler::residency::AdmissionContext::new(
                    &arm_observed,
                    &limits,
                    now,
                    ttl,
                    max_parked,
                ),
            )
        })
        .await?;
    if !permits_send(&result) {
        return Err(CoordinatorError::Service(
            "recorded arm is not replay permission".into(),
        ));
    }
    // The immutable runtime survives completion and dropped observers. Cleanup
    // must establish durable release authority before removing it.
    if shared
        .retained
        .lock()
        .map_err(|_| shared.fail("runtime registry poisoned"))?
        .insert(work.binding_id().into(), driver.clone())
        .is_some()
    {
        return Err(shared.fail("immutable runtime binding already retained"));
    }
    let context = context.ok_or_else(|| shared.fail("new arm missing execution context"))?;
    if context.binding_id != work.binding_id()
        || context.incarnation != work.incarnation()
        || context.token.operation_id != work.operation_id()
        || context.token.step_id != work.step_id()
        || context.token.deployment_id != work.fence().deployment_id
        || context.token.revision != work.fence().revision
        || context.token.generation != work.fence().generation
        || context.token.qualification_id != work.effective().profile.qualification_id
        || context.deadline_ms != work.deadline_ms()
        || context.launch_settings.as_ref() != Some(&work.effective().profile.launch_settings)
    {
        return Err(CoordinatorError::Service("frozen binding mismatch".into()));
    }
    let step = work.step_id().to_owned();
    let expected = context.clone();
    let ttl = shared
        .read(move |owner, now| {
            owner.store().revalidate_qualified_initialize_send(
                owner.session(),
                &step,
                &expected,
                now,
            )
        })
        .await?;
    // A new clock read after ALL provenance/context/driver validation is required.
    let now = (shared.clock)()?;
    if now >= work.deadline_ms()
        || now < context.issued_at_ms
        || !fresh(&observed, now, ttl)
        || *stop.borrow()
        || !shared.accepting.load(Ordering::Acquire)
    {
        return Err(CoordinatorError::Service(
            "pre-send deadline, observation, or shutdown fence".into(),
        ));
    }
    let bound = Duration::from_millis((work.deadline_ms() - now) as u64)
        .min(shared.options.protocol_timeout);
    let command = RuntimeCommand {
        action: RuntimeAction::Initialize,
        context,
    };
    // No child task is spawned: timeout/cancellation drops this effect future
    // before recording uncertainty, so an effect task cannot outlive ownership.
    let observation = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown during Initialize".into())),
        result = tokio::time::timeout(bound, driver.engine.execute_persisted(&command)) => result.map_err(|_| CoordinatorError::Service("Initialize timeout".into()))?.map_err(|e| CoordinatorError::Service(e.to_string()))?,
    };
    let step = work.step_id().to_owned();
    let receipt = OwnedLaunchReceipt {
        binding_id: observation.binding_id,
        incarnation: observation.incarnation,
        identities: observation.identities.clone(),
        observed_at_ms: observation.observed_at_ms,
        receipt: observation.receipt.clone(),
    };
    shared
        .read(move |owner, now| {
            owner
                .store()
                .record_owned_launch(owner.session(), &step, &receipt, now)
        })
        .await?;
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities,
        observed_at_ms: observation.observed_at_ms,
        control_receipt: Some(observation.receipt),
        milestones: observation.facts,
    };
    let step = work.step_id().to_owned();
    shared
        .read(move |owner, now| {
            owner
                .store()
                .complete_step(owner.session(), &step, &evidence, now, ttl)?;
            Ok(())
        })
        .await
}

async fn drive_cleanup(
    shared: &Arc<Shared>,
    work: &OrdinaryCleanupReceipt,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    // The single loop reaches here only after the predecessor's effect future
    // has exited. Generation fencing or claim handoff never cancels that future.
    let driver = shared
        .retained
        .lock()
        .map_err(|_| shared.fail("runtime registry poisoned"))?
        .get(&work.binding_id)
        .cloned()
        .ok_or_else(|| {
            CoordinatorError::Stopped("original runtime is not retained by this worker".into())
        })?;
    remaining(shared, work.deadline_ms)?;
    if *stop.borrow() || !shared.accepting.load(Ordering::Acquire) {
        return Err(CoordinatorError::Stopped(
            "shutdown before cleanup arm".into(),
        ));
    }
    let step = work.step_id.clone();
    let (arm, context) = shared
        .read(move |owner, now| {
            owner
                .store()
                .arm_ordinary_cleanup_with_context(owner.session(), &step, now)
        })
        .await?;
    if !permits_send(&arm) {
        return Err(CoordinatorError::Service(
            "recorded cleanup arm is not replay permission".into(),
        ));
    }
    let context = context.ok_or_else(|| shared.fail("new cleanup arm missing context"))?;
    if context.binding_id != work.binding_id
        || context.incarnation != work.incarnation
        || context.operation_id != work.operation_id
        || context.step_id != work.step_id
        || context.fence.revision != work.revision
        || context.fence.generation != work.generation
        || context.deadline_ms != work.deadline_ms
        || context.mode != mllm_store::candidate_creation::cleanup::CleanupMode::TerminateOwned
    {
        return Err(shared.fail("cleanup binding mismatch"));
    }
    let step = work.step_id.clone();
    let expected = context.clone();
    let ttl = shared
        .read(move |owner, now| {
            owner
                .store()
                .revalidate_ordinary_cleanup_send(owner.session(), &step, &expected, now)
        })
        .await?;
    let now = (shared.clock)()?;
    if now < context.issued_at_ms
        || now >= context.deadline_ms
        || *stop.borrow()
        || !shared.accepting.load(Ordering::Acquire)
    {
        return Err(CoordinatorError::Service(
            "pre-send cleanup deadline or shutdown fence".into(),
        ));
    }
    let bound = Duration::from_millis((context.deadline_ms - now) as u64)
        .min(shared.options.protocol_timeout);
    let evidence = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown during cleanup".into())),
        result = tokio::time::timeout(bound, (driver.cleanup)(context)) => result.map_err(|_| CoordinatorError::Service("cleanup timeout".into()))??,
    };
    let step = work.step_id.clone();
    shared
        .read(move |owner, now| {
            owner
                .store()
                .complete_cleanup(owner.session(), &step, &evidence, now, ttl)
        })
        .await?;
    shared
        .retained
        .lock()
        .map_err(|_| shared.fail("runtime registry poisoned"))?
        .remove(&work.binding_id);
    Ok(())
}
