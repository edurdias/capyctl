use super::permits_send;
use crate::ownership::SharedCoordinatorState;
use futures::FutureExt;
use mllm_adapters::{
    fake::FakeEngine,
    traits::{EngineAdapter, RuntimeAction, RuntimeCommand},
};
use mllm_domain::{
    completion::{CompletionEvidence, OwnedLaunchReceipt},
    resources::{MemoryLimit, MemoryObservation},
};
use mllm_store::{
    lifecycle::{DeploymentFence, LifecycleError},
    ordinary_lifecycle::worker::{QualifiedInitializeStatus, QualifiedInitializeWork},
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
    clock: ServiceClock,
    wake: Notify,
    changed: Notify,
    accepting: AtomicBool,
    observers: Arc<Semaphore>,
    store_jobs: Arc<Semaphore>,
    retained: Mutex<BTreeMap<String, Arc<dyn EngineAdapter>>>,
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

type DriverFactory = Arc<
    dyn Fn(&QualifiedInitializeWork) -> Result<Arc<dyn EngineAdapter>, CoordinatorError>
        + Send
        + Sync,
>;

impl OwnedCoordinator {
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
                Ok(Arc::new(FakeEngine::for_qualification_with_clock(
                    Arc::new(move || {
                        clock().map_err(|_| {
                            mllm_adapters::traits::RuntimeError::Uncertain(
                                "service observation clock failed".into(),
                            )
                        })
                    }),
                )))
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
            owner,
            clock,
            wake: Notify::new(),
            changed: Notify::new(),
            accepting: AtomicBool::new(true),
            observers: Arc::new(Semaphore::new(options.max_observers)),
            store_jobs: Arc::new(Semaphore::new(options.max_observers + 1)),
            retained: Mutex::new(BTreeMap::new()),
            options,
        });
        let (stop, stop_rx) = watch::channel(false);
        let (status_tx, status) = watch::channel(WorkerStatus::Running);
        let task_shared = shared.clone();
        let task = tokio::spawn(async move {
            let result = AssertUnwindSafe(run(task_shared.clone(), observations, factory, stop_rx))
                .catch_unwind()
                .await;
            let status = result.unwrap_or_else(|_| {
                WorkerStatus::Failed("worker panicked; durable arm retained".into())
            });
            task_shared.accepting.store(false, Ordering::Release);
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
        let owner = self
            .shared
            .owner
            .lock()
            .map_err(|_| self.shared.fail("ownership mutex poisoned"))?;
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(CoordinatorError::Stopped(format!("{:?}", self.status())));
        }
        let accepted = owner
            .store()
            .accept_qualified_start(owner.session(), fence, (self.shared.clock)()?, deadline_ms)
            .map_err(|error| self.shared.store_error(error))?;
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

    pub async fn shutdown(mut self) -> Result<WorkerStatus, CoordinatorError> {
        self.shared.accepting.store(false, Ordering::Release);
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
        self.shared.accepting.store(false, Ordering::Release);
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
                    QualifiedInitializeStatus::Planned | QualifiedInitializeStatus::Armed
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

impl Shared {
    fn fail(&self, message: impl Into<String>) -> CoordinatorError {
        self.accepting.store(false, Ordering::Release);
        self.wake.notify_one();
        CoordinatorError::Service(message.into())
    }
    fn store_error(&self, error: LifecycleError) -> CoordinatorError {
        if matches!(
            error,
            LifecycleError::Sql(_) | LifecycleError::CorruptStoredData
        ) {
            self.fail(error.to_string())
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
            let owner = shared
                .owner
                .lock()
                .map_err(|_| shared.fail("ownership mutex poisoned"))?;
            let now = (shared.clock)()?;
            action(&owner, now).map_err(|error| shared.store_error(error))
        })
        .await
        .map_err(|_| self.fail("Store task panicked"))?
    }
}

async fn run(
    shared: Arc<Shared>,
    observations: Arc<dyn ServiceObservation>,
    factory: DriverFactory,
    mut stop: watch::Receiver<bool>,
) -> WorkerStatus {
    loop {
        if *stop.borrow() {
            return WorkerStatus::Stopped;
        }
        if !shared.accepting.load(Ordering::Acquire) {
            return WorkerStatus::Failed("service stopped accepting work".into());
        }
        let work = match shared
            .read(|owner, _| owner.store().next_qualified_initialize(owner.session()))
            .await
        {
            Ok(Some(work)) => work,
            Ok(None) => {
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
                let reason = match failure {
                    Ok(Err(error)) => error.to_string(),
                    _ => "worker step panicked".into(),
                };
                let status_step = step_id.clone();
                let status = shared
                    .read(move |owner, now| {
                        owner.store().qualified_initialize_status(
                            owner.session(),
                            &status_step,
                            now,
                        )
                    })
                    .await;
                return match status {
                    Ok(QualifiedInitializeStatus::Armed) => {
                        match shared
                            .read(move |owner, now| {
                                owner.store().mark_qualified_initialize_uncertain(
                                    owner.session(),
                                    &step_id,
                                    now,
                                )
                            })
                            .await
                        {
                            Ok(_) => WorkerStatus::Uncertain {
                                operation_id,
                                reason,
                            },
                            Err(error) => {
                                WorkerStatus::Failed(format!("{reason}; arm retained: {error}"))
                            }
                        }
                    }
                    Ok(QualifiedInitializeStatus::Uncertain) => WorkerStatus::Uncertain {
                        operation_id,
                        reason,
                    },
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
    // The immutable runtime survives completion and dropped observers. A future
    // cleanup lane must establish release authority before removing it.
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
        result = tokio::time::timeout(bound, driver.execute_persisted(&command)) => result.map_err(|_| CoordinatorError::Service("Initialize timeout".into()))?.map_err(|e| CoordinatorError::Service(e.to_string()))?,
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
