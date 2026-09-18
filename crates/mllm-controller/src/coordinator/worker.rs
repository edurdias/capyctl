use super::permits_send;
use crate::ownership::SharedCoordinatorState;
use futures::FutureExt;
use mllm_adapters::traits::{EngineAdapter, OwnedProcessLaunch, RuntimeAction, RuntimeCommand};
use mllm_adapters::vllm::args::redact_text;
use mllm_config::engine_policy::Engine;
use mllm_launchers::{AssociationError, LaunchAssociation};
use mllm_domain::{
    completion::{CleanupEvidence, CompletionEvidence, OwnedLaunchReceipt},
    resources::{MemoryLimit, MemoryObservation},
};
use mllm_store::{
    lifecycle::{DeploymentFence, LifecycleError},
    ordinary_lifecycle::cleanup::CleanupExecutionContext,
    ordinary_lifecycle::cleanup::{OrdinaryCleanupReceipt, OrdinaryCleanupStatus},
    ordinary_lifecycle::unarmed_stop::OrdinaryStopReceipt,
    ordinary_lifecycle::worker::{
        InitializePoll, InitializeStatus, InitializeWork,
    },
    ordinary_lifecycle::StartReceipt,
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

#[path = "native_failure.rs"]
mod native_failure;


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
    /// ADR 0011 decision 5: how many times one configuration is attempted before the
    /// deployment is given up on. Policy, not a constant; host-published policy
    /// supplies it when remote hosts exist (F3).
    pub max_attempts: u32,
    /// The wait before the first retry. Each later retry doubles it, so a broken
    /// recipe does not burn a GPU in a tight loop and a transient failure does not
    /// wait minutes.
    pub retry_cooldown: Duration,
    /// Spec §4: how long one Initialize may take. It is not `protocol_timeout`: a
    /// cold start reads weights off disk and this project measured a 4B model
    /// taking 27 to 63 seconds, so the 30-second protocol bound would give up on a
    /// healthy engine mid-load and then kill it.
    pub initialize_timeout: Duration,
    /// Spec §5: how long a terminated process group is given to exit after
    /// `SIGTERM` before it is killed.
    pub terminate_grace: Duration,
}
impl Default for CoordinatorOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            protocol_timeout: Duration::from_secs(30),
            max_observers: 256,
            max_attempts: 3,
            retry_cooldown: Duration::from_secs(30),
            initialize_timeout: Duration::from_secs(900),
            terminate_grace: Duration::from_secs(15),
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
    cleanup_accepting: AtomicBool,
    shutdown_requested: AtomicBool,
    initializing: AtomicBool,
    observers: Arc<Semaphore>,
    store_jobs: Arc<Semaphore>,
    retained: Mutex<BTreeMap<String, Arc<Driver>>>,
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

/// Preserve Store rejection categories for the service's command boundary.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorCommandError {
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
}

impl CoordinatorCommands {
    /// Answer a read against the owned store.
    ///
    /// The coordinator owns its store exclusively behind the controller lock, so
    /// callers receive answers rather than a handle. That is what makes a second
    /// authority impossible rather than merely discouraged.
    ///
    /// A poisoned ownership mutex fails the worker: this coordinator is unavailable,
    /// which is a different thing from the caller's question having no answer.
    pub fn read<T>(
        &self,
        query: impl FnOnce(&mllm_store::Store) -> Result<T, mllm_store::StoreError>,
    ) -> Result<T, crate::fault::LifecycleFault> {
        let owner = self.shared.owner.lock().map_err(|error| {
            drop(error);
            crate::fault::LifecycleFault::from(self.shared.fail("ownership mutex poisoned"))
        })?;
        query(owner.store()).map_err(Into::into)
    }

    /// Lock the owned state for a read whose error type is not a store error.
    ///
    /// `read` covers the common case; this exists for the few store APIs that
    /// report lifecycle errors instead, so neither has to widen to accommodate the
    /// other.
    pub fn owner_for_read(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, crate::ownership::OwnedCoordinatorState>, crate::fault::LifecycleFault>
    {
        self.shared.owner.lock().map_err(|error| {
            drop(error);
            crate::fault::LifecycleFault::from(self.shared.fail("ownership mutex poisoned"))
        })
    }

    /// Publish the ceiling admission is judged against.
    ///
    /// Imported with its observations: a ceiling asserted without a reading of the
    /// machine is a guess, and a guess is how a host gets overcommitted.
    pub fn import_resource_policy(
        &self,
        host: &mllm_config::effective::HostPolicy,
        observations: &[mllm_domain::resources::MemoryObservation],
        now_ms: i64,
    ) -> Result<(), crate::fault::LifecycleFault> {
        let owner = self.shared.owner.lock().map_err(|error| {
            drop(error);
            crate::fault::LifecycleFault::from(self.shared.fail("ownership mutex poisoned"))
        })?;
        owner
            .store()
            .import_resource_policy(owner.session(), host, observations, now_ms)
            .map_err(|error| {
                crate::fault::LifecycleFault::Blocked(format!("resource policy refused: {error}"))
            })?;
        Ok(())
    }

    /// Create a stopped managed configuration.
    ///
    /// This is deployment creation in this model: the record and its effective
    /// revision are written together.
    pub fn create_managed_configuration(
        &self,
        principal: &str,
        key: &str,
        request_json: &str,
        trusted_host: &serde_json::Value,
        now_ms: i64,
    ) -> Result<
        mllm_store::managed_configuration::ManagedConfigurationReceipt,
        crate::fault::LifecycleFault,
    > {
        let owner = self.shared.owner.lock().map_err(|error| {
            drop(error);
            crate::fault::LifecycleFault::from(self.shared.fail("ownership mutex poisoned"))
        })?;
        owner
            .store()
            .create_stopped_managed_configuration(
                owner.session(),
                principal,
                key,
                request_json,
                trusted_host,
                now_ms,
            )
            .map_err(Into::into)
    }

    /// The coordinator's own clock, so a deadline is judged against the same epoch
    /// that records it.
    pub fn now_ms(&self) -> Result<i64, crate::fault::LifecycleFault> {
        (self.shared.clock)().map_err(Into::into)
    }

    /// Accept a deployment durably and return what was accepted.
    ///
    /// Acceptance is idempotent on the caller's key: a response lost after the
    /// record was written must not produce a second deployment when the caller
    /// retries, which the store enforces on the unique key rather than by the
    /// caller checking first.
    ///
    /// This does not start anything. A deployment exists as durable intent before
    /// any runtime does, which is what lets its id be returned before readiness.
    pub fn accept_deployment(
        &self,
        request: mllm_store::deployments::AcceptDeployment,
    ) -> Result<mllm_store::deployments::Accepted, crate::fault::LifecycleFault> {
        let owner = self.shared.owner.lock().map_err(|error| {
            drop(error);
            crate::fault::LifecycleFault::from(self.shared.fail("ownership mutex poisoned"))
        })?;
        owner.store().accept_deployment(request).map_err(Into::into)
    }

    /// Verify composition uses the worker's exact owned Store and session.
    pub fn shares_state(&self, state: &SharedCoordinatorState) -> bool {
        Arc::ptr_eq(&self.shared.owner, state)
    }

    /// Commit Stop using a service-resolved generation. History is observation only.
    ///
    /// The deployment stays eligible for on-demand activation, which is what SPEC
    /// §6.3 requires of an idle eviction.
    pub fn stop(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<OrdinaryStopReceipt, CoordinatorCommandError> {
        self.stop_inner(
            principal,
            deployment_id,
            expected_revision,
            key,
            requested_deadline_ms,
            false,
        )
    }

    /// An operator's Stop, which also suspends automatic activation (SPEC §6.3).
    pub fn administrative_stop(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<OrdinaryStopReceipt, CoordinatorCommandError> {
        self.stop_inner(
            principal,
            deployment_id,
            expected_revision,
            key,
            requested_deadline_ms,
            true,
        )
    }

    fn stop_inner(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
        administrative: bool,
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
        let now = (self.shared.clock)()?;
        let store = owner.store();
        let accept = if administrative {
            mllm_store::Store::accept_administrative_stop_command
        } else {
            mllm_store::Store::accept_ordinary_stop_command
        };
        let receipt = accept(
            store,
            owner.session(),
            principal,
            deployment_id,
            expected_revision,
            key,
            now,
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
    ) -> Result<StartReceipt, CoordinatorCommandError> {
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
            .start_command_receipt(
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
            .accept_start_command(
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
// Private bundle: the callbacks and the process tools belong to one immutable
// runtime binding, so cleanup and the failure path act on exactly the engine that
// was built for it.
struct Driver {
    engine: Arc<dyn EngineAdapter>,
    cleanup: Arc<dyn Fn(CleanupExecutionContext) -> CleanupFuture + Send + Sync>,
    /// Spec §3: the process tools this builder was given, when it owns the
    /// processes it launched. A builder that only talks to an engine somebody else
    /// started has none, and neither terminates anything.
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
}

/// Builds the process tools for one launch around the association that records its
/// API identity. The director owns both: the builder never learns where identities
/// are written (Spec §3).
pub type ToolsFactory = Arc<
    dyn Fn(Arc<dyn LaunchAssociation + Send + Sync>) -> Arc<dyn OwnedProcessLaunch> + Send + Sync,
>;

/// Records the API identity of a launch under the binding it belongs to, through
/// the coordinator's own owned state.
///
/// Spec §3: the durable launcher holds the child at a gate until this returns, so
/// a process that exists is a process mllm has on record. Anything that cannot be
/// written is uncertain, never assumed written.
struct StoreAssociation {
    owner: SharedCoordinatorState,
    fence: DeploymentFence,
    binding_id: String,
}

impl LaunchAssociation for StoreAssociation {
    fn persist_api_identity(
        &self,
        identity: &mllm_domain::completion::ProcessIdentity,
    ) -> Result<(), AssociationError> {
        let owner = self.owner.lock().map_err(|error| {
            drop(error);
            AssociationError::Uncertain("owner poisoned".into())
        })?;
        owner
            .store()
            .record_api_identity(owner.session(), &self.fence, &self.binding_id, identity)
            .map_err(|error| AssociationError::Uncertain(error.to_string()))
    }
}
/// Supplies what the lifecycle must not know: resolved credentials and the frozen
/// launch plan a binding was admitted against. Implemented by the application,
/// which owns credential storage.
pub trait EngineBindings: Send + Sync {
    fn spec(
        &self,
        work: &InitializeWork,
    ) -> Result<mllm_adapters::resolve::AdapterSpec, CoordinatorError>;

    /// The adapter one frozen binding is driven through.
    ///
    /// Resolution is the whole of it in production: the spec is built against the
    /// family the profile declares, and a spec for any other family is refused
    /// rather than quietly resolved. The step is a method rather than a fixed call
    /// so that a test can drive the lifecycle against an engine it controls without
    /// the coordinator gaining a second lane; everything before this point — the
    /// frozen plan, the stored per-launch key, the process tools — is the same work
    /// the product does.
    fn adapter(
        &self,
        declared: Engine,
        spec: mllm_adapters::resolve::AdapterSpec,
        tools: Arc<dyn OwnedProcessLaunch>,
    ) -> Result<Arc<dyn EngineAdapter>, CoordinatorError> {
        Ok(Arc::from(
            mllm_adapters::resolve::resolve(declared, spec, Some(tools)).map_err(|_| {
                CoordinatorError::Service(
                    "engine spec does not match the declared family".into(),
                )
            })?,
        ))
    }
}

/// Cleanup evidence for any engine family: the recorded processes are observed
/// gone. Anything short of every identity proven absent retains ownership, because
/// an unreadable or racing observation looks exactly like absence while the process
/// keeps holding device memory.
fn observed_gone(
    context: &CleanupExecutionContext,
    clock: &ServiceClock,
) -> Result<CleanupEvidence, CoordinatorError> {
    use mllm_launchers::process_absence::{verify_gone, GoneProof};
    let observed_at_ms =
        clock().map_err(|_| CoordinatorError::Service("cleanup clock failed".into()))?;
    match verify_gone(&context.identities) {
        GoneProof::AllGone => Ok(CleanupEvidence {
            binding_id: context.binding_id.clone(),
            incarnation: context.incarnation.clone(),
            identities: context.identities.clone(),
            observed_at_ms,
            receipt: "every recorded process observed gone".into(),
        }),
        GoneProof::SomeAlive => Err(CoordinatorError::Service(
            "a recorded process is still alive; ownership is retained".into(),
        )),
        GoneProof::Indeterminate => Err(CoordinatorError::Service(
            "process absence could not be established; ownership is retained".into(),
        )),
    }
}

/// Cleanup for a builder that owns its processes: terminate the recorded group,
/// then prove it gone the same way every other family is proved.
///
/// Spec §5: the order matters. An engine's own report that it shut down is not
/// evidence, so the proof still runs, and it runs after the signal rather than
/// instead of it. The blocking signal and wait go to a blocking thread, so a slow
/// stop never holds an async worker for the length of a grace period.
fn terminate_then_prove_gone(
    tools: Arc<dyn OwnedProcessLaunch>,
    clock: ServiceClock,
    grace: Duration,
) -> Arc<dyn Fn(CleanupExecutionContext) -> CleanupFuture + Send + Sync> {
    Arc::new(move |context| {
        let tools = tools.clone();
        let clock = clock.clone();
        Box::pin(async move {
            let identities = context.identities.clone();
            tokio::task::spawn_blocking(move || tools.terminate_owned(&identities, grace))
                .await
                .map_err(|_| CoordinatorError::Service("terminate task failed".into()))?
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            observed_gone(&context, &clock)
        })
    })
}

type DriverFactory =
    Arc<dyn Fn(&InitializeWork) -> Result<Arc<Driver>, CoordinatorError> + Send + Sync>;

impl OwnedCoordinator {
    pub fn commands(&self) -> CoordinatorCommands {
        CoordinatorCommands {
            shared: self.shared.clone(),
        }
    }

    /// Spawn a coordinator that drives whichever engine family each binding
    /// declares, rather than the single Fake lane.
    ///
    /// The coordinator deliberately does not resolve secrets or frozen launch
    /// plans; `bindings` supplies those, so credential handling stays outside the
    /// lifecycle. Resolution then rejects a spec whose family differs from the one
    /// the runtime profile declares, because a profile's fingerprint, reserved-flag
    /// policy and operational evidence are only meaningful for the engine it names.
    ///
    /// Cleanup is proved the same way for every family: the recorded identities must
    /// be observed gone. An engine's own report that it shut down is not evidence
    /// that its processes released the device.
    pub fn spawn_resolved(
        owner: SharedCoordinatorState,
        observations: Arc<dyn ServiceObservation>,
        clock: ServiceClock,
        options: CoordinatorOptions,
        bindings: Arc<dyn EngineBindings>,
        tools_factory: ToolsFactory,
    ) -> Result<Self, CoordinatorError> {
        let cleanup_clock = clock.clone();
        let grace = options.terminate_grace;
        let factory_owner = owner.clone();
        Self::spawn(
            owner,
            observations,
            clock,
            options,
            Arc::new(move |work| {
                let declared = work.effective().profile.engine;
                if work.endpoint().is_empty() || work.credential_ref().is_empty() {
                    return Err(CoordinatorError::Service(
                        "frozen binding lacks an endpoint or credential reference".into(),
                    ));
                }
                let spec = bindings.spec(work)?;
                // SPEC §13.3: the key the builder is about to use must already be
                // recoverable from the store, or a restart would leave an engine
                // running that nothing can authenticate against again.
                if let mllm_adapters::resolve::AdapterSpec::Vllm {
                    engine_key: Some(key),
                    ..
                } = &spec
                {
                    let sealed: [u8; 32] = hex::decode(key)
                        .ok()
                        .and_then(|bytes| bytes.try_into().ok())
                        .ok_or_else(|| {
                            CoordinatorError::Service("engine key is not 32 bytes".into())
                        })?;
                    let owner = factory_owner.lock().map_err(|error| {
                        drop(error);
                        CoordinatorError::Service("ownership mutex poisoned".into())
                    })?;
                    owner
                        .store()
                        .store_engine_key(work.binding_id(), work.incarnation(), &sealed)
                        .map_err(|error| {
                            CoordinatorError::Service(format!(
                                "the engine key was not stored: {error}"
                            ))
                        })?;
                }
                // Spec §3: the association is built per launch and captures this
                // binding's own fence, so an identity can only ever be recorded
                // against the launch that produced it.
                let association: Arc<dyn LaunchAssociation + Send + Sync> =
                    Arc::new(StoreAssociation {
                        owner: factory_owner.clone(),
                        fence: work.fence().clone(),
                        binding_id: work.binding_id().to_owned(),
                    });
                let tools = tools_factory(association);
                let engine = bindings.adapter(declared, spec, tools.clone())?;
                Ok(Arc::new(Driver {
                    engine,
                    cleanup: terminate_then_prove_gone(
                        tools.clone(),
                        cleanup_clock.clone(),
                        grace,
                    ),
                    tools: Some(tools),
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
        if options.poll_interval.is_zero()
            || options.poll_interval > Duration::from_secs(1)
            || options.protocol_timeout.is_zero()
            || options.protocol_timeout > Duration::from_secs(3600)
            || options.max_observers == 0
            || options.max_observers > 16384
            || options.max_attempts == 0
            || options.max_attempts > 16
            || options.retry_cooldown.is_zero()
            || options.retry_cooldown > Duration::from_secs(3600)
            // Spec §4: an Initialize bound below half a minute would fail every
            // real cold start, and one above two hours is no bound at all.
            || options.initialize_timeout < Duration::from_secs(30)
            || options.initialize_timeout > Duration::from_secs(7200)
            // Spec §5: cleanup terminates and then proves the group gone, all
            // inside `protocol_timeout`. A grace that leaves no room for the
            // proof would turn every Stop into a timeout, so it is refused here
            // rather than discovered on the first Stop.
            || options.terminate_grace < Duration::from_secs(1)
            || options.terminate_grace + Duration::from_secs(5) >= options.protocol_timeout
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
            cleanup_accepting: AtomicBool::new(true),
            shutdown_requested: AtomicBool::new(false),
            initializing: AtomicBool::new(true),
            observers: Arc::new(Semaphore::new(options.max_observers)),
            store_jobs: Arc::new(Semaphore::new(options.max_observers + 1)),
            retained: Mutex::new(BTreeMap::new()),
            options,
        });
        let (stop, stop_rx) = watch::channel(false);
        let (status_tx, status) = watch::channel(WorkerStatus::Running);
        let task_shared = shared.clone();
        let task = tokio::spawn(async move {
            let result = AssertUnwindSafe(run(
                task_shared.clone(),
                observations,
                factory,
                stop_rx,
                &status_tx,
            ))
            .catch_unwind()
            .await;
            let status = result.unwrap_or_else(|_| {
                WorkerStatus::Failed("worker panicked; durable arm retained".into())
            });
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
            .accept_start(owner.session(), fence, (self.shared.clock)()?, deadline_ms)
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
    ) -> Result<InitializeStatus, CoordinatorError> {
        tokio::time::timeout(caller_timeout, async {
            loop {
                let step = self.step_id.clone();
                let status = self
                    .shared
                    .read(move |owner, now| {
                        owner
                            .store()
                            .initialize_status(owner.session(), &step, now)
                    })
                    .await?;
                if !matches!(
                    status,
                    InitializeStatus::Planned
                        | InitializeStatus::Armed
                        | InitializeStatus::Expired
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
    fn close_admission(&self) {
        // Serialize closure with the entire command lookup/check/commit boundary.
        // Recover a poisoned guard only to close admission, never to access Store.
        let _owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        self.accepting.store(false, Ordering::Release);
        self.cleanup_accepting.store(false, Ordering::Release);
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
        let work = match shared
            .read(|owner, now| {
                owner
                    .store()
                    .next_initialize_or_expire(owner.session(), now)
            })
            .await
        {
            Ok(InitializePoll::Work(work)) => work,
            Ok(InitializePoll::ExpiredUnarmed) => {
                shared.changed.notify_waiters();
                continue;
            }
            Ok(InitializePoll::Idle) => {
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
            Ok(Ok(())) => {
                // ADR 0011 decision 5: a success resets the budget. This
                // configuration reached Ready, so the failures it took to get
                // there must not count against the next time it is started.
                let fence = work.fence().clone();
                if let Err(error) = shared
                    .with_owner(move |owner| {
                        owner
                            .store()
                            .clear_attempts(&fence)
                            .map_err(|error| CoordinatorError::Service(error.to_string()))
                    })
                    .await
                {
                    return WorkerStatus::Failed(format!(
                        "reached Ready but the attempt budget was not reset: {error}"
                    ));
                }
                shared.changed.notify_waiters();
            }
            failure => {
                shared.set_initializing(false);
                let reason = match failure {
                    Ok(Err(error)) => error.to_string(),
                    _ => "worker step panicked".into(),
                };
                // SPEC §17: failures are recorded. The match below moves `reason`
                // into the outcome, so keep a copy for the journal.
                let recorded_reason = reason.clone();
                let deployment_id = work.fence().deployment_id.clone();
                let journal_operation = work.operation_id().to_owned();
                let status_step = step_id.clone();
                // Spec §6: a builder that owns its processes can be classified by
                // proof instead of paused. Only a driver this worker retains for
                // this exact binding qualifies, and only one with process tools.
                let native = shared
                    .retained
                    .lock()
                    .ok()
                    .and_then(|retained| retained.get(work.binding_id()).cloned())
                    .filter(|driver| driver.tools.is_some());
                let is_native = native.is_some();
                let status = shared
                    .read(move |owner, now| {
                        // Keep observation and annotation under the exact owned
                        // lock. Stop cannot transfer the claim between these
                        // transactions and strand its frozen predecessor.
                        let status = owner.store().initialize_status(
                            owner.session(),
                            &status_step,
                            now,
                        )?;
                        if status == InitializeStatus::Expired {
                            owner.store().expire_unarmed_initialize(
                                owner.session(),
                                &status_step,
                                now,
                            )?;
                            Ok(InitializeStatus::ExpiredUnarmed)
                        } else if status == InitializeStatus::Armed {
                            if is_native {
                                // The annotation belongs to the settlement below,
                                // which decides between a proven release and the
                                // uncertain pause. Reporting it armed is what
                                // carries that decision out of this transaction.
                                Ok(InitializeStatus::Armed)
                            } else {
                                owner.store().mark_initialize_uncertain(
                                    owner.session(),
                                    &status_step,
                                    now,
                                )?;
                                Ok(InitializeStatus::Uncertain)
                            }
                        } else {
                            Ok(status)
                        }
                    })
                    .await;
                let mut settled_native = false;
                let status = match (status, native) {
                    (Ok(InitializeStatus::Armed), Some(driver)) => {
                        let settled = native_failure::settle_failed_native_launch(
                            &shared,
                            &driver,
                            &work,
                            &recorded_reason,
                        )
                        .await;
                        settled_native = matches!(settled, Ok(InitializeStatus::Closed));
                        settled
                    }
                    (status, _) => status,
                };
                // SPEC §13.2: only a step the store still reports as planned is a
                // failure known not to have landed. Nothing was armed, so nothing
                // can be running, and the same step is still there to be driven
                // again. Every other outcome is retained, not retried.
                let unlanded = matches!(status, Ok(InitializeStatus::Planned));
                let outcome = match status {
                    Ok(InitializeStatus::ExpiredUnarmed) => {
                        shared.set_initializing(true);
                        shared.changed.notify_waiters();
                        continue;
                    }
                    // Spec §6: the settlement terminated, proved, released, wrote
                    // the redacted reason, closed this deployment's admission and
                    // re-admitted every other one, all under a single lock. There
                    // is nothing left for this loop to decide. Falling through to
                    // the give-up branch would journal the same event a second
                    // time, unredacted and labelled as an exhausted retry budget,
                    // when ADR 0011 decision 5 says a launch that failed after arm
                    // is terminal for that start with no retry: an operator would
                    // read two different stories for one failure.
                    Ok(InitializeStatus::Closed) if settled_native => {
                        shared.changed.notify_waiters();
                        continue;
                    }
                    Ok(InitializeStatus::Uncertain) => WorkerStatus::Uncertain {
                        operation_id,
                        reason,
                    },
                    Ok(InitializeStatus::Superseded) => {
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
                    // ADR 0011 decision 4: a deployment that already closed its own
                    // admission is blocked on that closure, not superseded by
                    // someone else's work.
                    Ok(
                        InitializeStatus::Planned
                        | InitializeStatus::Expired
                        | InitializeStatus::Closed,
                    ) => WorkerStatus::Blocked {
                        operation_id,
                        reason,
                    },
                    Ok(_) => WorkerStatus::Failed(format!("{reason}; superseded work retained")),
                    Err(error) => {
                        // SPEC §17: failures are recorded. This branch may leave an
                        // armed step behind while the worker keeps running, so the
                        // reason must survive even though the annotation did not.
                        journal_failure(
                            &shared,
                            &deployment_id,
                            &journal_operation,
                            "attempt_not_annotated",
                            &redact_text(&format!(
                                "{recorded_reason}; durable arm retained if recorded: {error}"
                            )),
                        )
                        .await;
                        WorkerStatus::Failed(format!(
                            "{reason}; durable arm retained if recorded: {error}"
                        ))
                    }
                };
                // Only a process-wide condition still halts the worker: closed
                // global admission (poisoned mutex, corrupt store — store_error
                // already closed it above) or requested shutdown.
                if !shared.accepting.load(Ordering::Acquire) || *stop.borrow() {
                    return outcome;
                }
                if matches!(outcome, WorkerStatus::Uncertain { .. }) {
                    // Keep the same session and retained instance for explicit Stop.
                    // No further Initialize is admitted or discovered until this
                    // exact retained binding has a committed verified cleanup.
                    status_tx.send_replace(outcome.clone());
                    paused = Some((work.binding_id().into(), outcome));
                    shared.changed.notify_waiters();
                    continue;
                }
                // The worker is admitting Initialize again before anything about
                // this deployment is written, not after. An observer that watches
                // for the closed deployment would otherwise see it closed and still
                // be refused a start for a healthy one, which is the blast radius
                // this removes.
                shared.set_initializing(true);
                // SPEC §13.2 and ADR 0011 decision 5: a failure known not to have
                // landed is retried. The attempt is counted against this exact
                // configuration, and the deployment is given up on once the budget
                // is spent. An uncertain outcome never reaches here: it pauses
                // above and resolves through the gone-proof first.
                // SPEC §13.2 and spec §6: a builder's reason quotes the engine's
                // own output, and this journal is not the owner-only log, so every
                // reason written from here is redacted at the coordinator rather
                // than trusting whichever builder produced it to have done so.
                let mut closing = redact_text(&format!("gave up: {recorded_reason}"));
                if unlanded {
                    let fence = work.fence().clone();
                    let counting = shared.clone();
                    let attempt_operation = journal_operation.clone();
                    // SPEC §17: failures are recorded. The attempt and the reason
                    // it failed are written in the same transaction, so a counted
                    // attempt is never left without an explanation.
                    let attempt_evidence = redact_text(&format!(
                        "deployment {deployment_id}: attempt failed: {recorded_reason}"
                    ));
                    let record = match shared
                        .with_owner(move |owner| {
                            let now = (counting.clock)()?;
                            let record = owner
                                .store()
                                .record_attempt(&fence, now)
                                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
                            owner
                                .store()
                                .record_journal(
                                    None,
                                    Some(&attempt_operation),
                                    Some("attempt_failed"),
                                    &attempt_evidence,
                                )
                                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
                            Ok(record)
                        })
                        .await
                    {
                        Ok(record) => record,
                        Err(error) => {
                            return WorkerStatus::Failed(format!(
                                "{outcome:?}; attempt not recorded: {error}"
                            ))
                        }
                    };
                    if record.attempts < i64::from(shared.options.max_attempts) {
                        // The step is still planned against its original
                        // reservation, so the next poll rediscovers this exact
                        // work. Nothing is released, no epoch advances and no
                        // dispatch is replayed.
                        let exponent = u32::try_from(record.attempts.saturating_sub(1))
                            .unwrap_or(u32::MAX)
                            .min(16);
                        let wait = shared
                            .options
                            .retry_cooldown
                            .saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
                        // ADR 0011 decision 5: retries happen within the start
                        // command's deadline. Sleeping past it would only hand the
                        // step to the expiry path mid-cooldown, so a deadline that
                        // arrives before the budget is spent is terminal for this
                        // start and the deployment closes its own admission now.
                        let now = match (shared.clock)() {
                            Ok(now) => now,
                            Err(error) => {
                                return WorkerStatus::Failed(format!(
                                    "{outcome:?}; retry cooldown could not be bounded: {error}"
                                ))
                            }
                        };
                        let left = work.deadline_ms().saturating_sub(now);
                        let left = Duration::from_millis(u64::try_from(left).unwrap_or(0));
                        if left > wait {
                            status_tx.send_replace(WorkerStatus::Running);
                            shared.changed.notify_waiters();
                            tokio::select! {
                                _ = stop.changed() => {},
                                _ = tokio::time::sleep(wait.min(left)) => {},
                            }
                            continue;
                        }
                        closing = redact_text(&format!(
                            "deadline reached before the budget was spent: {recorded_reason}"
                        ));
                    }
                }
                // ADR 0011 decision 4: the budget is spent, the deadline arrived
                // first, or the failure was not one that may be replayed. A
                // deployment that is given up on closes its own admission, not the
                // host's. Every other deployment keeps being served; only this one
                // is no longer admitted until an operator reopens it.
                let closed_deployment = deployment_id.clone();
                let closing_operation = journal_operation.clone();
                if let Err(error) = shared
                    .with_owner(move |owner| {
                        owner
                            .store()
                            .set_admission_enabled(&closed_deployment, false)
                            .map_err(|error| CoordinatorError::Service(error.to_string()))?;
                        // SPEC §17: failures are recorded.
                        owner
                            .store()
                            .record_journal(
                                None,
                                Some(&closing_operation),
                                Some("given_up"),
                                &format!("deployment {closed_deployment}: {closing}"),
                            )
                            .map_err(|error| CoordinatorError::Service(error.to_string()))
                    })
                    .await
                {
                    return WorkerStatus::Failed(format!(
                        "{outcome:?}; failed to close the deployment's own admission: {error}"
                    ));
                }
                shared.changed.notify_waiters();
            }
        }
    }
}

/// Append one journal entry naming the deployment and why its attempt failed.
///
/// SPEC §17: failures are recorded. The journal is evidence only: it releases
/// nothing, advances no epoch and replays no dispatch, so a journal that cannot
/// be written must not turn a recorded failure into a different outcome.
async fn journal_failure(
    shared: &Arc<Shared>,
    deployment_id: &str,
    operation_id: &str,
    state: &str,
    reason: &str,
) {
    let evidence = format!("deployment {deployment_id}: {reason}");
    let operation = operation_id.to_owned();
    let state = state.to_owned();
    let _ = shared
        .with_owner(move |owner| {
            owner
                .store()
                .record_journal(None, Some(&operation), Some(&state), &evidence)
                .map_err(|error| CoordinatorError::Service(error.to_string()))
        })
        .await;
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
    work: &InitializeWork,
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
            owner.store().arm_initialize_with_context(
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
        || context.deadline_ms != work.deadline_ms()
        || context.launch_settings.as_ref() != Some(&work.effective().profile.launch_settings)
    {
        return Err(CoordinatorError::Service("frozen binding mismatch".into()));
    }
    let step = work.step_id().to_owned();
    let expected = context.clone();
    let ttl = shared
        .read(move |owner, now| {
            owner.store().revalidate_initialize_send(
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
    // Spec §4: Initialize is bounded by its own timeout. `protocol_timeout` bounds a
    // control call to a running engine; a cold start is not one, and giving up on a
    // loading engine after thirty seconds would kill a healthy one.
    let bound = Duration::from_millis((work.deadline_ms() - now) as u64)
        .min(shared.options.initialize_timeout);
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
        || context.mode != mllm_store::ordinary_lifecycle::cleanup::CleanupMode::TerminateOwned
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
