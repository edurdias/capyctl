use super::permits_send;
use crate::ownership::SharedCoordinatorState;
use capyctl_adapters::traits::{
    EngineAdapter, OwnedProcessLaunch, RuntimeAction, RuntimeCommand, RuntimeError,
};
use capyctl_adapters::vllm::args::redact_text;
use capyctl_config::engine_policy::Engine;
use capyctl_domain::{
    completion::{CleanupEvidence, CompletionEvidence, OwnedLaunchReceipt},
    resources::{MemoryLimit, MemoryObservation},
};
use capyctl_launchers::{AssociationError, LaunchAssociation};
use capyctl_store::ordinary_lifecycle::park::{
    PreinitializeReceipt, ResidencyArm, ResidencyKind, ResidencyReceipt, ResidencyWork, WakeScope,
};
use capyctl_store::{
    lifecycle::{DeploymentFence, LifecycleError},
    ordinary_lifecycle::cleanup::CleanupExecutionContext,
    ordinary_lifecycle::cleanup::{OrdinaryCleanupReceipt, OrdinaryCleanupStatus},
    ordinary_lifecycle::unarmed_stop::OrdinaryStopReceipt,
    ordinary_lifecycle::worker::{InitializePoll, InitializeStatus, InitializeWork},
    ordinary_lifecycle::StartReceipt,
};
use futures::FutureExt;
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

#[path = "local_adoption.rs"]
mod local_adoption;

#[path = "cleanup_adoption.rs"]
mod cleanup_adoption;

// SPEC §10, ADR 0013 §8 (W10): request-driven switching commands.
#[path = "switching.rs"]
mod switching;

// ADR 0015: per-instance concurrent lifecycle work.
#[path = "scheduler.rs"]
mod scheduler;
use scheduler::run;

#[derive(Clone, Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("coordinator stopped: {0}")]
    Stopped(String),
    /// SPEC §6.1, §13.2: new activations wait while a stop whose cleanup is
    /// not yet proven, or a launch whose outcome is uncertain, settles. The
    /// worker is running; the same command can be sent again once it settles.
    #[error("new starts are paused: {0}")]
    Paused(String),
    #[error("coordinator observer capacity exhausted")]
    Busy,
    #[error("caller observation deadline elapsed")]
    CallerTimeout,
    #[error("coordinator service failed: {0}")]
    Service(String),
    #[error("invalid coordinator options")]
    Invalid,
    /// SPEC §6.5 (W5): the step was deferred without any effect (parked
    /// instances are being reclaimed first); it stays planned and is not an
    /// attempt.
    #[error("deferred: {0}")]
    Deferred(String),
}

pub type ObservationFuture =
    Pin<Box<dyn Future<Output = Result<Vec<MemoryObservation>, CoordinatorError>> + Send>>;

/// Observations with the per-process resident memory sampled beside them.
pub type ResidentObservationFuture = Pin<
    Box<
        dyn Future<
                Output = Result<
                    (
                        Vec<MemoryObservation>,
                        Vec<capyctl_domain::resources::ProcessResident>,
                    ),
                    CoordinatorError,
                >,
            > + Send,
    >,
>;

/// Service-owned observation source. Implementations perform observations only;
/// they cannot provide owner residency credit or choose resource limits.
pub trait ServiceObservation: Send + Sync + 'static {
    fn observe(&self, host_id: String) -> ObservationFuture;

    /// ADR 0007: the observations together with the memory each process on
    /// the host held in the same sample, keyed by process identity. Only
    /// evidence: the store attributes it to owners by the identities it
    /// recorded, and credits it as a lower bound. A source that samples no
    /// processes reports none, which credits nothing.
    fn observe_with_residents(&self, host_id: String) -> ResidentObservationFuture {
        let observed = self.observe(host_id);
        Box::pin(async move { Ok((observed.await?, Vec::new())) })
    }

    /// ADR 0013 §4 step 1, W12: the hosts eligible for placement now (a live
    /// reconciled session, not draining, an approved configuration and a
    /// matching profile build). `None` when this source has no notion of
    /// eligibility (the embedded host, which is its own); every allowed host
    /// that resolved is then a candidate.
    fn eligible_hosts(&self) -> Option<std::collections::BTreeSet<String>> {
        None
    }

    /// Owner decision 2026-09-25: why each host this source knows is not
    /// eligible now, one line each for the operator (drain-only after version
    /// skew with both versions, draining, unresponsive, not reconciled, a
    /// placement requirement missing). A host absent here has no live control
    /// session. Diagnostics only: `eligible_hosts` alone decides placement.
    fn ineligible_hosts(&self) -> std::collections::BTreeMap<String, String> {
        Default::default()
    }

    /// Owner decision 4 (2026-09-22): the remote hosts reachable now (a live,
    /// reconciled session). A planned cleanup of a remote binding whose host is
    /// not among them is deferred, unarmed, until it is, instead of timing out
    /// against an offline host. `None` when this source has no notion of
    /// connectivity (the embedded host); nothing is deferred then.
    fn online_hosts(&self) -> Option<std::collections::BTreeSet<String>> {
        None
    }
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
    /// SPEC §6.5 (W5): the controller-owned idle policy. Disabled by default:
    /// a timer runs only when the server's configuration names one.
    pub idle: capyctl_store::ordinary_lifecycle::park::IdlePolicy,
    /// ADR 0015: how many activation effects (Initialize, park, restore) may be
    /// in flight at once across all instances, and separately how many
    /// cleanups. One instance never has more than one effect in flight;
    /// admission, reservations and the ledger stay serialized in the store.
    pub max_concurrent_effects: usize,
    /// Owner decision 2026-09-23: how often an Initialize samples its host's
    /// published memory availability to measure its startup peak. `None`
    /// measures nothing, and every start keeps its declared or placeholder
    /// startup budget.
    pub startup_sample_interval: Option<Duration>,
    /// SPEC §6.3 ("drain, and terminate engine workers"), §10 ("normal
    /// switches do not kill live requests"): how long a Stop's cleanup waits
    /// for the requests already accepted by its instance to complete before
    /// it terminates the engine. The operator's or policy's Stop is the
    /// authorization to terminate once the bound has passed; the requests'
    /// leases still settle on evidence. The default matches
    /// `switching.drain_timeout`.
    pub stop_drain_timeout: Duration,
}
impl Default for CoordinatorOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            protocol_timeout: Duration::from_secs(30),
            max_observers: 256,
            max_attempts: 3,
            retry_cooldown: Duration::from_secs(30),
            // ADR 0014 amendment A1: the step deadline (the deployment's
            // `timeouts.initialize` or an override) governs; this is a ceiling.
            initialize_timeout: Duration::from_secs(3600),
            terminate_grace: Duration::from_secs(15),
            idle: Default::default(),
            max_concurrent_effects: 8,
            startup_sample_interval: Some(Duration::from_secs(1)),
            stop_drain_timeout: Duration::from_secs(30),
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
    /// The service observation source, also the placement eligibility source.
    observations: Arc<dyn ServiceObservation>,
    wake: Notify,
    changed: Notify,
    accepting: AtomicBool,
    cleanup_accepting: AtomicBool,
    shutdown_requested: AtomicBool,
    /// Admission was closed by a service fault (a store fault, a poisoned
    /// mutex, a refused binding), not by shutdown. A task that fails after it
    /// reports the worker failed even though the scheduler, leaving on the
    /// closed admission, has meanwhile signalled every task to stop.
    faulted: AtomicBool,
    initializing: AtomicBool,
    /// Whether this worker adopts retired sessions' remote launches at startup.
    adopt_remote: bool,
    /// SPEC §4.3 (P3): how an embedded worker rebuilds the runtime of a Ready
    /// launch its retired session left running, so the restarted role re-attaches
    /// it instead of stranding it. Only the resolved embedded worker has one.
    local_adoption: Option<DriverFactory>,
    observers: Arc<Semaphore>,
    store_jobs: Arc<Semaphore>,
    retained: Mutex<BTreeMap<String, Arc<Driver>>>,
    /// ADR 0015: uncertain launches, by binding, that pause new activations
    /// until each is settled on evidence. Changed only under the owner mutex
    /// (lock order: owner, then this), which is where admission reads the
    /// `initializing` flag it drives.
    paused: Mutex<BTreeMap<String, Paused>>,
    /// SPEC §6.5 (W5): the last deferral reason of each deferred start, by
    /// binding, and how many times in a row it repeated.
    deferrals: Mutex<BTreeMap<String, (String, u32)>>,
    /// SPEC §6.5 (W5): the router's last request time per instance incarnation
    /// (`generation` -1: the deployment, instance unknown). Ephemeral: after a
    /// restart idleness counts from the worker's start, never from before it.
    activity: Mutex<BTreeMap<(String, i64), i64>>,
    /// When this worker started, on its own clock: the idle timers' floor.
    started_ms: i64,
    /// When the idle policy last ran.
    idle_checked_ms: std::sync::atomic::AtomicI64,
    /// When cancelling leases were last checked against engine quiescence.
    cancel_checked_ms: std::sync::atomic::AtomicI64,
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

/// The deadline an instance Stop is accepted with.
#[derive(Clone, Copy)]
enum StopDeadline {
    /// The caller's own deadline, sent as is.
    Exact(i64),
    /// SPEC §4.3: an explicit drain's bound, lowered per instance.
    DrainBound(i64),
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
        query: impl FnOnce(&capyctl_store::Store) -> Result<T, capyctl_store::StoreError>,
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
    ) -> Result<
        std::sync::MutexGuard<'_, crate::ownership::OwnedCoordinatorState>,
        crate::fault::LifecycleFault,
    > {
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
        host: &capyctl_config::effective::HostPolicy,
        observations: &[capyctl_domain::resources::MemoryObservation],
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
        capyctl_store::managed_configuration::ManagedConfigurationReceipt,
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
        request: capyctl_store::deployments::AcceptDeployment,
    ) -> Result<capyctl_store::deployments::Accepted, crate::fault::LifecycleFault> {
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

    /// Owner decision Q7: stop one instance with verified cleanup. `None` when
    /// the instance holds no runtime (recording the operator's stop was the
    /// whole effect). An ordinary stop: the deployment stays eligible for
    /// on-demand activation of its other instances (SPEC §6.3).
    pub fn stop_instance(
        &self,
        principal: &str,
        deployment_id: &str,
        instance: u32,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<Option<OrdinaryStopReceipt>, CoordinatorCommandError> {
        self.stop_instance_inner(
            principal,
            deployment_id,
            instance,
            expected_revision,
            key,
            StopDeadline::Exact(requested_deadline_ms),
        )
    }

    /// SPEC §4.3: the Stop an explicit drain issues for one instance. The
    /// drain's `bound_ms` is lowered to the instance's own request-deadline
    /// window (SPEC §6: no operation's deadline lies beyond it), and a retry
    /// under the same `key` replays the deadline first accepted
    /// ([`capyctl_store::Store::drain_stop_deadline`]). Otherwise as
    /// [`CoordinatorCommands::stop_instance`].
    pub fn drain_stop_instance(
        &self,
        principal: &str,
        deployment_id: &str,
        instance: u32,
        expected_revision: i64,
        key: &str,
        bound_ms: i64,
    ) -> Result<Option<OrdinaryStopReceipt>, CoordinatorCommandError> {
        self.stop_instance_inner(
            principal,
            deployment_id,
            instance,
            expected_revision,
            key,
            StopDeadline::DrainBound(bound_ms),
        )
    }

    fn stop_instance_inner(
        &self,
        principal: &str,
        deployment_id: &str,
        instance: u32,
        expected_revision: i64,
        key: &str,
        deadline: StopDeadline,
    ) -> Result<Option<OrdinaryStopReceipt>, CoordinatorCommandError> {
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
        // Resolved under the ownership lock, so a concurrent retry of the same
        // drain sees the Stop this one accepts and replays its deadline.
        let requested_deadline_ms = match deadline {
            StopDeadline::Exact(deadline) => deadline,
            StopDeadline::DrainBound(bound) => {
                let now = (self.shared.clock)()?;
                owner
                    .store()
                    .drain_stop_deadline(
                        owner.session(),
                        principal,
                        deployment_id,
                        instance,
                        key,
                        now,
                        bound,
                    )
                    .map_err(store_error)?
            }
        };
        if let Some(receipt) = owner
            .store()
            .instance_stop_command_receipt(
                owner.session(),
                principal,
                deployment_id,
                instance,
                expected_revision,
                key,
                requested_deadline_ms,
            )
            .map_err(store_error)?
        {
            return Ok(Some(receipt));
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(CoordinatorError::Stopped("worker is not admitting cleanup".into()).into());
        }
        let now = (self.shared.clock)()?;
        let receipt = owner
            .store()
            .accept_instance_stop_command(
                owner.session(),
                principal,
                deployment_id,
                instance,
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

    /// SPEC §6.3 (W6): `delete deployment` removes a deployment whose cleanup
    /// is already verified, in one store transaction. Nothing is executed and no accounting is
    /// released: a deployment still holding a runtime, reservation, lease or
    /// unresolved step on any instance is refused (`RuntimeRetained`), and the
    /// operator stops it first. An exact retry returns the original receipt.
    pub fn delete(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<capyctl_store::delete::DeleteReceipt, CoordinatorCommandError> {
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
        let now = (self.shared.clock)()?;
        owner
            .store()
            .accept_delete_command(
                owner.session(),
                principal,
                deployment_id,
                expected_revision,
                key,
                now,
                requested_deadline_ms,
            )
            .map_err(|error| {
                if matches!(
                    error,
                    LifecycleError::Sql(_) | LifecycleError::CorruptStoredData
                ) {
                    self.shared.fail_locked(&owner, error.to_string());
                }
                CoordinatorCommandError::Lifecycle(error)
            })
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
            capyctl_store::Store::accept_administrative_stop_command
        } else {
            capyctl_store::Store::accept_ordinary_stop_command
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
    ///
    /// Owner decision Q5 (ADR 0013 §9): an explicit start targets every
    /// instance of the deployment, each placed on an eligible allowed host.
    pub fn start(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<StartReceipt, CoordinatorCommandError> {
        self.start_scoped(
            principal,
            deployment_id,
            capyctl_store::ordinary_lifecycle::placement::StartScope::All,
            expected_revision,
            key,
            requested_deadline_ms,
        )
    }

    /// Owner decision Q5: on-demand activation brings up one instance (the
    /// lowest-index one the operator did not stop that fits) and the rest only
    /// where they fit now; nothing is evicted.
    pub fn start_on_demand(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<StartReceipt, CoordinatorCommandError> {
        self.start_scoped(
            principal,
            deployment_id,
            capyctl_store::ordinary_lifecycle::placement::StartScope::OnDemand,
            expected_revision,
            key,
            requested_deadline_ms,
        )
    }

    /// Owner decision Q7: `start instance <n>`, placed like any other start.
    pub fn start_instance(
        &self,
        principal: &str,
        deployment_id: &str,
        instance: u32,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<StartReceipt, CoordinatorCommandError> {
        self.start_scoped(
            principal,
            deployment_id,
            capyctl_store::ordinary_lifecycle::placement::StartScope::Instance(instance),
            expected_revision,
            key,
            requested_deadline_ms,
        )
    }

    fn start_scoped(
        &self,
        principal: &str,
        deployment_id: &str,
        scope: capyctl_store::ordinary_lifecycle::placement::StartScope,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<StartReceipt, CoordinatorCommandError> {
        let instance = match scope {
            capyctl_store::ordinary_lifecycle::placement::StartScope::Instance(k) => Some(k),
            _ => None,
        };
        // Read before the owner lock: the source keeps its own state.
        let eligible = self.shared.observations.eligible_hosts();
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
            .scoped_start_command_receipt(
                owner.session(),
                principal,
                deployment_id,
                instance,
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
            return Err(self
                .shared
                .not_initializing("worker is not admitting Initialize")
                .into());
        }
        let receipt = owner
            .store()
            .accept_scoped_start_command(
                owner.session(),
                principal,
                deployment_id,
                scope,
                expected_revision,
                key,
                (self.shared.clock)()?,
                requested_deadline_ms,
                eligible.as_ref(),
            )
            .map_err(store_error)?;
        drop(owner);
        self.shared.wake.notify_one();
        Ok(receipt)
    }

    /// SPEC §6.3 `park deployment` (W5): drain and park every READY instance
    /// at the declared tier. Refused (`Unsupported`) for a restart-only
    /// deployment or a host that opted out of deep parking.
    pub fn park(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<ResidencyReceipt, CoordinatorCommandError> {
        self.residency_command(false, |owner, now| {
            owner.store().accept_park_command(
                owner.session(),
                principal,
                deployment_id,
                expected_revision,
                key,
                now,
                requested_deadline_ms,
            )
        })
    }

    /// SPEC §6.3 `start deployment` on parked instances, and on-demand
    /// activation (owner decision Q5): restore the parked instances `scope`
    /// names in place, on the host each parked on. `None` when none is
    /// parked, so the caller starts cold instead.
    pub fn wake(
        &self,
        principal: &str,
        deployment_id: &str,
        scope: WakeScope,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<Option<ResidencyReceipt>, CoordinatorCommandError> {
        self.residency_command(true, |owner, now| {
            owner.store().accept_restore_command(
                owner.session(),
                principal,
                deployment_id,
                scope,
                expected_revision,
                key,
                now,
                requested_deadline_ms,
            )
        })
    }

    /// SPEC §6.3, §6.5 `preinitialize deployment` (W5): start, verify and
    /// park each instance in turn. Refused (`Unsupported`) for a
    /// restart-only deployment: it must fail rather than claim it prewarmed.
    pub fn preinitialize(
        &self,
        principal: &str,
        deployment_id: &str,
        expected_revision: i64,
        key: &str,
        requested_deadline_ms: i64,
    ) -> Result<PreinitializeReceipt, CoordinatorCommandError> {
        self.residency_command(true, |owner, now| {
            owner.store().accept_preinitialize_command(
                owner.session(),
                principal,
                deployment_id,
                expected_revision,
                key,
                now,
                requested_deadline_ms,
            )
        })
    }

    /// SPEC §6.5 (W5): the router saw a request for this instance (or, with
    /// no generation, for the deployment); the ready-idle timer restarts.
    pub fn note_activity(&self, deployment_id: &str, generation: Option<i64>) {
        let Ok(now) = (self.shared.clock)() else {
            return;
        };
        let Ok(mut activity) = self.shared.activity.lock() else {
            return;
        };
        // Bounded: entries a day old say nothing an idle timer needs.
        if activity.len() >= 4096 {
            activity.retain(|_, at| now.saturating_sub(*at) < 86_400_000);
            if activity.len() >= 4096 {
                activity.clear();
            }
        }
        activity.insert((deployment_id.to_owned(), generation.unwrap_or(-1)), now);
    }

    /// One residency command, accepted synchronously under the owned store.
    /// `increases` refuses it while the worker admits no Initialize (it waits
    /// on an uncertain launch), exactly as a start is refused then.
    fn residency_command<T>(
        &self,
        increases: bool,
        accept: impl FnOnce(&crate::ownership::OwnedCoordinatorState, i64) -> Result<T, LifecycleError>,
    ) -> Result<T, CoordinatorCommandError> {
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
        if !self.shared.accepting.load(Ordering::Acquire)
            || (increases && !self.shared.initializing.load(Ordering::Acquire))
        {
            return Err(self
                .shared
                .not_initializing("worker is not admitting this command")
                .into());
        }
        let now = (self.shared.clock)()?;
        let receipt = accept(&owner, now).map_err(|error| {
            if matches!(
                error,
                LifecycleError::Sql(_) | LifecycleError::CorruptStoredData
            ) {
                self.shared.fail_locked(&owner, error.to_string());
            }
            CoordinatorCommandError::Lifecycle(error)
        })?;
        drop(owner);
        self.shared.wake.notify_one();
        Ok(receipt)
    }
}

pub type CleanupFuture =
    Pin<Box<dyn Future<Output = Result<CleanupEvidence, CoordinatorError>> + Send>>;
/// Host-specific cleanup must return authenticated physical evidence. An adapter
/// shutdown acknowledgement or a local `/proc` lookup of a remote PID is not proof.
pub type CleanupExecutor = Arc<dyn Fn(CleanupExecutionContext) -> CleanupFuture + Send + Sync>;

/// SPEC §§3, 13: execution transport belongs below the existing durable lifecycle.
/// Construction supplies no arm/send authority; the worker still owns admission,
/// persistence, fencing, dispatch, completion, and uncertainty accounting.
pub struct ExecutionBinding {
    engine: Arc<dyn EngineAdapter>,
    cleanup: CleanupExecutor,
    settle: Option<SettlementExecutor>,
}
impl ExecutionBinding {
    /// Remote bindings deliberately have no local process tools. Their cleanup
    /// callback must establish absence on the authenticated assigned host.
    pub fn remote(engine: Arc<dyn EngineAdapter>, cleanup: CleanupExecutor) -> Self {
        Self {
            engine,
            cleanup,
            settle: None,
        }
    }

    /// SPEC §§6, 13.2: how a launch that failed or went uncertain after arm is
    /// settled when this binding has no local process tools. Without one, such
    /// a launch pauses uncertain until an operator Stop resolves it.
    pub fn with_settlement(mut self, settle: SettlementExecutor) -> Self {
        self.settle = Some(settle);
        self
    }
}

/// One armed launch whose outcome was lost, as a settlement must act on it.
///
/// `identities` are exactly what the binding recorded, which is the set the
/// returned evidence must prove gone; the store refuses any other set. The
/// deadline bounds the settlement's own control, not the launch's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementContext {
    pub fence: DeploymentFence,
    pub operation_id: String,
    pub step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<capyctl_domain::completion::ProcessIdentity>,
    pub deadline_ms: i64,
}

/// Settles a failed remote launch: terminate it on the authenticated host and
/// return that host's evidence that the owned launch is gone. An error, or no
/// answer, retains everything (SPEC §6.1).
pub type SettlementExecutor = Arc<dyn Fn(SettlementContext) -> CleanupFuture + Send + Sync>;
pub trait ExecutionBindings: Send + Sync {
    fn resolve(&self, work: &InitializeWork) -> Result<ExecutionBinding, CoordinatorError>;
}
impl<F> ExecutionBindings for F
where
    F: Fn(&InitializeWork) -> Result<ExecutionBinding, CoordinatorError> + Send + Sync,
{
    fn resolve(&self, work: &InitializeWork) -> Result<ExecutionBinding, CoordinatorError> {
        self(work)
    }
}

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
    /// SPEC §13.2: a remote builder's way to settle a failed launch through its
    /// authenticated host, in place of local process tools.
    settle: Option<SettlementExecutor>,
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
/// a process that exists is a process capyctl has on record. Anything that cannot be
/// written is uncertain, never assumed written.
struct StoreAssociation {
    owner: SharedCoordinatorState,
    fence: DeploymentFence,
    binding_id: String,
}

impl LaunchAssociation for StoreAssociation {
    fn persist_api_identity(
        &self,
        identity: &capyctl_domain::completion::ProcessIdentity,
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
    ) -> Result<capyctl_adapters::resolve::AdapterSpec, CoordinatorError>;

    /// The adapter one frozen binding is driven through.
    ///
    /// Resolution is the whole of it in production: the spec is built against the
    /// family the profile declares, and a spec for any other family is refused
    /// rather than quietly resolved. The step is a method rather than a fixed call
    /// so that a test can drive the lifecycle against an engine it controls without
    /// the coordinator gaining a second lane; everything before this point — the
    /// frozen plan, the stored per-launch key, the process tools — is the same work
    /// the product does.
    /// ADR 0014 §7 (WE3): the verifier that measures this host's checkpoints,
    /// when the bindings build engines that read one. A builder that drives no
    /// real engine (a test Fake) reads no checkpoint and has none.
    fn checkpoint_verifier(&self) -> Option<Arc<capyctl_agent::checkpoint::CheckpointVerifier>> {
        None
    }

    /// SPEC §8.2 / T21: called once the launch `incarnation`'s recorded
    /// processes are proved gone, so per-launch host state (its rendezvous
    /// directory) can be removed on verified evidence only.
    fn launch_gone(&self, _incarnation: &str) {}

    /// ADR 0008 (owner decision 2026-09-23): the engine installation the
    /// embedded host registered for `work`'s profile, measured again before
    /// each Initialize for drift. Bindings that keep no registration have none.
    fn installation(
        &self,
        _work: &InitializeWork,
    ) -> Option<Arc<crate::installation_gate::EmbeddedInstallation>> {
        None
    }

    fn adapter(
        &self,
        declared: Engine,
        spec: capyctl_adapters::resolve::AdapterSpec,
        tools: Arc<dyn OwnedProcessLaunch>,
    ) -> Result<Arc<dyn EngineAdapter>, CoordinatorError> {
        Ok(Arc::from(
            capyctl_adapters::resolve::resolve(declared, spec, Some(tools)).map_err(|error| {
                // The mapping must keep the real cause: a family mismatch, a
                // frozen-shape validation refusal and a construction failure
                // all land here, and "family" alone misdiagnoses a validation
                // refusal as a wiring bug (found live: an SGLang start failed
                // three times on a shape the journal could not name).
                CoordinatorError::Service(match error {
                    RuntimeError::Unsupported => {
                        "engine spec does not match the declared family".to_owned()
                    }
                    other => other.to_string(),
                })
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
    use capyctl_launchers::process_absence::{verify_gone, GoneProof};
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
    bindings: Arc<dyn EngineBindings>,
) -> Arc<dyn Fn(CleanupExecutionContext) -> CleanupFuture + Send + Sync> {
    Arc::new(move |context| {
        let tools = tools.clone();
        let clock = clock.clone();
        let bindings = bindings.clone();
        Box::pin(async move {
            let identities = context.identities.clone();
            tokio::task::spawn_blocking(move || tools.terminate_owned(&identities, grace))
                .await
                .map_err(|_| CoordinatorError::Service("terminate task failed".into()))?
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            let evidence = observed_gone(&context, &clock)?;
            // SPEC §8.2 / T21: only after every recorded process is proved
            // gone is the launch's per-launch host state removed.
            bindings.launch_gone(&context.incarnation);
            Ok(evidence)
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

    /// SPEC §§3, 13: share the same lifecycle worker and Store for host-scoped
    /// execution. Never instantiate local launchers or infer remote process absence.
    pub fn spawn_with_execution_bindings(
        owner: SharedCoordinatorState,
        observations: Arc<dyn ServiceObservation>,
        clock: ServiceClock,
        options: CoordinatorOptions,
        bindings: Arc<dyn ExecutionBindings>,
    ) -> Result<Self, CoordinatorError> {
        Self::spawn_with(
            owner,
            observations,
            clock,
            options,
            Arc::new(move |work| {
                let binding = bindings.resolve(work)?;
                Ok(Arc::new(Driver {
                    engine: binding.engine,
                    cleanup: binding.cleanup,
                    tools: None,
                    settle: binding.settle,
                }))
            }),
            true,
            None,
        )
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
        let adoption = local_adoption::factory(
            owner.clone(),
            bindings.clone(),
            tools_factory.clone(),
            clock.clone(),
            grace,
        );
        Self::spawn_with(
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
                let mut spec = bindings.spec(work)?;
                // SPEC §13.3: the keys the builder is about to use must already
                // be recoverable from the store, or a restart would leave an
                // engine running that nothing can authenticate against again.
                // The factory runs under the owner, so the same transaction
                // point is where the SGLang launch scope learns the session ULID
                // the private descriptor must name.
                {
                    let owner = factory_owner.lock().map_err(|error| {
                        drop(error);
                        CoordinatorError::Service("ownership mutex poisoned".into())
                    })?;
                    let store = owner.store();
                    if matches!(
                        spec,
                        capyctl_adapters::resolve::AdapterSpec::Tensorfold { .. }
                    ) {
                        let recorded = store.retained_processes().map_err(|error| {
                            CoordinatorError::Service(format!(
                                "retained launches could not be read: {error}"
                            ))
                        })?;
                        crate::engine_bindings::clear_stale_build_locks(&mut spec, &recorded);
                    }
                    match &mut spec {
                        capyctl_adapters::resolve::AdapterSpec::Vllm {
                            engine_key: Some(key),
                            admin_key,
                            ..
                        } => {
                            // SPEC §9.1 / T21, ADR 0012: the admin role is sealed
                            // beside the inference role, before the builder runs,
                            // so an adopted engine can still be parked and woken.
                            // The two keys must differ, or the guard would admit
                            // the inference key on the control routes.
                            if admin_key.as_deref() == Some(key.as_str()) {
                                return Err(CoordinatorError::Service(
                                    "the admin and inference engine keys are the same".into(),
                                ));
                            }
                            let roles =
                                std::iter::once((
                                    key.as_str(),
                                    capyctl_store::secrets::SecretRole::Inference,
                                ))
                                .chain(admin_key.as_deref().map(
                                    |admin| (admin, capyctl_store::secrets::SecretRole::Admin),
                                ));
                            for (key, role) in roles {
                                let sealed: [u8; 32] = hex::decode(key)
                                    .ok()
                                    .and_then(|bytes| bytes.try_into().ok())
                                    .ok_or_else(|| {
                                        CoordinatorError::Service(format!(
                                            "the {role:?} engine key is not 32 bytes"
                                        ))
                                    })?;
                                store
                                    .store_engine_key(
                                        work.binding_id(),
                                        work.incarnation(),
                                        &sealed,
                                        role,
                                    )
                                    .map_err(|error| {
                                        CoordinatorError::Service(format!(
                                            "the engine key was not stored: {error}"
                                        ))
                                    })?;
                            }
                        }
                        capyctl_adapters::resolve::AdapterSpec::Sglang {
                            inference,
                            admin,
                            session,
                            ..
                        } => {
                            *session = Some(owner.session().id().to_owned());
                            for (key, role) in [
                                (
                                    inference.as_str(),
                                    capyctl_store::secrets::SecretRole::Inference,
                                ),
                                (admin.as_str(), capyctl_store::secrets::SecretRole::Admin),
                            ] {
                                let sealed: [u8; 32] = hex::decode(key)
                                    .ok()
                                    .and_then(|bytes| bytes.try_into().ok())
                                    .ok_or_else(|| {
                                        CoordinatorError::Service(format!(
                                            "the {role:?} engine key is not 32 bytes"
                                        ))
                                    })?;
                                store
                                    .store_engine_key(
                                        work.binding_id(),
                                        work.incarnation(),
                                        &sealed,
                                        role,
                                    )
                                    .map_err(|error| {
                                        CoordinatorError::Service(format!(
                                            "the engine key was not stored: {error}"
                                        ))
                                    })?;
                            }
                        }
                        _ => {}
                    }
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
                // ADR 0014 §7 (WE3): the embedded host verifies the checkpoint
                // against the recorded digest before Initialize and Restore,
                // exactly as a remote host agent does.
                let engine = match bindings.checkpoint_verifier() {
                    Some(checkpoints) => crate::checkpoint_digests::CheckpointGate::new(
                        engine,
                        factory_owner.clone(),
                        checkpoints,
                        work,
                    ) as Arc<dyn EngineAdapter>,
                    None => engine,
                };
                // SPEC §§6.2, 9.1 / ADR 0008: a residency the engine cannot
                // honor is refused here, by the decision a host agent makes,
                // after the drift check and before the checkpoint is measured.
                let engine = crate::capability_gate::CapabilityGate::wrap(engine, work.effective());
                // ADR 0008: outermost, so a drifted installation under
                // `installation_drift: refuse` is refused before anything else.
                let engine = match bindings.installation(work) {
                    Some(installation) => crate::installation_gate::InstallationGate::new(
                        engine,
                        factory_owner.clone(),
                        installation,
                        work,
                    ) as Arc<dyn EngineAdapter>,
                    None => engine,
                };
                Ok(Arc::new(Driver {
                    engine,
                    cleanup: terminate_then_prove_gone(
                        tools.clone(),
                        cleanup_clock.clone(),
                        grace,
                        bindings.clone(),
                    ),
                    tools: Some(tools),
                    settle: None,
                }))
            }),
            false,
            Some(adoption),
        )
    }

    /// A worker over a bare driver factory, with no adoption of either kind.
    #[cfg(test)]
    fn spawn(
        owner: SharedCoordinatorState,
        observations: Arc<dyn ServiceObservation>,
        clock: ServiceClock,
        options: CoordinatorOptions,
        factory: DriverFactory,
    ) -> Result<Self, CoordinatorError> {
        Self::spawn_with(owner, observations, clock, options, factory, false, None)
    }

    /// `adopt_remote` lets this worker adopt retired sessions' remote launches
    /// at startup (SPEC §13.2). `local_adoption` lets an embedded worker adopt
    /// the Ready embedded launches its retired session left running (SPEC §4.3,
    /// P3); their dispatch stays closed until the local readiness supervisor
    /// collects fresh local proof.
    fn spawn_with(
        owner: SharedCoordinatorState,
        observations: Arc<dyn ServiceObservation>,
        clock: ServiceClock,
        options: CoordinatorOptions,
        factory: DriverFactory,
        adopt_remote: bool,
        local_adoption: Option<DriverFactory>,
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
            // SPEC §6.5: an idle timer, when named, is a positive duration.
            || options.idle.ready_idle_ms.is_some_and(|ms| ms <= 0)
            || options.idle.parked_idle_ms.is_some_and(|ms| ms <= 0)
            // ADR 0015: at least one effect at a time, and a bound.
            // Owner decision 2026-09-23: a sampling period, when named, is
            // positive.
            || options.startup_sample_interval.is_some_and(|d| d.is_zero())
            || options.max_concurrent_effects == 0
            || options.max_concurrent_effects > 256
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
        // SPEC §6.5: idleness is never counted from before this worker ran.
        let started_ms = clock().unwrap_or(0);
        let shared = Arc::new(Shared {
            owner,
            observations: observations.clone(),
            clock,
            wake: Notify::new(),
            changed: Notify::new(),
            accepting: AtomicBool::new(true),
            cleanup_accepting: AtomicBool::new(true),
            shutdown_requested: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
            initializing: AtomicBool::new(true),
            adopt_remote,
            local_adoption,
            observers: Arc::new(Semaphore::new(options.max_observers)),
            store_jobs: Arc::new(Semaphore::new(options.max_observers + 1)),
            retained: Mutex::new(BTreeMap::new()),
            paused: Mutex::new(BTreeMap::new()),
            deferrals: Mutex::new(BTreeMap::new()),
            activity: Mutex::new(BTreeMap::new()),
            started_ms,
            idle_checked_ms: std::sync::atomic::AtomicI64::new(i64::MIN),
            cancel_checked_ms: std::sync::atomic::AtomicI64::new(i64::MIN),
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
        self.shared
            .shutdown_requested
            .store(true, Ordering::Release);
        self.shared.close_admission();
        self.stop.send_replace(true);
        let status = self
            .task
            .take()
            .expect("owned task")
            .await
            .map_err(|_| CoordinatorError::Stopped("worker task failed".into()));
        // ADR 0015 invariant 6: a cancelled task can leave a Store job queued
        // on the blocking pool, holding the owned state. Wait for every such
        // job, so a returned shutdown holds nothing (a restart in the same
        // process can open the state at once). A closed queue has none left.
        let all = u32::try_from(self.shared.options.max_observers + 1).unwrap_or(u32::MAX);
        let _ = self.shared.store_jobs.acquire_many(all).await;
        status
    }
}
impl Drop for OwnedCoordinator {
    fn drop(&mut self) {
        self.shared
            .shutdown_requested
            .store(true, Ordering::Release);
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
                        owner.store().initialize_status(owner.session(), &step, now)
                    })
                    .await?;
                if !matches!(
                    status,
                    InitializeStatus::Planned | InitializeStatus::Armed | InitializeStatus::Expired
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
    /// Why an activation is not admitted, read under the owner mutex: a
    /// running worker that holds activations for an unproven stop or an
    /// uncertain launch is `Paused` (retry once it settles); otherwise the
    /// worker is closing or closed (`Stopped`).
    fn not_initializing(&self, stopped: &str) -> CoordinatorError {
        if self.accepting.load(Ordering::Acquire)
            && !self.shutdown_requested.load(Ordering::Acquire)
            && !self.paused_is_empty()
        {
            return CoordinatorError::Paused(
                "a stop or launch is not yet confirmed on its host".into(),
            );
        }
        CoordinatorError::Stopped(stopped.into())
    }

    fn close_admission(&self) {
        // Serialize closure with the entire command lookup/check/commit boundary.
        // Recover a poisoned guard only to close admission, never to access Store.
        let _owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        self.accepting.store(false, Ordering::Release);
        self.cleanup_accepting.store(false, Ordering::Release);
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
        self.faulted.store(true, Ordering::Release);
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
            // Dropped before the permit, so a waiter that holds every permit
            // (shutdown) knows this job no longer holds the owned state.
            let shared = shared;
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

/// A retained uncertain binding this worker waits on before admitting
/// Initialize (ADR 0015: one entry per paused binding).
struct Paused {
    binding: String,
    status: WorkerStatus,
    /// A remote launch the worker keeps settling, and when it next tries.
    retry: Option<(native_failure::LaunchRef, tokio::time::Instant)>,
}
impl Paused {
    fn due(&self) -> Option<native_failure::LaunchRef> {
        self.retry
            .as_ref()
            .filter(|(_, at)| tokio::time::Instant::now() >= *at)
            .map(|(launch, _)| launch.clone())
    }
    fn retry_later(&mut self, after: Duration) {
        if let Some((_, at)) = self.retry.as_mut() {
            *at = tokio::time::Instant::now() + after;
        }
    }
}

/// SPEC §17: what a reconciliation pass did is recorded, one entry each.
async fn journal_reconciled(
    shared: &Arc<Shared>,
    done: &[capyctl_store::ordinary_lifecycle::reconcile::Reconciled],
) {
    use capyctl_store::ordinary_lifecycle::reconcile::Reconciled;
    let entries: Vec<(Option<String>, &'static str, String)> = done
        .iter()
        .map(|action| match action {
            Reconciled::Stopped { deployment_id, instance, operation_id, reason } => (
                Some(operation_id.clone()),
                "instance_stop_requested",
                format!("deployment {deployment_id}: instance {instance} stops ({reason}); cleanup completes only on gone evidence"),
            ),
            Reconciled::Started { deployment_id, instance, operation_id } => (
                Some(operation_id.clone()),
                "instance_start_placed",
                format!("deployment {deployment_id}: pending instance {instance} was placed and started"),
            ),
            Reconciled::Deferred { deployment_id, instance, code } => (
                None,
                "instance_start_deferred",
                format!("deployment {deployment_id}: instance {instance} fits no allowed host yet ({code})"),
            ),
            Reconciled::Expired { deployment_id, instance } => (
                None,
                "instance_start_expired",
                format!("deployment {deployment_id}: instance {instance} was not placed before its start deadline"),
            ),
            Reconciled::Retired { deployment_id, instance } => (
                None,
                "instance_retired",
                format!("deployment {deployment_id}: retired instance {instance} holds nothing and was removed"),
            ),
        })
        .collect();
    let _ = shared
        .with_owner(move |owner| {
            for (operation, state, evidence) in &entries {
                owner
                    .store()
                    .record_journal(None, operation.as_deref(), Some(state), evidence)
                    .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            }
            Ok(())
        })
        .await;
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

/// The first device domain of `controls` with no observation in `observed`.
fn unobserved_device<'a>(
    controls: &'a capyctl_config::resource_controls::ResourceControls,
    observed: &[MemoryObservation],
) -> Option<&'a str> {
    controls
        .domains
        .iter()
        .filter(|(_, domain)| domain.memory == capyctl_config::effective::DomainMemory::Device)
        .map(|(id, _)| id.as_str())
        .find(|id| !observed.iter().any(|o| o.domain == *id))
}

fn fresh(observations: &[MemoryObservation], now: i64, ttl: i64) -> bool {
    !observations.is_empty()
        && observations.len() <= 1024
        && ttl > 0
        && observations
            .iter()
            .all(|o| o.sampled_at_ms >= 0 && o.sampled_at_ms <= now && now - o.sampled_at_ms <= ttl)
}

/// Owner decision 2026-09-23: the startup peak of one Initialize, measured as
/// the largest drop in its host's published availability of the domain its
/// startup reservation charges, below what was available when it armed.
///
/// ADR 0014 amendment A12: samples are kept until the engine answers, so that
/// those taken while it reported a kernel build can be left out.
struct StartupPeak {
    domain: Option<String>,
    baseline: Option<(i64, i64)>,
    /// `(sampled_at_ms, available_bytes)`, one per distinct host sample.
    samples: Vec<(i64, i64)>,
}

/// Samples one Initialize keeps. The coordinator's 3600 s ceiling at the
/// default 1 s interval stays below it; later samples are dropped.
const MAX_STARTUP_SAMPLES: usize = 8192;

impl StartupPeak {
    fn new(work: &InitializeWork, baseline: &[MemoryObservation]) -> Self {
        let domain = match work.effective().resources.cold.allocations.as_slice() {
            [allocation] => Some(allocation.domain.clone()),
            _ => None,
        };
        let baseline = domain.as_ref().and_then(|domain| {
            baseline
                .iter()
                .find(|o| &o.domain == domain)
                .map(|o| (o.available_bytes, o.sampled_at_ms))
        });
        Self {
            domain,
            baseline,
            samples: Vec::new(),
        }
    }

    /// Take one sample. Only an observation sampled after the baseline counts.
    fn sample(&mut self, observed: &[MemoryObservation]) {
        let (Some(domain), Some((_, since))) = (&self.domain, self.baseline) else {
            return;
        };
        if let Some(o) = observed
            .iter()
            .find(|o| &o.domain == domain && o.sampled_at_ms > since && o.available_bytes >= 0)
        {
            // A host that has not published since the last tick repeats it.
            if self.samples.last() == Some(&(o.sampled_at_ms, o.available_bytes))
                || self.samples.len() >= MAX_STARTUP_SAMPLES
            {
                return;
            }
            self.samples.push((o.sampled_at_ms, o.available_bytes));
        }
    }

    /// The measured peak, when a fresh sample outside every kernel build the
    /// engine reported showed availability dropping. ADR 0014 amendment A12:
    /// a compiler's memory is the build's, not the engine's.
    fn peak(&self, builds: &[capyctl_domain::completion::KernelBuild]) -> Option<i64> {
        let (available, _) = self.baseline?;
        let lowest = self
            .samples
            .iter()
            .filter(|(at, _)| !builds.iter().any(|build| build.covers(*at)))
            .map(|(_, bytes)| *bytes)
            .min()?;
        available.checked_sub(lowest).filter(|drop| *drop > 0)
    }
}

async fn drive(
    shared: &Arc<Shared>,
    work: &InitializeWork,
    source: &dyn ServiceObservation,
    factory: &DriverFactory,
    stop: &mut watch::Receiver<bool>,
    contended: &AtomicBool,
) -> Result<(), CoordinatorError> {
    let bound = remaining(shared, work.deadline_ms())?;
    let (observed, residents) = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown before arm".into())),
        result = tokio::time::timeout(bound, source.observe_with_residents(work.effective().host.name.clone())) => result.map_err(|_| CoordinatorError::Service("observation timeout".into()))??,
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
    // SPEC §7.2 / ADR 0019 (T29): a device domain whose GPU could not be read
    // has no observation. Its memory is unknown, so admission closes there;
    // it is never admitted against host RAM, and nothing reserved is released.
    if let Some(domain) = unobserved_device(&work.policy().controls, &observed) {
        return Err(CoordinatorError::Service(format!(
            "device_unobserved: device memory domain {domain} has no observation"
        )));
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
            reserve_absorbs_unmanaged: d.memory == capyctl_config::effective::DomainMemory::Device,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    let step = work.step_id().to_owned();
    let ttl = controls.observation_ttl_ms;
    let max_parked = controls.max_parked as usize;
    // SPEC §6.5 (W5): a start that does not fit first reclaims the least
    // recently parked instances on its host; it waits, planned, meanwhile.
    let (reclaim_step, reclaim_observed, reclaim_limits, reclaim_residents) = (
        step.clone(),
        observed.clone(),
        limits.clone(),
        residents.clone(),
    );
    if let Some(reason) = shared
        .read(move |owner, now| {
            owner.store().reclaim_for_start_with_residents(
                owner.session(),
                &reclaim_step,
                capyctl_scheduler::residency::AdmissionContext::new(
                    &reclaim_observed,
                    &reclaim_limits,
                    now,
                    ttl,
                    max_parked,
                ),
                &reclaim_residents,
            )
        })
        .await?
    {
        return Err(CoordinatorError::Deferred(reason));
    }
    let arm_observed = observed.clone();
    if *stop.borrow() || !shared.accepting.load(Ordering::Acquire) {
        return Err(CoordinatorError::Stopped("shutdown before arm".into()));
    }
    remaining(shared, work.deadline_ms())?;
    let (result, context) = shared
        .read(move |owner, now| {
            owner.store().arm_initialize_with_residents(
                owner.session(),
                &step,
                capyctl_scheduler::residency::AdmissionContext::new(
                    &arm_observed,
                    &limits,
                    now,
                    ttl,
                    max_parked,
                ),
                &residents,
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
        || context.launch_settings.as_ref() != Some(&work.effective().engine_config)
    {
        return Err(CoordinatorError::Service("frozen binding mismatch".into()));
    }
    let step = work.step_id().to_owned();
    let expected = context.clone();
    let ttl = shared
        .read(move |owner, now| {
            owner
                .store()
                .revalidate_initialize_send(owner.session(), &step, &expected, now)
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
    // Owner decision 2026-09-23: sample the host's published availability
    // while the engine starts, to measure its startup peak. Sampling never
    // decides anything about this Initialize.
    let mut peak = StartupPeak::new(work, &observed);
    let host = work.effective().host.name.clone();
    let interval = shared.options.startup_sample_interval;
    let protocol_timeout = shared.options.protocol_timeout;
    let sampling = async {
        let Some(interval) = interval else {
            return std::future::pending::<()>().await;
        };
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Ok(Ok(observed)) =
                tokio::time::timeout(protocol_timeout, source.observe(host.clone())).await
            {
                peak.sample(&observed);
            }
        }
    };
    // No child task is spawned: timeout/cancellation drops this effect future
    // before recording uncertainty, so an effect task cannot outlive ownership.
    let observation = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown during Initialize".into())),
        result = tokio::time::timeout(bound, driver.engine.execute_persisted(&command)) => result.map_err(|_| CoordinatorError::Service("Initialize timeout".into()))?.map_err(|e| CoordinatorError::Service(e.to_string()))?,
        _ = sampling => unreachable!("sampling never ends"),
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
    let builds = observation.kernel_builds.clone();
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
        .await?;
    // Owner decision 2026-09-23: a peak measured while nothing else changed
    // the host's memory (no other activation or cleanup on it) is recorded
    // for later starts of this revision there. A failure to record it costs
    // nothing but the measurement.
    if let Some(peak) = peak
        .peak(&builds)
        .filter(|_| !contended.load(Ordering::Acquire))
    {
        let step = work.step_id().to_owned();
        let _ = shared
            .read(move |owner, now| {
                owner
                    .store()
                    .record_startup_peak(owner.session(), &step, peak, now)
            })
            .await;
    }
    Ok(())
}

/// SPEC §§6.3, 9.1, 10, 13 (W5): arm and send one planned park or restore.
/// Returns whether anything changed. A step whose runtime this worker does not
/// retain, whose host cannot be observed now, that is still draining or that
/// does not fit yet stays planned for a later pass (its deadline closes it
/// without effect). ADR 0015: each runs on its instance's own task.
async fn drive_residency(
    shared: &Arc<Shared>,
    source: &dyn ServiceObservation,
    work: ResidencyWork,
    stop: &mut watch::Receiver<bool>,
) -> Result<bool, CoordinatorError> {
    if *stop.borrow() || !shared.accepting.load(Ordering::Acquire) {
        return Err(CoordinatorError::Stopped(
            "shutdown before a residency arm".into(),
        ));
    }
    let driver = shared
        .retained
        .lock()
        .map_err(|_| shared.fail("runtime registry poisoned"))?
        .get(&work.binding_id)
        .cloned();
    let Some(driver) = driver else {
        return Ok(false);
    };
    let now = (shared.clock)()?;
    let bound =
        Duration::from_millis(u64::try_from(work.deadline_ms.saturating_sub(now)).unwrap_or(0))
            .min(shared.options.protocol_timeout);
    if bound.is_zero() {
        return Ok(false);
    }
    let (observed, residents) = tokio::select! {
        biased;
        _ = stop.changed() => return Err(CoordinatorError::Stopped("shutdown before a residency arm".into())),
        result = tokio::time::timeout(bound, source.observe_with_residents(work.host.clone())) => match result {
            Ok(Ok(observed)) => observed,
            _ => return Ok(false),
        },
    };
    let (step, limits, ttl, max_parked) = (
        work.step_id.clone(),
        work.limits.clone(),
        work.observation_ttl_ms,
        work.max_parked,
    );
    let armed = shared
        .read(move |owner, now| {
            owner.store().arm_residency_with_residents(
                owner.session(),
                &step,
                capyctl_scheduler::residency::AdmissionContext::new(
                    &observed, &limits, now, ttl, max_parked,
                ),
                &residents,
            )
        })
        .await;
    let context = match armed {
        Ok(ResidencyArm::New(context)) => *context,
        Ok(ResidencyArm::Reclaiming(_) | ResidencyArm::Refused(_)) => return Ok(true),
        Ok(ResidencyArm::Draining) => return Ok(false),
        Ok(ResidencyArm::Blocked(why)) => {
            log_blocked_residency(&work.step_id, &why);
            return Ok(false);
        }
        Err(error) => {
            if !shared.accepting.load(Ordering::Acquire) {
                return Err(error);
            }
            return Ok(false);
        }
    };
    // SPEC §10 step 5: an embedded engine must be quiescent before it is
    // parked (a remote host checks its own ingress and gauges, W4). The
    // check is a read: an engine that is not, or cannot say, refuses the
    // park before anything is sent, and the launch serves again.
    if work.kind == ResidencyKind::Park && driver.tools.is_some() {
        let member = capyctl_adapters::traits::MemberRef {
            deployment_id: work.deployment_id.clone(),
            member_id: work.binding_id.clone(),
        };
        let refusal = match driver.engine.prepare_park(&member).await {
            Ok(quiescence) if quiescence.quiescent => None,
            Err(capyctl_adapters::traits::AdapterError::UnsupportedCapability) => None,
            Ok(_) => Some("the engine is not quiescent".to_string()),
            Err(error) => Some(format!("engine quiescence is unknown: {error}")),
        };
        if let Some(reason) = refusal {
            let step = work.step_id.clone();
            shared
                .read(move |owner, _| {
                    owner
                        .store()
                        .refuse_residency(owner.session(), &step, &redact_text(&reason))
                })
                .await?;
            return Ok(true);
        }
    }
    execute_residency(shared, &driver, &work, context, stop).await?;
    if work.kind == ResidencyKind::Park {
        measure_parked(shared, source, &work, stop).await;
    }
    Ok(true)
}

/// ADR 0014 amendment A13: once a park completed, sample the host and record
/// what the parked processes still hold, so later parks of the revision are
/// charged it instead of the placeholder. Only a sample taken after the park
/// counts; a host whose report predates it is asked once more. A failure costs
/// nothing but the measurement.
async fn measure_parked(
    shared: &Arc<Shared>,
    source: &dyn ServiceObservation,
    work: &ResidencyWork,
    stop: &mut watch::Receiver<bool>,
) {
    let Ok(since) = (shared.clock)() else {
        return;
    };
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::select! {
                biased;
                _ = stop.changed() => return,
                _ = tokio::time::sleep(PARKED_SAMPLE_RETRY) => {}
            }
        }
        let sampled = tokio::select! {
            biased;
            _ = stop.changed() => return,
            result = tokio::time::timeout(
                shared.options.protocol_timeout,
                source.observe_with_residents(work.host.clone()),
            ) => result,
        };
        let Ok(Ok((observed, residents))) = sampled else {
            return;
        };
        if observed.iter().any(|o| o.sampled_at_ms < since) {
            continue;
        }
        let step = work.step_id.clone();
        let _ = shared
            .read(move |owner, now| {
                owner.store().record_parked_residue(
                    owner.session(),
                    &step,
                    &observed,
                    &residents,
                    since,
                    now,
                )
            })
            .await;
        return;
    }
}

/// How long a host whose report predates a park is given before it is asked
/// again for the parked residue.
const PARKED_SAMPLE_RETRY: Duration = Duration::from_secs(3);

/// Send one armed park or restore exactly once and record what it proved.
async fn execute_residency(
    shared: &Arc<Shared>,
    driver: &Arc<Driver>,
    work: &ResidencyWork,
    context: capyctl_domain::completion::StepExecutionContext,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let now = (shared.clock)()?;
    // A restore reloads weights and probes the model: bounded by its own
    // deadline (the deployment's wake window), under the Initialize ceiling.
    let bound =
        Duration::from_millis(u64::try_from(context.deadline_ms.saturating_sub(now)).unwrap_or(0))
            .min(shared.options.initialize_timeout);
    let outcome = tokio::select! {
        biased;
        _ = stop.changed() => None,
        result = tokio::time::timeout(bound, residency_effect(driver, work.kind, context)) => Some(result),
    };
    let step = work.step_id.clone();
    let verb = match work.kind {
        ResidencyKind::Park => "park",
        ResidencyKind::Restore => "restore",
    };
    let uncertain = |reason: String| {
        let step = step.clone();
        async move {
            shared
                .read(move |owner, _| {
                    owner.store().mark_residency_uncertain(
                        owner.session(),
                        &step,
                        &redact_text(&reason),
                    )
                })
                .await
                .map(|_| ())
        }
    };
    match outcome {
        None => {
            uncertain(format!("the controller stopped during the {verb}")).await?;
            return Err(CoordinatorError::Stopped(format!(
                "shutdown during a {verb}"
            )));
        }
        Some(Err(_)) => uncertain(format!("the {verb} outlived its deadline")).await?,
        Some(Ok(Ok(observation))) => {
            let step = step.clone();
            let completed = shared
                .read(move |owner, now| {
                    owner
                        .store()
                        .complete_residency(owner.session(), &step, &observation, now)
                })
                .await;
            if let Err(error) = completed {
                if !shared.accepting.load(Ordering::Acquire) {
                    return Err(error);
                }
                uncertain(format!("its evidence was not accepted: {error}")).await?;
            }
        }
        // SPEC §13 (W4): refused before any effect: settled at once.
        Some(Ok(Err(
            error @ (RuntimeError::Unsupported
            | RuntimeError::Refused(_)
            | RuntimeError::StaleRevision
            | RuntimeError::Missing),
        ))) => {
            let reason = redact_text(&error.to_string());
            let step = step.clone();
            shared
                .read(move |owner, _| {
                    owner
                        .store()
                        .refuse_residency(owner.session(), &step, &reason)
                })
                .await?;
        }
        Some(Ok(Err(RuntimeError::Uncertain(reason)))) => uncertain(reason).await?,
        // A park or restore never launches; an engine reported gone here is
        // an unexplained effect, so it stays uncertain with its accounting.
        Some(Ok(Err(error @ RuntimeError::LaunchFailed(_)))) => {
            uncertain(error.to_string()).await?
        }
    }
    Ok(())
}

/// The engine effect of one park or restore. A remote host runs the whole
/// restore contract as one command (W4) and answers with all four facts; an
/// embedded vLLM engine runs SPEC §9.1's steps one persisted call each, then a
/// fresh model probe, and any failure after the first effect is uncertain,
/// never retried (T20). An embedded group must still be every recorded engine
/// process (SPEC §13.2); a helper may have exited (ADR 0027).
async fn residency_effect(
    driver: &Arc<Driver>,
    kind: ResidencyKind,
    context: capyctl_domain::completion::StepExecutionContext,
) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
    use capyctl_domain::completion::Milestone;
    let action = match kind {
        ResidencyKind::Park => RuntimeAction::Park,
        ResidencyKind::Restore => RuntimeAction::Restore,
    };
    let mut observation = driver
        .engine
        .execute_persisted(&RuntimeCommand {
            action,
            context: context.clone(),
        })
        .await?;
    if kind == ResidencyKind::Restore && observation.facts != kind.facts() {
        if observation.facts != [Milestone::AllocationsRestored] {
            return Err(RuntimeError::Uncertain(
                "the restore reported unexpected evidence".into(),
            ));
        }
        for (next, suffix) in [
            (RuntimeAction::ReloadWeights, "reload"),
            (RuntimeAction::InvalidateCache, "cache"),
            (RuntimeAction::Probe, "probe"),
        ] {
            let mut sub = context.clone();
            sub.token.step_id = format!("{}:{suffix}", context.token.step_id);
            let step = driver
                .engine
                .execute_persisted(&RuntimeCommand {
                    action: next,
                    context: sub,
                })
                .await
                .map_err(|error| match error {
                    RuntimeError::Uncertain(reason) => RuntimeError::Uncertain(reason),
                    other => RuntimeError::Uncertain(format!(
                        "the restore stopped after its first effect: {other}"
                    )),
                })?;
            observation.facts.extend(step.facts);
            observation.observed_at_ms = step.observed_at_ms;
            observation.receipt = step.receipt;
        }
        observation.token = context.token.clone();
    }
    if driver.tools.is_some() {
        // ADR 0027: the engine's own processes; a helper may have exited, and
        // the evidence then names only what is alive.
        observation.identities.retain(|identity| {
            !identity.is_helper()
                || capyctl_launchers::process_absence::presence(identity)
                    == capyctl_domain::completion::Presence::Alive
        });
        for identity in &observation.identities {
            if capyctl_launchers::process_absence::presence(identity)
                != capyctl_domain::completion::Presence::Alive
            {
                return Err(RuntimeError::Uncertain(format!(
                    "recorded {} process {} is not the one alive",
                    identity.role, identity.pid
                )));
            }
        }
    }
    Ok(observation)
}

/// SPEC §6.5 (W5): advance open preinitializes and, at most once a second,
/// the idle policy. Returns whether anything was accepted.
async fn residency_policy(shared: &Arc<Shared>) -> Result<bool, CoordinatorError> {
    if !shared.initializing.load(Ordering::Acquire) {
        return Ok(false);
    }
    let eligible = shared.observations.eligible_hosts();
    let progress = shared
        .read(move |owner, now| {
            owner
                .store()
                .advance_preinitialize(owner.session(), now, eligible.as_ref())
        })
        .await?;
    let mut changed = !progress.is_empty();
    let idle = shared.options.idle;
    if idle.ready_idle_ms.is_none() && idle.parked_idle_ms.is_none() {
        return Ok(changed);
    }
    let now = (shared.clock)()?;
    let last = shared.idle_checked_ms.load(Ordering::Acquire);
    if last != i64::MIN && now.saturating_sub(last) < 1_000 {
        return Ok(changed);
    }
    shared.idle_checked_ms.store(now, Ordering::Release);
    let activity = shared
        .activity
        .lock()
        .map_err(|_| shared.fail("activity registry poisoned"))?
        .clone();
    let floor = shared.started_ms;
    let actions = shared
        .read(move |owner, now| {
            let last = |deployment: &str, generation: i64| {
                [generation, -1]
                    .iter()
                    .filter_map(|g| activity.get(&(deployment.to_owned(), *g)).copied())
                    .max()
            };
            owner
                .store()
                .apply_idle_policy(owner.session(), now, idle, &last, floor)
        })
        .await?;
    changed |= !actions.is_empty();
    Ok(changed)
}

/// The receipt a local engine's quiescence leaves on a cancelling lease.
const LOCAL_CANCELLATION_RECEIPT: &str =
    "the engine's own counters read no running and no waiting request after the client hung up";
/// The receipt a remote host's load report leaves on a cancelling lease.
const REMOTE_CANCELLATION_RECEIPT: &str = "the host reported the launch's engine with 0 running and 0 waiting and 0 in flight at its ingress after the client hung up";
/// One quiescence question may take no longer than this.
const QUIESCENCE_QUESTION_BOUND: Duration = Duration::from_secs(3);

fn lease_store_error(error: capyctl_store::StoreError) -> LifecycleError {
    match error {
        capyctl_store::StoreError::Sql(error) => LifecycleError::Sql(error),
        _ => LifecycleError::CorruptStoredData,
    }
}

/// The longest a binding waits before it is asked again after its engine
/// left a quiescence question unanswered.
const QUIESCENCE_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// After `timeouts` unanswered questions in a row, how long the binding waits
/// before it is asked again: one second, doubling, up to the cap.
fn quiescence_backoff(timeouts: u32) -> Duration {
    Duration::from_secs(1)
        .saturating_mul(
            1u32.checked_shl(timeouts.saturating_sub(1).min(16))
                .unwrap_or(u32::MAX),
        )
        .min(QUIESCENCE_BACKOFF_CAP)
}

/// One quiescence question's answer, applied by the scheduler on a later pass.
struct QuiescenceAnswer {
    binding_id: String,
    /// The newest hang-up the question was about; only leases cancelled at
    /// or before it close.
    cutoff_ms: i64,
    remote: bool,
    /// `None` when the engine did not answer within the bound.
    quiescent: Option<bool>,
}

/// SPEC §10 (amended 2026-10-01): at most every 250 ms, the retained bindings
/// with a cancelling lease, each with its driver. Reading them waits for no
/// engine.
async fn cancelling_drivers(
    shared: &Arc<Shared>,
) -> Result<
    Vec<(
        capyctl_store::request_lease_cancellations::CancellingBinding,
        Arc<Driver>,
    )>,
    CoordinatorError,
> {
    // The lease ledger stamps a cancellation on the wall clock, so the
    // questions are paced on that same clock.
    let now = capyctl_protocol::now_unix_ms();
    let last = shared.cancel_checked_ms.load(Ordering::Acquire);
    if last != i64::MIN && now.saturating_sub(last) < 250 {
        return Ok(vec![]);
    }
    shared.cancel_checked_ms.store(now, Ordering::Release);
    let cancelling = shared
        .read(|owner, _| {
            owner
                .store()
                .cancelling_bindings(owner.session())
                .map_err(lease_store_error)
        })
        .await?;
    if cancelling.is_empty() {
        return Ok(vec![]);
    }
    let retained = shared
        .retained
        .lock()
        .map_err(|_| shared.fail("runtime registry poisoned"))?;
    Ok(cancelling
        .into_iter()
        .filter_map(|c| retained.get(&c.binding_id).cloned().map(|d| (c, d)))
        .collect())
}

/// SPEC §10 (amended 2026-10-01): ask one engine, bounded, whether it has
/// been quiescent since the binding's newest hang-up. Quiescence is
/// engine-wide: another request on the engine keeps the cancelled one charged.
async fn ask_quiescence(
    cancelling: capyctl_store::request_lease_cancellations::CancellingBinding,
    driver: Arc<Driver>,
) -> QuiescenceAnswer {
    let member = capyctl_adapters::traits::MemberRef {
        deployment_id: cancelling.deployment_id.clone(),
        member_id: cancelling.binding_id.clone(),
    };
    // A remote sample is clamped to the time it reached the controller, so
    // the question is about the hang-up, not about the time it is asked.
    let cutoff_ms = cancelling.newest_cancelled_at_ms;
    let quiescent = tokio::time::timeout(
        QUIESCENCE_QUESTION_BOUND,
        driver.engine.engine_quiescent(&member, cutoff_ms),
    )
    .await
    .ok();
    QuiescenceAnswer {
        binding_id: cancelling.binding_id,
        cutoff_ms,
        remote: driver.settle.is_some(),
        quiescent,
    }
}

/// Close the binding's leases cancelled at or before the answer's cutoff.
/// Returns whether any closed.
async fn settle_quiescent(
    shared: &Arc<Shared>,
    answer: QuiescenceAnswer,
) -> Result<bool, CoordinatorError> {
    let receipt = if answer.remote {
        REMOTE_CANCELLATION_RECEIPT
    } else {
        LOCAL_CANCELLATION_RECEIPT
    };
    let settled = shared
        .read(move |owner, _| {
            owner
                .store()
                .settle_cancelled_leases(
                    owner.session(),
                    &answer.binding_id,
                    answer.cutoff_ms,
                    receipt,
                )
                .map_err(lease_store_error)
        })
        .await?;
    Ok(settled > 0)
}

/// SPEC §6.3: a Stop drains before it terminates. Admission to the instance
/// closed when the Stop was accepted; this waits, bounded by
/// `stop_drain_timeout` and by the time the cleanup itself still needs before
/// its deadline, for the requests already accepted to complete (their leases
/// close). SPEC §10: live requests are not killed while the bound runs. When it
/// passes with requests still charged, the Stop terminates anyway: an explicit
/// or policy Stop is the separate authorization force termination needs. Their
/// leases keep their accounting until they settle on evidence.
async fn drain_before_terminate(
    shared: &Arc<Shared>,
    work: &OrdinaryCleanupReceipt,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let now = (shared.clock)()?;
    // Leave the cleanup its protocol bound before its deadline.
    let budget = u64::try_from(work.deadline_ms.saturating_sub(now).saturating_sub(
        i64::try_from(shared.options.protocol_timeout.as_millis()).unwrap_or(i64::MAX),
    ))
    .unwrap_or(0);
    let bound = shared
        .options
        .stop_drain_timeout
        .min(Duration::from_millis(budget));
    let until = tokio::time::Instant::now() + bound;
    let poll = shared.options.poll_interval.min(Duration::from_millis(100));
    loop {
        let binding = work.binding_id.clone();
        let outstanding = shared
            .read(move |owner, _| owner.store().binding_outstanding_leases(&binding))
            .await?
            .unwrap_or(0);
        if outstanding == 0 || tokio::time::Instant::now() >= until {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = stop.changed() => {
                return Err(CoordinatorError::Stopped("shutdown while draining before cleanup".into()))
            }
            _ = tokio::time::sleep(poll) => {}
        }
    }
}

/// Spec §5, ADR 0023 §6: after the drain, an engine with its own work counters
/// (TensorFold) is read before the stop signal. Idle, exited or not listening,
/// it is signalled at once; hung, once the bound passes. Still answering busy
/// at the bound, nothing is sent: the cleanup stays uncertain and keeps its
/// accounting.
async fn idle_before_terminate(
    shared: &Arc<Shared>,
    work: &OrdinaryCleanupReceipt,
    driver: &Driver,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    let binding = work.binding_id.clone();
    let deployment_id = shared
        .read(move |owner, _| owner.store().binding_lane(&binding))
        .await?
        .map(|(deployment, _)| deployment)
        .unwrap_or_default();
    let member = capyctl_adapters::traits::MemberRef {
        deployment_id,
        member_id: work.binding_id.clone(),
    };
    // An engine without its own counters answers `None` at once; the lease
    // drain above alone decides for it.
    let now = (shared.clock)()?;
    let budget = u64::try_from(work.deadline_ms.saturating_sub(now).saturating_sub(
        i64::try_from(shared.options.protocol_timeout.as_millis()).unwrap_or(i64::MAX),
    ))
    .unwrap_or(0);
    let idle = capyctl_adapters::tensorfold::wait_idle(
        driver.engine.as_ref(),
        &member,
        Duration::from_millis(budget),
        crate::supervised::cancelled(stop),
    )
    .await;
    if idle {
        Ok(())
    } else if *stop.borrow() {
        Err(CoordinatorError::Stopped(
            "shutdown while waiting for the engine to be idle".into(),
        ))
    } else {
        Err(CoordinatorError::Service(
            "the engine still reports work in flight; the stop was not sent".into(),
        ))
    }
}

async fn drive_cleanup(
    shared: &Arc<Shared>,
    work: &OrdinaryCleanupReceipt,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), CoordinatorError> {
    // ADR 0015: the scheduler hands a cleanup to its instance's lane only after
    // the predecessor's effect task has exited. Generation fencing or claim
    // handoff never cancels that future.
    let driver = shared
        .retained
        .lock()
        .map_err(|_| shared.fail("runtime registry poisoned"))?
        .get(&work.binding_id)
        .cloned()
        .ok_or_else(|| {
            // Review finding: not a shutdown. The cleanup is unclassifiable
            // here; its binding stays uncertain and pauses activations.
            CoordinatorError::Service("original runtime is not retained by this worker".into())
        })?;
    remaining(shared, work.deadline_ms)?;
    if *stop.borrow() || !shared.accepting.load(Ordering::Acquire) {
        return Err(CoordinatorError::Stopped(
            "shutdown before cleanup arm".into(),
        ));
    }
    drain_before_terminate(shared, work, stop).await?;
    idle_before_terminate(shared, work, &driver, stop).await?;
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
        || context.mode != capyctl_store::ordinary_lifecycle::cleanup::CleanupMode::TerminateOwned
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

/// Found live 2026-09-23 (matrix M33): a wake held for capacity waited out its
/// whole deadline with no trace of why. The reason (a fixed resource-policy
/// message, never a credential) is logged once per step and reason.
fn log_blocked_residency(step_id: &str, why: &str) {
    static SEEN: std::sync::OnceLock<Mutex<std::collections::BTreeSet<(String, String)>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()));
    let Ok(mut seen) = seen.lock() else {
        return;
    };
    if seen.len() > 4096 {
        seen.clear();
    }
    if seen.insert((step_id.to_owned(), why.to_owned())) {
        capyctl_domain::role_log::event(
            serde_json::json!({"event": "residency_blocked", "step": step_id, "reason": why}),
        );
    }
}
