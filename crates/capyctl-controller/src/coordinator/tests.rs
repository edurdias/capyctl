use super::*;
use capyctl_adapters::traits::*;
use capyctl_domain::resources::ResourcePhase;
use std::sync::{atomic::AtomicI64, Mutex};

#[path = "tests_cleanup.rs"]
mod cleanup;

#[path = "tests_start_command.rs"]
mod start_command;

#[path = "tests_remote.rs"]
mod remote;

#[path = "tests_residency.rs"]
mod residency;

#[path = "tests_concurrency.rs"]
mod concurrency;

// Owner decision 2026-09-23: the startup memory budget and per-host gate.
#[path = "tests_startup.rs"]
mod startup;

// W10: request-driven switching.
#[path = "tests_switching.rs"]
mod switching_tests;

use capyctl_testkit::{fixture, FakeEngine};

struct Observations(Vec<MemoryObservation>);
impl ServiceObservation for Observations {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

async fn setup() -> (
    tempfile::TempDir,
    SharedCoordinatorState,
    DeploymentFence,
    Vec<MemoryObservation>,
) {
    use std::os::unix::fs::PermissionsExt;
    let source = fixture::owned_source().await;
    let fence = source.fence.clone();
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    (dir, owner, fence, source.observations.clone())
}

struct Gate {
    engine: FakeEngine,
    calls: Mutex<Vec<RuntimeAction>>,
    entered: Semaphore,
    release: Semaphore,
    active: AtomicBool,
    panic: bool,
    association: Mutex<Option<SharedCoordinatorState>>,
    /// Record only the API identity, as a durable launcher does the moment the
    /// process exists, instead of the full association a completed launch writes.
    api_only: AtomicBool,
    lost_reply: AtomicBool,
    /// The builder's own reason for failing, once it has done whatever it does.
    failure: Mutex<Option<String>>,
}
impl Gate {
    fn new(panic: bool) -> Arc<Self> {
        Arc::new(Self {
            engine: FakeEngine::with_lifecycle(),
            calls: Mutex::new(vec![]),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            active: AtomicBool::new(false),
            panic,
            association: Mutex::new(None),
            api_only: AtomicBool::new(false),
            lost_reply: AtomicBool::new(false),
            failure: Mutex::new(None),
        })
    }
    async fn entered(&self) {
        // Real qualification proof fans out across independent copied stores.
        // This hang detector is not the service clock or protocol deadline.
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(60), self.entered.acquire())
            .await
            .unwrap_or_else(|error| panic!("Initialize gate wait {:?}: {error}", started.elapsed()))
            .unwrap()
            .forget();
        eprintln!("Initialize gate wait: {:?}", started.elapsed());
    }
}
struct Active<'a>(&'a AtomicBool);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl EngineAdapter for Gate {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        self.calls.lock().unwrap().push(command.action);
        self.active.store(true, Ordering::SeqCst);
        let _active = Active(&self.active);
        let result = self.engine.execute_persisted(command).await;
        if let (Some(owner), Ok(observation)) = (&*self.association.lock().unwrap(), &result) {
            let o = owner.lock().unwrap();
            if self.api_only.load(Ordering::SeqCst) {
                // Spec §3: the launcher persists the API identity before the
                // engine has answered anything; no association exists yet.
                o.store()
                    .record_api_identity(
                        o.session(),
                        &DeploymentFence {
                            deployment_id: command.context.token.deployment_id.clone(),
                            revision: command.context.token.revision,
                            generation: command.context.token.generation,
                        },
                        &observation.binding_id,
                        &observation.identities[0],
                    )
                    .unwrap();
            } else {
                o.store()
                    .record_owned_launch(
                        o.session(),
                        &command.context.token.step_id,
                        &OwnedLaunchReceipt {
                            binding_id: observation.binding_id.clone(),
                            incarnation: observation.incarnation.clone(),
                            identities: observation.identities.clone(),
                            observed_at_ms: observation.observed_at_ms,
                            receipt: observation.receipt.clone(),
                        },
                        1900,
                    )
                    .unwrap();
            }
        }
        self.entered.add_permits(1);
        assert!(!self.panic, "injected adapter panic after effect");
        self.release.acquire().await.unwrap().forget();
        if let Some(reason) = self.failure.lock().unwrap().clone() {
            return Err(RuntimeError::Uncertain(reason));
        }
        if self.lost_reply.load(Ordering::SeqCst) {
            return Err(RuntimeError::Uncertain(
                "lost Initialize reply after associated launch".into(),
            ));
        }
        result
    }
    async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn cancel_work(
        &self,
        _: &MemberRef,
        _: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
}

/// A coordinator that drives the Fake engine on every binding.
///
/// This is what `OwnedCoordinator::spawn_fake` was before the Fake left the
/// product: the driver factory is the seam, and the engine behind it now comes
/// from the testkit. Passing here is never qualification of an engine recipe.
fn spawn_fake(
    owner: SharedCoordinatorState,
    observations: Arc<dyn ServiceObservation>,
    clock: ServiceClock,
    options: CoordinatorOptions,
) -> Result<OwnedCoordinator, CoordinatorError> {
    let observation_clock = clock.clone();
    OwnedCoordinator::spawn(
        owner,
        observations,
        clock,
        options,
        Arc::new(move |work| {
            if work.endpoint().is_empty() || work.credential_ref().is_empty() {
                return Err(CoordinatorError::Service(
                    "frozen binding lacks an endpoint or credential reference".into(),
                ));
            }
            let clock = observation_clock.clone();
            let engine = Arc::new(FakeEngine::with_lifecycle_clock(Arc::new(move || {
                clock()
                    .map_err(|_| RuntimeError::Uncertain("service observation clock failed".into()))
            })));
            let cleanup = engine.clone();
            Ok(Arc::new(Driver {
                engine,
                cleanup: Arc::new(move |context| {
                    let engine = cleanup.clone();
                    Box::pin(async move {
                        engine
                            .lifecycle_cleanup_observed(
                                &context.binding_id,
                                &context.incarnation,
                                &context.identities,
                            )
                            .map_err(|e| CoordinatorError::Service(e.to_string()))
                    })
                }),
                // The Fake launches nothing, so there is nothing to terminate.
                tools: None,
                settle: None,
            }))
        }),
    )
}

fn worker(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    gate: Arc<Gate>,
    options: CoordinatorOptions,
) -> OwnedCoordinator {
    OwnedCoordinator::spawn(
        owner,
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        options,
        Arc::new(move |_| Ok(test_driver(gate.clone()))),
    )
    .unwrap()
}
fn test_driver(gate: Arc<Gate>) -> Arc<Driver> {
    let cleanup = gate.clone();
    Arc::new(Driver {
        engine: gate,
        cleanup: Arc::new(move |context| {
            let gate = cleanup.clone();
            Box::pin(async move {
                gate.engine
                    .lifecycle_cleanup(
                        &context.binding_id,
                        &context.incarnation,
                        &context.identities,
                        true,
                        1900,
                    )
                    .map_err(|e| CoordinatorError::Service(e.to_string()))
            })
        }),
        // This builder launches nothing, so it has nothing to terminate: the
        // failure path pauses uncertain for it, as it does for the Fake.
        tools: None,
        settle: None,
    })
}
async fn stopped(worker: &OwnedCoordinator) -> WorkerStatus {
    let started = std::time::Instant::now();
    let status = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = worker.status();
            if status != WorkerStatus::Running {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("worker status wait {:?}: {error}", started.elapsed()));
    eprintln!("worker status wait: {:?}", started.elapsed());
    status
}
fn assert_peak(owner: &SharedCoordinatorState, fence: &DeploymentFence) {
    let o = owner.lock().unwrap();
    assert_eq!(
        o.store().resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Cold
    );
    assert_eq!(
        o.store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
}

#[tokio::test]
async fn dropped_waiters_and_caller_timeout_do_not_cancel_or_hold_store() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = Arc::new(worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    ));
    let first = w.clone();
    let second = w.clone();
    let first_fence = fence.clone();
    let second_fence = fence.clone();
    let (a, b) = tokio::join!(
        tokio::task::spawn_blocking(move || first.start(&first_fence, 10000)),
        tokio::task::spawn_blocking(move || second.start(&second_fence, 10000)),
    );
    let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
    let w = Arc::try_unwrap(w).unwrap_or_else(|_| panic!("unexpected coordinator owner"));
    assert_eq!(a.operation_id(), b.operation_id());
    gate.entered().await;
    assert!(matches!(
        a.wait(Duration::from_millis(10)).await,
        Err(CoordinatorError::CallerTimeout)
    ));
    let step = a.step_id().to_owned();
    drop(a);
    drop(b);
    // The engine is suspended here: acquiring Store and writing a transaction
    // must succeed, proving the driver await does not retain a Store guard.
    {
        let owned = owner.clone();
        let step = step.clone();
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                let o = owned.lock().unwrap();
                assert_eq!(
                    o.store()
                        .initialize_execution(o.session(), &step)
                        .unwrap()
                        .deadline_ms,
                    10000
                );
            }),
        )
        .await
        .expect("Store guard held across driver await")
        .unwrap();
    }
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let done = {
                let o = owner.lock().unwrap();
                o.store()
                    .initialize_status(o.session(), &step, 1900)
                    .unwrap()
                    == InitializeStatus::Completed
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    assert!(!gate.active.load(Ordering::SeqCst));
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn timeout_and_panic_retain_peak_and_never_stop_or_continue() {
    for panic in [false, true] {
        let (_dir, owner, fence, observations) = setup().await;
        let gate = Gate::new(panic);
        let w = worker(
            owner.clone(),
            observations,
            gate.clone(),
            CoordinatorOptions::default(),
        );
        // Spec §4: Initialize is bounded by the smaller of its own timeout and what
        // is left of the accepted deadline. The deadline is the short one here, so
        // the gate that never replies is abandoned 30 ms after the step arms.
        let a = w.start(&fence, 1930).unwrap();
        gate.entered().await;
        assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
        assert_eq!(
            a.wait(Duration::from_secs(10)).await.unwrap(),
            InitializeStatus::Uncertain
        );
        assert_peak(&owner, &fence);
        assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
        assert!(!gate.active.load(Ordering::SeqCst));
        assert!(w.start(&fence, 10000).is_err());
        w.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn shutdown_joins_effect_before_ownership_can_be_reacquired() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let a = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    drop(a);
    drop(owner);
    assert!(crate::ownership::OwnedCoordinatorState::open(dir.path()).is_err());
    assert!(matches!(
        w.shutdown().await.unwrap(),
        WorkerStatus::Uncertain { .. }
    ));
    assert!(!gate.active.load(Ordering::SeqCst));
    let restarted = crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap();
    assert!(restarted
        .store()
        .next_initialize(restarted.session())
        .unwrap()
        .is_none());
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn missing_notification_is_recovered_by_durable_poll() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    {
        let o = owner.lock().unwrap();
        o.store()
            .accept_start(o.session(), &fence, 1900, 10000)
            .unwrap();
    }
    gate.entered().await;
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_observation_blocks_but_expired_queue_releases_unused_endpoint() {
    for expired in [false, true] {
        let (dir, owner, fence, mut observations) = setup().await;
        let clock = Arc::new(AtomicI64::new(1900));
        {
            let o = owner.lock().unwrap();
            o.store()
                .accept_start(o.session(), &fence, 1800, 1901)
                .unwrap();
        }
        if expired {
            clock.store(1902, Ordering::SeqCst);
        } else {
            for o in &mut observations {
                o.sampled_at_ms = 1901;
            }
        }
        let clock_read = clock.clone();
        let gate = Gate::new(false);
        let driver = gate.clone();
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(move || Ok(clock_read.load(Ordering::SeqCst))),
            // ADR 0011 decision 5: the denial is retried until the budget is
            // spent. The cooldown is shortened so the test does not wait for the
            // policy default.
            CoordinatorOptions {
                retry_cooldown: Duration::from_millis(20),
                ..Default::default()
            },
            Arc::new(move |_| Ok(test_driver(driver.clone()))),
        )
        .unwrap();
        if expired {
            let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    let failed: bool = sql.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE deployment_id=?1 AND kind='initialize' AND state='failed')", [&fence.deployment_id], |r| r.get(0)).unwrap();
                    if failed { break; }
                    assert_eq!(w.status(), WorkerStatus::Running);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.unwrap();
        } else {
            // ADR 0011 decision 4: a stale observation denies only this
            // deployment's own arm. The coordinator keeps running; only this
            // deployment's admission closes.
            let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    let closed: bool = sql
                        .query_row(
                            "SELECT admission_enabled=0 FROM deployments WHERE id=?1",
                            [&fence.deployment_id],
                            |r| r.get(0),
                        )
                        .unwrap();
                    if closed {
                        break;
                    }
                    assert_eq!(w.status(), WorkerStatus::Running);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
        assert!(gate.calls.lock().unwrap().is_empty());
        {
            let o = owner.lock().unwrap();
            assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
            let binding = o.store().runtime_binding(&fence.deployment_id).unwrap();
            if expired {
                assert!(binding.is_none());
            } else {
                assert_eq!(binding.unwrap().state, "reserved");
            }
        }
        w.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn expired_unarmed_worker_skips_driver_and_continues_later_queued_work() {
    let (_dir, owner, fence, observations) = setup().await;
    let source = fixture::owned_source().await;
    let (expired, later) = {
        let o = owner.lock().unwrap();
        let expired = o
            .store()
            .accept_start(o.session(), &fence, 1800, 1900)
            .unwrap();
        let later = o
            .store()
            .accept_start(o.session(), &source.other, 1801, 10000)
            .unwrap();
        (expired, later)
    };
    let gate = Gate::new(false);
    let driver = gate.clone();
    let built = Arc::new(Mutex::new(Vec::new()));
    let driver_built = built.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(move |work| {
            driver_built.lock().unwrap().push(work.step_id().to_owned());
            Ok(test_driver(driver.clone()))
        }),
    )
    .unwrap();
    gate.entered().await;
    assert_eq!(*built.lock().unwrap(), vec![later.step_id.clone()]);
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    {
        let o = owner.lock().unwrap();
        assert_eq!(
            o.store()
                .initialize_status(o.session(), &expired.step_id, 1900)
                .unwrap(),
            InitializeStatus::ExpiredUnarmed
        );
        assert!(o
            .store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_none());
    }
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = {
                let o = owner.lock().unwrap();
                o.store()
                    .initialize_status(o.session(), &later.step_id, 1900)
                    .unwrap()
            };
            if status == InitializeStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_unarmed_during_observation_is_rechecked_before_driver_or_arm() {
    let (_dir, owner, fence, observations) = setup().await;
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let clock = Arc::new(AtomicI64::new(1900));
    let read_clock = clock.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(HeldObservation {
            entered: entered.clone(),
            release: release.clone(),
            observations,
        }),
        Arc::new(move || Ok(read_clock.load(Ordering::SeqCst))),
        CoordinatorOptions::default(),
        Arc::new(|_| panic!("expired observation must not construct driver")),
    )
    .unwrap();
    let observer = w.start(&fence, 10000).unwrap();
    tokio::time::timeout(Duration::from_secs(60), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    clock.store(10000, Ordering::SeqCst);
    release.add_permits(1);
    assert_eq!(
        observer.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::ExpiredUnarmed
    );
    {
        let o = owner.lock().unwrap();
        assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        assert!(o
            .store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_none());
    }
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_unarmed_after_driver_validation_never_arms_or_executes() {
    let (_dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let read_clock = clock.clone();
    let gate = Gate::new(false);
    let driver = gate.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(move || Ok(read_clock.load(Ordering::SeqCst))),
        CoordinatorOptions::default(),
        Arc::new(move |_| {
            clock.store(10000, Ordering::SeqCst);
            Ok(test_driver(driver.clone()))
        }),
    )
    .unwrap();
    let observer = w.start(&fence, 10000).unwrap();
    assert_eq!(
        observer.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::ExpiredUnarmed
    );
    assert!(gate.calls.lock().unwrap().is_empty());
    {
        let o = owner.lock().unwrap();
        assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        assert!(o
            .store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_none());
    }
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_unarmed_discovery_does_not_require_current_launch_policy() {
    let (dir, owner, fence, observations) = setup().await;
    {
        let o = owner.lock().unwrap();
        o.store()
            .accept_start(o.session(), &fence, 1800, 1900)
            .unwrap();
        let policy = o.store().resource_policy("lab").unwrap().unwrap();
        let mut revoked = policy.controls;
        revoked.device_sharing = capyctl_config::effective::Sharing::Exclusive;
        for sharing in revoked.device_sharing_overrides.values_mut() {
            *sharing = capyctl_config::effective::Sharing::Exclusive;
        }
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                policy.revision,
                "expiry-revoke",
                &revoked,
                &observations,
                1900,
            )
            .unwrap();
    }
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(|_| panic!("expired work must not construct driver")),
    )
    .unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let failed: bool = sql.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE deployment_id=?1 AND kind='initialize' AND state='failed')", [&fence.deployment_id], |r| r.get(0)).unwrap();
            if failed { break; }
            assert_eq!(w.status(),WorkerStatus::Running);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    {
        let o = owner.lock().unwrap();
        assert!(o
            .store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_none());
        assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
    }
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn observer_bound_is_enforced_without_accepting_extra_work() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner,
        observations,
        gate,
        CoordinatorOptions {
            max_observers: 1,
            ..Default::default()
        },
    );
    let a = w.start(&fence, 10000).unwrap();
    assert!(matches!(
        w.start(&fence, 10000),
        Err(CoordinatorError::Busy)
    ));
    drop(a);
    let b = w.start(&fence, 10000).unwrap();
    drop(b);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_worker_cannot_claim_the_same_owned_session() {
    let (_dir, owner, _fence, observations) = setup().await;
    let w = worker(
        owner.clone(),
        observations.clone(),
        Gate::new(false),
        CoordinatorOptions::default(),
    );
    assert!(spawn_fake(
        owner,
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default()
    )
    .is_err());
    w.shutdown().await.unwrap();
}

struct HeldObservation {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    observations: Vec<MemoryObservation>,
}
impl ServiceObservation for HeldObservation {
    fn observe(&self, _: String) -> ObservationFuture {
        let entered = self.entered.clone();
        let release = self.release.clone();
        let observations = self.observations.clone();
        Box::pin(async move {
            entered.add_permits(1);
            release.acquire().await.unwrap().forget();
            Ok(observations)
        })
    }
}

#[tokio::test]
async fn current_policy_race_and_observation_timeout_deny_send() {
    for timeout in [false, true] {
        let (dir, owner, fence, observations) = setup().await;
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let source = Arc::new(HeldObservation {
            entered: entered.clone(),
            release: release.clone(),
            observations: observations.clone(),
        });
        let gate = Gate::new(false);
        let driver = gate.clone();
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            source,
            Arc::new(|| Ok(1900)),
            CoordinatorOptions {
                // The denial is retried until the budget is spent; the test does
                // not wait for the policy default between attempts.
                retry_cooldown: Duration::from_millis(20),
                ..Default::default()
            },
            Arc::new(move |_| Ok(test_driver(driver.clone()))),
        )
        .unwrap();
        // The accepted deadline bounds the observation, so an observation that is
        // never released is abandoned 200 ms after the step is discovered.
        let a = w.start(&fence, 2100).unwrap();
        entered.acquire().await.unwrap().forget();
        if !timeout {
            let o = owner.lock().unwrap();
            let mut controls = o.store().resource_policy("lab").unwrap().unwrap().controls;
            controls.domains.get_mut("unified").unwrap().managed_limit = 9_i64 << 30;
            o.store()
                .update_resource_policy(
                    o.session(),
                    "owner",
                    "lab",
                    1,
                    "race",
                    &controls,
                    &observations,
                    1900,
                )
                .unwrap();
            release.add_permits(1);
        }
        // ADR 0011 decision 4: this denial is this deployment's own — a stale
        // observation or a policy race for its own arm — so the coordinator
        // keeps running and only this deployment's admission closes.
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let closed: bool = sql
                    .query_row(
                        "SELECT admission_enabled=0 FROM deployments WHERE id=?1",
                        [&fence.deployment_id],
                        |r| r.get(0),
                    )
                    .unwrap();
                if closed {
                    break;
                }
                assert_eq!(w.status(), WorkerStatus::Running);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(gate.calls.lock().unwrap().is_empty());
        assert!(owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .owners
            .is_empty());
        drop(a);
        w.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn clock_read_after_validation_preserves_arm_when_freshness_expires() {
    let (dir, owner, fence, observations) = setup().await;
    let armed = Arc::new(AtomicBool::new(false));
    let arm_seen = armed.clone();
    let runtime_thread = std::thread::current().id();
    let sql = Mutex::new(rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap());
    // Service time advances only at the final clock read outside Store. This
    // simulates costly provenance validation consuming the observation window.
    // ADR 0015: the scheduler's store jobs run beside the Initialize task, so a
    // busy owner lock no longer marks a read made under it; store jobs run on
    // blocking threads, the task's final clock read on this runtime thread.
    let clock = Arc::new(move || {
        if std::thread::current().id() == runtime_thread {
            let armed: bool = sql.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE o.kind='initialize' AND s.state='armed')", [], |r| r.get(0)).unwrap();
            if armed {
                arm_seen.store(true, Ordering::SeqCst);
                return Ok(9999);
            }
        }
        Ok(1900)
    });
    let gate = Gate::new(false);
    let driver = gate.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        clock,
        CoordinatorOptions::default(),
        Arc::new(move |_| Ok(test_driver(driver.clone()))),
    )
    .unwrap();
    let a = w.start(&fence, 10000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert!(armed.load(Ordering::SeqCst));
    assert!(gate.calls.lock().unwrap().is_empty());
    assert_peak(&owner, &fence);
    drop(a);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_racing_completion_cannot_publish_ready() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let a = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    {
        let o = owner.lock().unwrap();
        o.store().fence_stop(o.session(), &fence, 10000).unwrap();
    }
    gate.release.add_permits(1);
    // ADR 0011 decision 4: the raced fence leaves this deployment's own binding
    // retained with no tracked cleanup for it, and the start is given up on —
    // but that is this deployment's own accounting, not a process-wide fault,
    // so the coordinator keeps running for every other deployment. T18: the
    // give-up names the incarnation the stop already fenced, so its closure is
    // stale and closes nothing (there is no deployment-wide fallback).
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let given_up: bool = sql
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM journal_entries WHERE operation_id=?1 AND state='given_up')",
                    [a.operation_id()],
                    |r| r.get(0),
                )
                .unwrap();
            if given_up {
                break;
            }
            assert_eq!(w.status(), WorkerStatus::Running);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        a.wait(Duration::from_secs(3)).await.unwrap(),
        InitializeStatus::Superseded
    );
    assert_peak(&owner, &fence);
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupt_store_after_effect_halts_later_increases_and_retains_arm() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let a = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    let other = fixture::owned_source().await.other.clone();
    let b = w.start(&other, 10000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    // Named negative fault: fail both evidence and uncertainty writes.
    sql.execute_batch("CREATE TRIGGER fail_worker_association BEFORE INSERT ON owned_launch_associations BEGIN SELECT RAISE(ABORT,'injected Store write failure'); END; CREATE TRIGGER fail_worker_uncertain BEFORE UPDATE OF state ON lifecycle_steps WHEN NEW.state='uncertain' BEGIN SELECT RAISE(ABORT,'injected uncertain failure'); END;").unwrap();
    gate.release.add_permits(1);
    assert!(matches!(stopped(&w).await, WorkerStatus::Failed(_)));
    assert_peak(&owner, &fence);
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    let state: String = sql
        .query_row(
            "SELECT state FROM lifecycle_steps WHERE id=?1",
            [a.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "armed");
    {
        let o = owner.lock().unwrap();
        assert_eq!(
            o.store()
                .runtime_binding(&other.deployment_id)
                .unwrap()
                .unwrap()
                .state,
            "reserved"
        );
    }
    drop(a);
    drop(b);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn persisted_queue_bound_denies_new_work_but_allows_join() {
    let (_dir, owner, fence, observations) = setup().await;
    {
        let o = owner.lock().unwrap();
        let mut controls = o.store().resource_policy("lab").unwrap().unwrap().controls;
        controls.queue.max_pending_total = 1;
        controls.queue.max_pending_per_deployment = 1;
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                1,
                "bound",
                &controls,
                &observations,
                1800,
            )
            .unwrap();
    }
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let a = w.start(&fence, 10000).unwrap();
    let b = w.start(&fence, 10000).unwrap();
    assert_eq!(a.operation_id(), b.operation_id());
    let other = fixture::owned_source().await.other.clone();
    assert!(w.start(&other, 10000).is_err());
    assert!(owner
        .lock()
        .unwrap()
        .store()
        .runtime_binding(&other.deployment_id)
        .unwrap()
        .is_none());
    drop(a);
    drop(b);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_elapsed_service_clock_bounds_provenance_before_send() {
    let (dir, owner, fence, observations) = setup().await;
    // Explicit test-only 10s policy accommodates concurrent debug validation.
    // The strict default-2s expiry test above remains separate.
    {
        let o = owner.lock().unwrap();
        let mut controls = o.store().resource_policy("lab").unwrap().unwrap().controls;
        controls.observation_ttl_ms = 10000;
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                1,
                "timing-fixture",
                &controls,
                &observations,
                1800,
            )
            .unwrap();
    }
    let started = std::time::Instant::now();
    let clock: ServiceClock = Arc::new(move || Ok(1900 + started.elapsed().as_millis() as i64));
    struct FreshSource(ServiceClock);
    impl ServiceObservation for FreshSource {
        fn observe(&self, _: String) -> ObservationFuture {
            let now = (self.0)().unwrap();
            Box::pin(async move {
                Ok(vec![MemoryObservation {
                    domain: "unified".into(),
                    capacity_bytes: 1_i64 << 50,
                    available_bytes: 1_i64 << 50,
                    sampled_at_ms: now,
                }])
            })
        }
    }
    let w = spawn_fake(
        owner,
        Arc::new(FreshSource(clock.clone())),
        clock,
        CoordinatorOptions::default(),
    )
    .unwrap();
    let a = w.start(&fence, 30000).unwrap();
    // Wait for the durable result even when concurrent fixture validation uses
    // most of its unchanged 30000ms deadline. This is only a caller watchdog.
    let status = a.wait(Duration::from_secs(60)).await.unwrap();
    eprintln!(
        "owned ordinary real-clock acceptance to observation: {:?}; {status:?}; {:?}",
        started.elapsed(),
        w.status()
    );
    assert_eq!(status, InitializeStatus::Completed);
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let (issued, observed): (i64, i64) = sql.query_row(
        "SELECT json_extract(s.step_json,'$.execution.issued_at_ms'),json_extract(a.association_json,'$.observed_at_ms') FROM lifecycle_steps s JOIN owned_launch_associations a ON a.step_id=s.id WHERE s.id=?1",
        [a.step_id()], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert!(
        observed > issued,
        "real milestone must not reuse arm timestamp"
    );
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn arm_context_is_not_reissued_and_pre_send_rejects_persisted_mutations() {
    let (dir, owner, fence, observations) = setup().await;
    let o = owner.lock().unwrap();
    let accepted = o
        .store()
        .accept_start(o.session(), &fence, 1800, 10000)
        .unwrap();
    let policy = o.store().resource_policy("lab").unwrap().unwrap();
    let limits: Vec<_> = policy
        .controls
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
    let admission = || {
        capyctl_scheduler::residency::AdmissionContext::new(
            &observations,
            &limits,
            1900,
            policy.controls.observation_ttl_ms,
            policy.controls.max_parked as usize,
        )
    };
    let (arm, context) = o
        .store()
        .arm_initialize_with_context(o.session(), &accepted.step_id, admission())
        .unwrap();
    assert!(permits_send(&arm));
    let context = context.unwrap();
    let (retry, no_context) = o
        .store()
        .arm_initialize_with_context(o.session(), &accepted.step_id, admission())
        .unwrap();
    assert!(!permits_send(&retry));
    assert!(no_context.is_none());
    assert!(o
        .store()
        .revalidate_initialize_send(o.session(), &accepted.step_id, &context, 1900)
        .is_ok());
    assert!(o
        .store()
        .revalidate_initialize_send(o.session(), &accepted.step_id, &context, 10000)
        .is_err());
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    // Named negative injections after arm. Each failed validation is read-only.
    for (mutation, restore) in [
        (
            "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution.issued_at_ms',1901) WHERE id=?1",
            "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution.issued_at_ms',1900) WHERE id=?1",
        ),
        (
            "UPDATE lifecycle_steps SET state='uncertain' WHERE id=?1",
            "UPDATE lifecycle_steps SET state='armed' WHERE id=?1",
        ),
        (
            "UPDATE runtime_bindings SET identities_json='[{}]' WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE id=?1)",
            "UPDATE runtime_bindings SET identities_json='[]' WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE id=?1)",
        ),
        (
            "UPDATE endpoint_leases SET host='127.0.0.2' WHERE binding_id=(SELECT binding_id FROM lifecycle_steps WHERE id=?1)",
            "UPDATE endpoint_leases SET host='127.0.0.1' WHERE binding_id=(SELECT binding_id FROM lifecycle_steps WHERE id=?1)",
        ),
        (
            "UPDATE lifecycle_claims SET generation=generation+1 WHERE operation_id=(SELECT operation_id FROM lifecycle_steps WHERE id=?1)",
            "UPDATE lifecycle_claims SET generation=generation-1 WHERE operation_id=(SELECT operation_id FROM lifecycle_steps WHERE id=?1)",
        ),
        (
            // ADR 0013 §5: dispatch is the instance's own gate.
            "UPDATE deployment_instances SET dispatch_enabled=1 WHERE deployment_id=(SELECT deployment_id FROM lifecycle_steps WHERE id=?1)",
            "UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id=(SELECT deployment_id FROM lifecycle_steps WHERE id=?1)",
        ),
        (
            "UPDATE resource_grants SET committed_epoch=committed_epoch+10 WHERE id=(SELECT grant_id FROM lifecycle_steps WHERE id=?1)",
            "UPDATE resource_grants SET committed_epoch=committed_epoch-10 WHERE id=(SELECT grant_id FROM lifecycle_steps WHERE id=?1)",
        ),
    ] {
        sql.execute(mutation, [&accepted.step_id]).unwrap();
        assert!(
            o.store()
                .revalidate_initialize_send(
                    o.session(),
                    &accepted.step_id,
                    &context,
                    1900
                )
                .is_err(),
            "{mutation}"
        );
        sql.execute(restore, [&accepted.step_id]).unwrap();
        assert!(
            o.store()
                .revalidate_initialize_send(
                    o.session(),
                    &accepted.step_id,
                    &context,
                    1900
                )
                .is_ok()
        );
    }
    o.store()
        .update_resource_policy(
            o.session(),
            "owner",
            "lab",
            1,
            "after-arm",
            &policy.controls,
            &observations,
            1900,
        )
        .unwrap();
    assert!(o
        .store()
        .revalidate_initialize_send(o.session(), &accepted.step_id, &context, 1900)
        .is_err());
}

#[tokio::test]
async fn completed_observer_rejects_binding_identity_and_evidence_corruption() {
    let (dir, owner, fence, observations) = setup().await;
    let w = spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let a = w.start(&fence, 10000).unwrap();
    assert_eq!(
        a.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    {
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        let o = owner.lock().unwrap();
        for (select, update, corrupt) in [
            (
                "SELECT b.state FROM runtime_bindings b JOIN lifecycle_steps s ON s.binding_id=b.id WHERE s.id=?1",
                "UPDATE runtime_bindings SET state=?2 WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE id=?1)",
                "uncertain",
            ),
            (
                "SELECT b.identities_json FROM runtime_bindings b JOIN lifecycle_steps s ON s.binding_id=b.id WHERE s.id=?1",
                "UPDATE runtime_bindings SET identities_json=?2 WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE id=?1)",
                "[]",
            ),
            (
                "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
                "UPDATE lifecycle_evidence SET evidence_json=?2 WHERE step_id=?1",
                "{}",
            ),
            (
                "SELECT o.state FROM operations o JOIN lifecycle_steps s ON s.operation_id=o.id WHERE s.id=?1",
                "UPDATE operations SET state=?2 WHERE id=(SELECT operation_id FROM lifecycle_steps WHERE id=?1)",
                "running",
            ),
        ] {
            let original: String = sql.query_row(select, [a.step_id()], |r| r.get(0)).unwrap();
            sql.execute(update, [a.step_id(), corrupt]).unwrap();
            assert!(
                o.store()
                    .initialize_status(o.session(), a.step_id(), 1900)
                    .is_err(),
                "{update}"
            );
            sql.execute(update, [a.step_id(), &original]).unwrap();
            assert_eq!(
                o.store()
                    .initialize_status(o.session(), a.step_id(), 1900)
                    .unwrap(),
                InitializeStatus::Completed
            );
        }
    }
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_with_an_armed_step_never_resends_it() {
    let (dir, owner, fence, observations) = setup().await;
    {
        let o = owner.lock().unwrap();
        let accepted = o
            .store()
            .accept_start(o.session(), &fence, 1800, 10000)
            .unwrap();
        let p = o.store().resource_policy("lab").unwrap().unwrap();
        let limits: Vec<_> = p
            .controls
            .domains
            .iter()
            .map(|(domain, d)| MemoryLimit {
                domain: domain.clone(),
                managed_bytes: d.managed_limit,
                free_reserve_bytes: d.free_reserve,
                reserve_absorbs_unmanaged: d.memory
                    == capyctl_config::effective::DomainMemory::Device,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect();
        let result = o
            .store()
            .arm_step(
                o.session(),
                &accepted.step_id,
                capyctl_scheduler::residency::AdmissionContext::new(
                    &observations,
                    &limits,
                    1900,
                    p.controls.observation_ttl_ms,
                    p.controls.max_parked as usize,
                ),
            )
            .unwrap();
        assert!(permits_send(&result));
    }
    drop(owner);
    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(gate.calls.lock().unwrap().is_empty());
    assert_peak(&owner, &fence);
    assert!(w.start(&fence, 10000).is_err());
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_observer_reads_remain_bounded_until_blocking_jobs_exit() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions {
            max_observers: 1,
            ..Default::default()
        },
    );
    let a = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = tokio::task::spawn_blocking(move || {
        let _guard = owner.lock().unwrap();
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    entered_rx.await.unwrap();
    for _ in 0..8 {
        assert!(matches!(
            a.wait(Duration::from_millis(5)).await,
            Err(CoordinatorError::CallerTimeout)
        ));
    }
    // Two submitted Store jobs retain their permits despite caller cancellation;
    // later reads wait outside the blocking pool and are cancelled there.
    assert_eq!(w.shared.store_jobs.available_permits(), 0);
    release_tx.send(()).unwrap();
    blocker.await.unwrap();
    let drained = tokio::time::timeout(
        Duration::from_secs(5),
        w.shared.store_jobs.clone().acquire_many_owned(2),
    )
    .await
    .unwrap()
    .unwrap();
    drop(drained);
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn measure_full_validation_stages_with_unmodified_observation_evidence() {
    let (_dir, owner, fence, mut observations) = setup().await;
    let started = std::time::Instant::now();
    let now = move || 1900 + started.elapsed().as_millis() as i64;
    fn stage<T>(
        owner: &SharedCoordinatorState,
        name: &str,
        f: impl FnOnce(&crate::ownership::OwnedCoordinatorState) -> T,
    ) -> T {
        let started = std::time::Instant::now();
        let result = f(&owner.lock().unwrap());
        eprintln!("ordinary worker stage {name}: {:?}", started.elapsed());
        result
    }
    let controls = stage(&owner, "test-only-10s-policy", |o| {
        let mut c = o.store().resource_policy("lab").unwrap().unwrap().controls;
        c.observation_ttl_ms = 10000;
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                1,
                "stage-timing",
                &c,
                &observations,
                1800,
            )
            .unwrap();
        c
    });
    let accepted = stage(&owner, "acceptance", |o| {
        o.store()
            .accept_start(o.session(), &fence, now(), 30000)
            .unwrap()
    });
    stage(&owner, "discovery", |o| {
        assert!(o.store().next_initialize(o.session()).unwrap().is_some());
    });
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
    for observation in &mut observations {
        observation.sampled_at_ms = now();
    }
    let (arm, context) = stage(&owner, "arm-and-context", |o| {
        o.store()
            .arm_initialize_with_context(
                o.session(),
                &accepted.step_id,
                capyctl_scheduler::residency::AdmissionContext::new(
                    &observations,
                    &limits,
                    now(),
                    controls.observation_ttl_ms,
                    controls.max_parked as usize,
                ),
            )
            .unwrap()
    });
    assert!(permits_send(&arm));
    let context = context.unwrap();
    stage(&owner, "pre-send-local-fences", |o| {
        o.store()
            .revalidate_initialize_send(o.session(), &accepted.step_id, &context, now())
            .unwrap()
    });
    let engine = FakeEngine::with_lifecycle_clock(Arc::new(move || Ok(now())));
    let observation = engine
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    let observed_at_ms = observation.observed_at_ms;
    stage(&owner, "owned-association-full-proof", |o| {
        o.store()
            .record_owned_launch(
                o.session(),
                &accepted.step_id,
                &OwnedLaunchReceipt {
                    binding_id: observation.binding_id,
                    incarnation: observation.incarnation,
                    identities: observation.identities.clone(),
                    observed_at_ms,
                    receipt: observation.receipt.clone(),
                },
                now(),
            )
            .unwrap()
    });
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities,
        observed_at_ms,
        control_receipt: Some(observation.receipt),
        milestones: observation.facts,
    };
    stage(&owner, "completion-full-proof", |o| {
        o.store()
            .complete_step(
                o.session(),
                &accepted.step_id,
                &evidence,
                now(),
                controls.observation_ttl_ms,
            )
            .unwrap()
    });
    assert_eq!(evidence.observed_at_ms, observed_at_ms);
    assert_eq!(
        stage(&owner, "observer-local-proof", |o| o
            .store()
            .initialize_status(o.session(), &accepted.step_id, now())
            .unwrap()),
        InitializeStatus::Completed
    );
}

/// Wait for the worker to be admitting Initialize again, which it publishes by
/// returning to Running.
async fn running(worker: &OwnedCoordinator) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while worker.status() != WorkerStatus::Running {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("worker never resumed admitting Initialize");
}

/// How many Initialize steps this deployment has, and whether its admission is
/// still open.
fn steps(sql: &rusqlite::Connection, deployment_id: &str) -> (i64, bool) {
    sql.query_row(
        "SELECT (SELECT COUNT(*) FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                 WHERE s.deployment_id=?1 AND o.kind='initialize'),
                (SELECT admission_enabled=1 FROM deployments WHERE id=?1)",
        [deployment_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

/// ADR 0011 decision 5: a failed attempt is retried, and the deployment is given
/// up on only after the budget is spent. SPEC §13.2: the retry is counted against
/// the exact configuration that failed, and the wait between attempts doubles. T20
// T20
#[tokio::test]
async fn a_failed_start_is_retried_until_the_budget_is_spent() {
    let (dir, owner, fence, observations) = setup().await;
    // The driver is never constructed, so this failure is known not to have
    // landed: the step stays planned, holds no grant and produced no evidence.
    let drives = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = drives.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            max_attempts: 3,
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
        Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Err(CoordinatorError::Service("injected recipe failure".into()))
        }),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let start = w.start(&fence, 10000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !steps(&sql, &fence.deployment_id).1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the deployment never gave up");
    // 20 ms before the second attempt and 40 ms before the third: a broken recipe
    // does not burn the device in a tight loop.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(60),
        "the cooldown did not double: {elapsed:?}"
    );
    assert_eq!(drives.load(Ordering::SeqCst), 3, "the budget was not spent");
    let record = {
        let o = owner.lock().unwrap();
        o.store().attempts(&fence).unwrap()
    };
    assert_eq!(record.map(|r| r.attempts), Some(3));
    // SPEC §17: failures are recorded. Every counted attempt and the give-up
    // itself name this deployment and why it failed.
    let journal = {
        let o = owner.lock().unwrap();
        o.store().journal_evidence(start.operation_id()).unwrap()
    };
    assert_eq!(
        journal
            .iter()
            .filter(
                |entry| entry.contains(&format!("deployment {}: ", fence.deployment_id))
                    && entry.contains("injected recipe failure")
            )
            .count(),
        4,
        "three attempts and one give-up were not journaled: {journal:?}"
    );
    assert!(
        journal.iter().any(|entry| entry.contains("gave up: ")),
        "the give-up was not journaled: {journal:?}"
    );
    // The coordinator itself is unaffected, and nothing was armed for a fourth
    // attempt: one step, still planned, no grant, no evidence.
    assert_eq!(w.status(), WorkerStatus::Running);
    assert_eq!(steps(&sql, &fence.deployment_id).0, 1);
    let durable: (String, bool, i64) = sql
        .query_row(
            "SELECT s.state,s.grant_id IS NULL,(SELECT COUNT(*) FROM lifecycle_evidence WHERE step_id=s.id) FROM lifecycle_steps s WHERE s.id=?1",
            [start.step_id()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(durable, ("planned".into(), true, 0));
    // The give-up is durable: no further attempt is recorded after the budget.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(drives.load(Ordering::SeqCst), 3);
    drop(start);
    w.shutdown().await.unwrap();
}

/// ADR 0011 decision 5, first row: a success is terminal and the attempts reset.
/// A configuration that reached Ready must not carry the failures it took to get
/// there into the next time it is started. T20
// T20
#[tokio::test]
async fn a_success_resets_the_attempt_budget() {
    let (_dir, owner, fence, observations) = setup().await;
    // The first two attempts fail before any driver exists; the third is healthy.
    let gate = Gate::new(false);
    gate.release.add_permits(1);
    let driver = gate.clone();
    let drives = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            max_attempts: 3,
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
        Arc::new(move |_| {
            if drives.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(CoordinatorError::Service("injected recipe failure".into()));
            }
            Ok(test_driver(driver.clone()))
        }),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    // The observer sees Ready when the step commits; the task resets the
    // budget just after that. Shutdown joins every task, so once it returns
    // the reset has run (under load the budget was read before the reset).
    w.shutdown().await.unwrap();
    let record = {
        let o = owner.lock().unwrap();
        o.store().attempts(&fence).unwrap()
    };
    assert_eq!(record, None, "the two failed attempts were not reset");
}

/// Retrying an effect that may have landed can start a second engine while the
/// first still holds memory. SPEC §13.2 and ADR 0011 decision 5: an uncertain
/// attempt is not counted and not retried until the recorded processes are proven
/// gone. T20
// T20
#[tokio::test]
async fn an_uncertain_attempt_is_not_retried_while_processes_remain() {
    let (dir, owner, fence, observations) = setup().await;
    // The launch association is recorded and then the reply is lost: the engine
    // holds device memory and the outcome of the step is unknown.
    let first = Gate::new(false);
    *first.association.lock().unwrap() = Some(owner.clone());
    first.lost_reply.store(true, Ordering::SeqCst);
    let second = Gate::new(false);
    second.release.add_permits(1);
    let constructed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (uncertain, healthy) = (first.clone(), second.clone());
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            max_attempts: 3,
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
        Arc::new(move |_| {
            Ok(test_driver(
                if constructed.fetch_add(1, Ordering::SeqCst) == 0 {
                    uncertain.clone()
                } else {
                    healthy.clone()
                },
            ))
        }),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    first.entered().await;
    first.release.add_permits(1);
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert_eq!(
        start.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Uncertain
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let attempts = |fence: &DeploymentFence| {
        let o = owner.lock().unwrap();
        o.store().attempts(fence).unwrap()
    };
    // Several cooldowns pass. Nothing is counted, nothing is retried, and the
    // deployment's own admission stays open: this is a pause, not a failure.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(attempts(&fence), None, "an uncertain attempt was counted");
    assert_eq!(steps(&sql, &fence.deployment_id), (1, true));
    assert_eq!(first.calls.lock().unwrap().len(), 1);
    // The explicit Stop drives cleanup, and only the gone-proof resolves it.
    let stop = w.stop("owner", &fence, "uncertain-stop", 10000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        capyctl_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Completed
    );
    running(&w).await;
    let next = DeploymentFence {
        deployment_id: fence.deployment_id.clone(),
        revision: stop.receipt().revision,
        generation: stop.receipt().generation,
    };
    let fresh = w.start(&next, 10000).unwrap();
    assert_eq!(
        fresh.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(steps(&sql, &fence.deployment_id).0, 2);
    assert_eq!(attempts(&fence), None);
    assert_eq!(attempts(&next), None);
    drop(start);
    w.shutdown().await.unwrap();
}

/// ADR 0011 decision 4: a deployment that gave up closes its own admission, and
/// an operator must still be able to stop it. Its start observer is told that it
/// is closed, not that it was superseded by somebody else's work. T20
// T20
#[tokio::test]
async fn a_given_up_deployment_reads_closed_and_can_still_be_stopped() {
    let (dir, owner, fence, observations) = setup().await;
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            max_attempts: 1,
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
        Arc::new(move |_| Err(CoordinatorError::Service("injected recipe failure".into()))),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !steps(&sql, &fence.deployment_id).1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the deployment never gave up");
    // The step is still planned, well inside its deadline, and the deployment is
    // no longer admitting. That is a closed deployment, not a superseded one.
    let status = {
        let o = owner.lock().unwrap();
        o.store()
            .initialize_status(o.session(), start.step_id(), 1900)
            .unwrap()
    };
    assert_eq!(status, InitializeStatus::Closed);
    let receipt = {
        let o = owner.lock().unwrap();
        o.store()
            .accept_ordinary_stop_command(
                o.session(),
                "owner",
                &fence.deployment_id,
                fence.revision,
                "stop",
                1900,
                10000,
            )
            .expect("an operator Stop of a given-up deployment was refused")
    };
    // The Stop that was accepted is the unarmed one: the step never armed, so
    // cleanup releases exactly what the planned step held and nothing more.
    let unarmed = {
        let o = owner.lock().unwrap();
        o.store()
            .unarmed_stop_for_predecessor(o.session(), start.step_id())
            .unwrap()
    };
    assert_eq!(
        unarmed.map(|r| r.operation_id),
        Some(receipt.operation_id.clone())
    );
    drop(start);
    w.shutdown().await.unwrap();
}

/// SPEC §6.1, §6.3: an operator's Stop of a deployment that gave up while it
/// still holds its runtime binding and endpoint lease is the ordinary Stop, not a
/// recorded one: accounting is released only when the worker completes that
/// cleanup, and automatic activation stays suspended. T10 T32
// T10 T32
#[tokio::test]
async fn an_operator_stop_of_a_given_up_deployment_holding_a_runtime_cleans_it_up() {
    let (dir, owner, fence, observations) = setup().await;
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            max_attempts: 1,
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
        Arc::new(move |_| Err(CoordinatorError::Service("injected recipe failure".into()))),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !steps(&sql, &fence.deployment_id).1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the deployment never gave up");
    let held = || {
        sql.query_row(
            "SELECT COUNT(*) FROM runtime_bindings b WHERE b.deployment_id=?1 AND b.state!='released'
                 AND EXISTS(SELECT 1 FROM endpoint_leases e WHERE e.binding_id=b.id)",
            [&fence.deployment_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(
        held(),
        1,
        "the given-up start still holds its binding and lease"
    );
    let stop = w
        .commands()
        .administrative_stop("owner", &fence.deployment_id, fence.revision, "stop", 10000)
        .expect("an operator Stop of a given-up deployment was refused");
    let kind: String = sql
        .query_row(
            "SELECT kind FROM operations WHERE id=?1",
            [stop.operation_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        kind, "ordinary_unarmed_stop",
        "held work is the ordinary Stop's"
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let done: bool = sql
                .query_row(
                    "SELECT state='succeeded' FROM operations WHERE id=?1",
                    [stop.operation_id()],
                    |r| r.get(0),
                )
                .unwrap();
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Stop never completed");
    {
        let o = owner.lock().unwrap();
        assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        assert!(o.store().is_admin_stopped(&fence.deployment_id).unwrap());
    }
    assert_eq!(
        held(),
        0,
        "the completed Stop released the binding and lease"
    );
    drop(start);
    w.shutdown().await.unwrap();
}

/// ADR 0011 decision 5: retries happen within the start command's deadline. A
/// deadline that arrives before the budget is spent is terminal for that start:
/// no second attempt is made, and the step still expires through the ordinary
/// deadline path. T20
// T20
#[tokio::test]
async fn a_deadline_reached_before_the_budget_is_terminal_for_that_start() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let read_clock = clock.clone();
    let drives = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = drives.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(move || Ok(read_clock.load(Ordering::SeqCst))),
        CoordinatorOptions {
            max_attempts: 3,
            // The budget could never be spent inside a 500 ms start window.
            retry_cooldown: Duration::from_secs(30),
            ..Default::default()
        },
        Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Err(CoordinatorError::Service("injected recipe failure".into()))
        }),
    )
    .unwrap();
    let observer = w.start(&fence, 2400).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !steps(&sql, &fence.deployment_id).1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the deadline did not close the deployment's admission");
    // One attempt, no cooldown sleep, and the budget deliberately unspent.
    assert_eq!(drives.load(Ordering::SeqCst), 1);
    let record = {
        let o = owner.lock().unwrap();
        o.store().attempts(&fence).unwrap()
    };
    assert_eq!(record.map(|r| r.attempts), Some(1));
    let journal = {
        let o = owner.lock().unwrap();
        o.store().journal_evidence(observer.operation_id()).unwrap()
    };
    assert!(
        journal
            .iter()
            .any(|entry| entry.contains("deadline reached before the budget was spent")),
        "the terminal deadline was not journaled: {journal:?}"
    );
    // The closed deployment still reaches its deadline: nothing is held for ever.
    clock.store(2400, Ordering::SeqCst);
    assert_eq!(
        observer.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::ExpiredUnarmed
    );
    {
        let o = owner.lock().unwrap();
        assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        assert!(o
            .store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_none());
    }
    w.shutdown().await.unwrap();
}

/// ADR 0011 decision 4: a closed deployment reaches its own deadline. An expired
/// step must not wait behind an older closed step whose deadline is later, or its
/// endpoint lease and reservation are held past the deadline for someone else's
/// reason. T20
// T20
#[tokio::test]
async fn an_expired_step_does_not_wait_behind_an_older_closed_step() {
    let (_dir, owner, _fence, _observations) = setup().await;
    let source = fixture::owned_source().await;
    let o = owner.lock().unwrap();
    // The older acceptance has the later deadline; the younger one has already
    // passed its own. Both deployments closed their own admission.
    let older = o
        .store()
        .accept_start(o.session(), &source.fence, 1800, 3000)
        .unwrap();
    let younger = o
        .store()
        .accept_start(o.session(), &source.other, 1801, 2000)
        .unwrap();
    for id in [&source.fence.deployment_id, &source.other.deployment_id] {
        o.store().set_admission_enabled(id, false).unwrap();
    }
    assert!(matches!(
        o.store()
            .next_initialize_or_expire(o.session(), 2500)
            .unwrap(),
        InitializePoll::ExpiredUnarmed
    ));
    assert_eq!(
        o.store()
            .initialize_status(o.session(), &younger.step_id, 2500)
            .unwrap(),
        InitializeStatus::ExpiredUnarmed
    );
    // The older step keeps its reservation: its own deadline has not arrived.
    assert_eq!(
        o.store()
            .initialize_status(o.session(), &older.step_id, 2500)
            .unwrap(),
        InitializeStatus::Closed
    );
    assert_eq!(
        o.store()
            .runtime_binding(&source.fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "reserved"
    );
    assert!(o
        .store()
        .runtime_binding(&source.other.deployment_id)
        .unwrap()
        .is_none());
}

mod cleanup_evidence {
    use super::super::*;
    use capyctl_domain::completion::ProcessIdentity;
    use capyctl_store::ordinary_lifecycle::cleanup::{CleanupExecutionContext, CleanupMode};

    fn boot() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .to_owned()
    }

    fn identity(pid: u32, start_ticks: u64, boot_id: &str) -> ProcessIdentity {
        ProcessIdentity {
            role: "api".into(),
            pid,
            boot_id: boot_id.into(),
            start_ticks,
        }
    }

    fn context(identities: Vec<ProcessIdentity>) -> CleanupExecutionContext {
        CleanupExecutionContext {
            operation_id: "op-1".into(),
            step_id: "step-1".into(),
            binding_id: "bind-1".into(),
            incarnation: "inc-1".into(),
            fence: capyctl_store::lifecycle::DeploymentFence {
                deployment_id: "dep-1".into(),
                revision: 1,
                generation: 1,
            },
            identities,
            issued_at_ms: 10,
            deadline_ms: i64::MAX,
            mode: CleanupMode::InspectOwnedGone,
        }
    }

    fn clock(value: i64) -> ServiceClock {
        Arc::new(move || Ok(value))
    }

    /// Bindings that record which launches were reported gone.
    #[derive(Default)]
    pub(super) struct Recording(pub(super) std::sync::Mutex<Vec<String>>);

    impl EngineBindings for Recording {
        fn spec(
            &self,
            _work: &InitializeWork,
        ) -> Result<capyctl_adapters::resolve::AdapterSpec, CoordinatorError> {
            Err(CoordinatorError::Service("no engine here".into()))
        }

        fn launch_gone(&self, incarnation: &str) {
            self.0.lock().unwrap().push(incarnation.to_owned());
        }
    }

    /// SPEC §8.2 / T21 (owner decision 2026-09-25): per-launch host state (the
    /// rendezvous directory) is released only on verified gone evidence; a
    /// launch whose absence is not proved keeps it.
    // T21 T33
    #[tokio::test]
    async fn per_launch_state_is_released_only_after_the_gone_proof() {
        let recording = Arc::new(Recording::default());
        let cleanup = terminate_then_prove_gone(
            capyctl_testkit::ScriptedTool::proving(),
            clock(7),
            Duration::from_millis(1),
            recording.clone(),
        );
        let gone = vec![identity(0x7FFF_FFF0, 1, &boot())];
        assert!(cleanup(context(gone)).await.is_ok());
        assert_eq!(*recording.0.lock().unwrap(), ["inc-1"]);
        // No identities is a missing record, not absence: nothing is released.
        assert!(cleanup(context(Vec::new())).await.is_err());
        assert_eq!(recording.0.lock().unwrap().len(), 1);
    }

    /// Absence of every recorded process is the only shape that yields evidence,
    /// and the evidence must carry exactly what was proven.
    #[test]
    fn every_process_proven_gone_produces_evidence() {
        let ids = vec![
            identity(0x7FFF_FFF0, 1, &boot()),
            identity(0x7FFF_FFF1, 2, &boot()),
        ];
        let evidence = observed_gone(&context(ids.clone()), &clock(4242)).unwrap();
        assert_eq!(evidence.binding_id, "bind-1");
        assert_eq!(evidence.incarnation, "inc-1");
        assert_eq!(evidence.identities, ids);
        assert_eq!(evidence.observed_at_ms, 4242);
    }

    /// A live process retains ownership. An engine reporting its own shutdown is
    /// not evidence that its processes released the device.
    #[test]
    fn a_live_process_retains_ownership() {
        // Name this very process, with its real start time, so it is genuinely alive.
        let pid = std::process::id();
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let close = raw.rfind(')').unwrap();
        let start: u64 = raw[close + 2..]
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let ids = vec![
            identity(0x7FFF_FFF0, 1, &boot()),
            identity(pid, start, &boot()),
        ];
        assert!(
            observed_gone(&context(ids), &clock(1)).is_err(),
            "a live member denies release"
        );
    }

    /// An empty identity set is a missing record, not an observation of absence.
    #[test]
    fn an_empty_identity_set_denies_release() {
        assert!(observed_gone(&context(Vec::new()), &clock(1)).is_err());
    }

    /// An unresolvable member denies release even when its siblings are proven gone.
    #[test]
    fn an_unresolvable_member_denies_release() {
        let ids = vec![identity(0x7FFF_FFF0, 1, &boot()), identity(0, 0, &boot())];
        assert!(observed_gone(&context(ids), &clock(1)).is_err());
    }

    /// Evidence is stamped with an observed time; a failing clock cannot be
    /// substituted with a default, which would date the proof to the epoch.
    #[test]
    fn a_failing_clock_denies_release() {
        let failing: ServiceClock =
            Arc::new(|| Err(CoordinatorError::Service("clock unavailable".into())));
        let ids = vec![identity(0x7FFF_FFF0, 1, &boot())];
        assert!(observed_gone(&context(ids), &failing).is_err());
    }
}

/// The director's side of a native launch: a builder that owns its processes, the
/// timeout that bounds it, the termination that ends it and the release that
/// follows a failure. The builder itself is scripted; a real engine is qualified on
/// the host and never here.
mod native {
    use super::*;
    // The scripted tools live in the testkit: a real process would make these
    // decisions depend on the host the suite happens to run on.
    use capyctl_testkit::ScriptedTool;

    /// A driver for a builder that owns its processes: the real cleanup closure and
    /// the tools the failure path terminates with.
    fn native_driver(
        gate: Arc<Gate>,
        tools: Arc<ScriptedTool>,
        options: &CoordinatorOptions,
    ) -> Arc<Driver> {
        let tools: Arc<dyn OwnedProcessLaunch> = tools;
        Arc::new(Driver {
            engine: gate,
            cleanup: terminate_then_prove_gone(
                tools.clone(),
                Arc::new(|| Ok(1900)),
                options.terminate_grace,
                Arc::new(super::cleanup_evidence::Recording::default()),
            ),
            tools: Some(tools),
            settle: None,
        })
    }

    fn native_options() -> CoordinatorOptions {
        CoordinatorOptions {
            // One attempt: a launch that failed after arm is terminal for that
            // start, so the budget never comes into it.
            max_attempts: 1,
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        }
    }

    /// A builder that arms, records whatever the test asked it to, and then fails
    /// with `reason`.
    fn failing_gate(owner: Option<SharedCoordinatorState>, reason: &str) -> Arc<Gate> {
        let gate = Gate::new(false);
        *gate.association.lock().unwrap() = owner;
        *gate.failure.lock().unwrap() = Some(reason.into());
        gate.release.add_permits(16);
        gate
    }

    fn journal(owner: &SharedCoordinatorState, operation: &str) -> Vec<String> {
        let o = owner.lock().unwrap();
        o.store().journal_evidence(operation).unwrap()
    }

    /// The `state` column of this operation's journal entries, in order: what the
    /// journal says happened, as distinct from the reason it gives.
    fn journal_kinds(sql: &rusqlite::Connection, operation: &str) -> Vec<String> {
        sql.prepare("SELECT state FROM journal_entries WHERE operation_id=?1 ORDER BY rowid")
            .unwrap()
            .query_map([operation], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn status(owner: &SharedCoordinatorState, step: &str) -> InitializeStatus {
        let o = owner.lock().unwrap();
        o.store()
            .initialize_status(o.session(), step, 1900)
            .unwrap()
    }

    /// Spec §4: Initialize is bounded by `initialize_timeout`, not by
    /// `protocol_timeout`. A cold start reads weights off disk; this project
    /// measured a 4B model taking 27 to 63 seconds, so the protocol bound would
    /// give up on a healthy engine mid-load and then kill it. T10
    // T10
    #[tokio::test]
    async fn initialize_outlives_protocol_timeout() {
        let (_dir, owner, fence, observations) = setup().await;
        let gate = Gate::new(false);
        // The builder becomes ready after the protocol bound has already passed.
        let slow = gate.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(6_200)).await;
            slow.release.add_permits(1);
        });
        let w = worker(
            owner.clone(),
            observations,
            gate.clone(),
            CoordinatorOptions {
                // The smallest protocol bound the one-second grace floor fits in.
                protocol_timeout: Duration::from_millis(6_100),
                terminate_grace: Duration::from_secs(1),
                initialize_timeout: Duration::from_secs(30),
                ..Default::default()
            },
        );
        let start = w.start(&fence, 10_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Completed,
            "the protocol timeout, not the Initialize timeout, bounded the builder"
        );
        assert_eq!(w.status(), WorkerStatus::Running);
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// Spec §6: a native launch that fails after arm is terminated, proven gone,
    /// released with evidence, journaled, and the deployment reads Closed. Before
    /// this the step paused Uncertain and waited for an operator's Stop. T20
    // T20
    #[tokio::test]
    async fn a_failed_native_launch_is_released_and_closed() {
        let (dir, owner, fence, observations) = setup().await;
        let other = fixture::owned_source().await.other.clone();
        let gate = failing_gate(Some(owner.clone()), "engine exited");
        let healthy = Gate::new(false);
        healthy.release.add_permits(16);
        let tools = ScriptedTool::proving();
        let options = native_options();
        let (failing_id, factory_owner) = (fence.deployment_id.clone(), owner.clone());
        let (driver_gate, driver_tools, driver_options) =
            (gate.clone(), tools.clone(), options.clone());
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            options,
            Arc::new(move |work| {
                if work.fence().deployment_id != failing_id {
                    return Ok(test_driver(healthy.clone()));
                }
                // Spec §3: the key is sealed under the binding before the builder
                // is handed it, so the release has one to delete.
                {
                    let o = factory_owner.lock().unwrap();
                    o.store()
                        .store_engine_key(
                            work.binding_id(),
                            work.incarnation(),
                            &capyctl_store::secrets::new_engine_key(),
                            capyctl_store::secrets::SecretRole::Inference,
                        )
                        .unwrap();
                }
                Ok(native_driver(
                    driver_gate.clone(),
                    driver_tools.clone(),
                    &driver_options,
                ))
            }),
        )
        .unwrap();
        let start = w.start(&fence, 10_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Closed
        );
        // ADR 0011 decision 4: the moment the closure is observable, a start for
        // another deployment is admitted. No waiting for the worker to come back
        // round: L7 on host-a arrived in exactly that gap and was refused.
        let fresh = w.start(&other, 10_000).unwrap();
        // The recorded processes were terminated, and exactly the recorded ones.
        let terminations = tools.terminations();
        assert_eq!(terminations.len(), 1, "the launch was not terminated");
        assert_eq!(terminations[0].len(), 2);
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        let step_state: String = sql
            .query_row(
                "SELECT state FROM lifecycle_steps WHERE id=?1",
                [start.step_id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(step_state, "cancelled");
        let binding_state: String = sql
            .query_row(
                "SELECT state FROM runtime_bindings WHERE deployment_id=?1",
                [&fence.deployment_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(binding_state, "released");
        {
            let o = owner.lock().unwrap();
            assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        }
        let secrets: i64 = sql
            .query_row("SELECT COUNT(*) FROM engine_secrets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(secrets, 0, "the engine key outlived its binding");
        // SPEC §17: failures are recorded, naming the deployment and the reason.
        let entries = journal(&owner, start.operation_id());
        assert!(
            entries.iter().any(|entry| entry
                .contains(&format!("deployment {}", fence.deployment_id))
                && entry.contains("engine exited")),
            "the failure was not journaled: {entries:?}"
        );
        // One event, one story. The settlement writes the redacted reason and the
        // worker must not then write it again as an exhausted retry budget: ADR
        // 0011 decision 5 says a launch that failed after arm is terminal for that
        // start with no retry.
        let kinds = journal_kinds(&sql, start.operation_id());
        assert_eq!(
            kinds.iter().filter(|kind| *kind == "launch_failed").count(),
            1,
            "the settlement's entry is the only account of the failure: {kinds:?}"
        );
        assert!(
            !kinds.iter().any(|kind| kind == "given_up"),
            "a settled native launch was also journaled as a give-up: {kinds:?}"
        );
        assert_eq!(status(&owner, start.step_id()), InitializeStatus::Closed);
        // Only this deployment closed. The worker keeps running and the other
        // deployment starts as though nothing had happened.
        assert_eq!(
            fresh.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Completed
        );
        running(&w).await;
        assert_eq!(w.status(), WorkerStatus::Running);
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// A coordinator whose launches arm and then fail with "engine exited", and
    /// the deployment whose one launch already failed and was released with
    /// evidence: it holds nothing and reads FAILED (SPEC §6.1).
    async fn failed_deployment() -> (
        tempfile::TempDir,
        SharedCoordinatorState,
        DeploymentFence,
        OwnedCoordinator,
    ) {
        let (dir, owner, fence, observations) = setup().await;
        let gate = failing_gate(Some(owner.clone()), "engine exited");
        let tools = ScriptedTool::proving();
        let options = native_options();
        let factory_owner = owner.clone();
        let (driver_gate, driver_tools, driver_options) =
            (gate.clone(), tools.clone(), options.clone());
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            options,
            Arc::new(move |work| {
                {
                    let o = factory_owner.lock().unwrap();
                    o.store()
                        .store_engine_key(
                            work.binding_id(),
                            work.incarnation(),
                            &capyctl_store::secrets::new_engine_key(),
                            capyctl_store::secrets::SecretRole::Inference,
                        )
                        .unwrap();
                }
                Ok(native_driver(
                    driver_gate.clone(),
                    driver_tools.clone(),
                    &driver_options,
                ))
            }),
        )
        .unwrap();
        let start = w.start(&fence, 10_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Closed
        );
        drop(start);
        assert_eq!(
            deployment_states(&owner, &fence.deployment_id),
            ("ready".into(), "failed".into())
        );
        (dir, owner, fence, w)
    }

    /// `(desired_state, observed_state)` as status reports them.
    fn deployment_states(owner: &SharedCoordinatorState, id: &str) -> (String, String) {
        let snapshot = owner.lock().unwrap().store().snapshot().unwrap();
        let d = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
        (d.desired_state.clone(), d.observed_state.clone())
    }

    fn operation_state(owner: &SharedCoordinatorState, operation: &str) -> Option<String> {
        let snapshot = owner.lock().unwrap().store().snapshot().unwrap();
        snapshot
            .operations
            .iter()
            .find(|op| op.id == operation)
            .map(|op| op.state.clone())
    }

    /// SPEC §6.1, §6.3 (live M16, M53): an operator's Stop of a FAILED
    /// deployment that holds nothing was refused as `Lifecycle state does not
    /// permit this action`. It is accepted, recorded at once (automatic
    /// activation suspended, desired `stopped`), releases nothing, replays its
    /// receipt, and the deployment can then be deleted. T10 T32
    // T10 T32
    #[tokio::test]
    async fn an_operator_stop_of_a_failed_deployment_holding_nothing_is_recorded_at_once() {
        let (dir, owner, fence, w) = failed_deployment().await;
        let id = fence.deployment_id.clone();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        let history = |table: &str| {
            sql.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        let (steps_before, runs_before) = (history("lifecycle_steps"), history("lifecycle_runs"));
        let stop = w
            .commands()
            .administrative_stop("owner", &id, fence.revision, "stop-failed", 10_000)
            .expect("an operator Stop of a failed deployment was refused");
        // Completed in the acceptance transaction: nothing to wait for.
        assert_eq!(
            operation_state(&owner, stop.operation_id()).as_deref(),
            Some("succeeded")
        );
        assert_eq!(
            deployment_states(&owner, &id),
            ("stopped".into(), "stopped".into())
        );
        {
            let o = owner.lock().unwrap();
            assert!(o.store().is_admin_stopped(&id).unwrap());
            assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        }
        // Nothing was executed: no step and no run was created for it.
        assert_eq!(
            (history("lifecycle_steps"), history("lifecycle_runs")),
            (steps_before, runs_before)
        );
        // SPEC §17: the Stop is journaled with what it found.
        let journal = journal(&owner, stop.operation_id());
        assert!(
            journal
                .iter()
                .any(|entry| entry.contains("\"held\":\"nothing\"")),
            "{journal:?}"
        );
        // T09: an exact retry returns the original receipt; the same key with
        // another deadline is another command.
        let replay = w
            .commands()
            .administrative_stop("owner", &id, fence.revision, "stop-failed", 10_000)
            .unwrap();
        assert_eq!(replay, stop);
        assert!(matches!(
            w.commands()
                .administrative_stop("owner", &id, fence.revision, "stop-failed", 10_001),
            Err(CoordinatorCommandError::Lifecycle(
                LifecycleError::IdempotencyConflict
            ))
        ));
        // SPEC §6.3: the next inference request does not undo it; it is told
        // an operator stopped the deployment (owner decision 2026-09-25).
        use crate::port::LifecyclePort;
        let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
        assert!(matches!(
            port.auto_activate(&id).await,
            Err(crate::fault::LifecycleFault::Stopped(_))
        ));
        // The operator path the CLI reaches reads the same answer.
        let handle = port
            .request_transition(&id, capyctl_domain::LifecycleAction::Stop)
            .await
            .expect("the port's Stop of a stopped deployment was refused");
        assert_eq!(
            port.wait_terminal(&handle).await.unwrap(),
            capyctl_domain::LifecycleState::Stopped
        );
        // SPEC §6.3 (W6): nothing is held, so the delete follows.
        w.commands()
            .delete("owner", &id, fence.revision, "delete", 10_000)
            .expect("the delete after the Stop was refused");
        w.shutdown().await.unwrap();
    }

    /// SPEC §6.3: `start deployment` on a FAILED deployment makes a fresh
    /// attempt, before or after an operator's Stop, and lifts that Stop. T10
    // T10
    #[tokio::test]
    async fn a_start_of_a_failed_deployment_re_attempts_before_and_after_a_stop() {
        let (dir, owner, fence, w) = failed_deployment().await;
        let id = fence.deployment_id.clone();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        let initializes = || {
            sql.query_row(
                "SELECT COUNT(*) FROM operations WHERE deployment_id=?1 AND kind='initialize'",
                [&id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        let settle = |operation: String| {
            let owner = owner.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(60), async {
                    while operation_state(&owner, &operation).as_deref() != Some("failed") {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("the re-attempt never settled");
            }
        };
        assert_eq!(initializes(), 1);
        let again = w
            .commands()
            .start("owner", &id, fence.revision, "start-again", 10_000)
            .expect("a start of a failed deployment was refused");
        assert_eq!(initializes(), 2, "the start made a fresh attempt");
        settle(again.operation_id().to_owned()).await;
        assert_eq!(deployment_states(&owner, &id).1, "failed");

        w.commands()
            .administrative_stop("owner", &id, fence.revision, "stop", 10_000)
            .unwrap();
        assert_eq!(
            deployment_states(&owner, &id),
            ("stopped".into(), "stopped".into())
        );
        let after = w
            .commands()
            .start("owner", &id, fence.revision, "start-after-stop", 10_000)
            .expect("a start after the Stop was refused");
        assert_eq!(initializes(), 3);
        assert!(
            !owner.lock().unwrap().store().is_admin_stopped(&id).unwrap(),
            "the start did not lift the operator's Stop"
        );
        settle(after.operation_id().to_owned()).await;
        w.shutdown().await.unwrap();
    }

    fn operation_kind(owner: &SharedCoordinatorState, operation: &str) -> Option<(String, String)> {
        let snapshot = owner.lock().unwrap().store().snapshot().unwrap();
        snapshot
            .operations
            .iter()
            .find(|op| op.id == operation)
            .map(|op| (op.action.clone(), op.state.clone()))
    }

    fn held_bindings(dir: &tempfile::TempDir, deployment: &str) -> i64 {
        rusqlite::Connection::open(dir.path().join("srv.sqlite3"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?1 AND state!='released'",
                [deployment],
                |r| r.get(0),
            )
            .unwrap()
    }

    async fn until_operation(owner: &SharedCoordinatorState, operation: &str, state: &str) {
        tokio::time::timeout(Duration::from_secs(60), async {
            while operation_state(owner, operation).as_deref() != Some(state) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("operation {operation} never reached {state}"));
    }

    /// The journaled follow-up Stop a resolved deferred Stop names.
    fn follow_up(owner: &SharedCoordinatorState, operation: &str) -> String {
        let entries = journal(owner, operation);
        let resolved = entries
            .iter()
            .find_map(|entry| {
                serde_json::from_str::<serde_json::Value>(entry)
                    .ok()
                    .and_then(|v| v["follow_up_operation_id"].as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| panic!("no follow-up journaled: {entries:?}"));
        resolved
    }

    /// SPEC §6.3 (live M47): an operator's Stop while a launch is in flight and
    /// not yet associated was refused as `Lifecycle state does not permit this
    /// action`. It is accepted and deferred: activation is suspended at once,
    /// nothing is fenced or released while the launch runs, and once the launch
    /// is Ready the Stop is carried out as an ordinary one, releasing the
    /// runtime only on the cleanup's evidence. T10 T20
    // T10 T20
    #[tokio::test]
    async fn an_operator_stop_during_an_unassociated_launch_is_deferred_until_it_settles() {
        let (dir, owner, fence, observations) = setup().await;
        let id = fence.deployment_id.clone();
        let gate = Gate::new(false);
        let w = worker(
            owner.clone(),
            observations,
            gate.clone(),
            CoordinatorOptions::default(),
        );
        let start = w.start(&fence, 10_000).unwrap();
        gate.entered().await;
        let stop = w
            .commands()
            .administrative_stop("owner", &id, fence.revision, "stop-launching", 10_000)
            .expect("an operator Stop during a launch was refused");
        assert_eq!(
            operation_kind(&owner, stop.operation_id()),
            Some(("administrative_stop_deferred".into(), "pending".into()))
        );
        assert!(owner.lock().unwrap().store().is_admin_stopped(&id).unwrap());
        assert_eq!(
            held_bindings(&dir, &id),
            1,
            "nothing released while launching"
        );
        // T09: an exact retry answers the same receipt.
        let replay = w
            .commands()
            .administrative_stop("owner", &id, fence.revision, "stop-launching", 10_000)
            .unwrap();
        assert_eq!(replay, stop);
        gate.release.add_permits(1);
        // The launch completes (the deferred Stop never fenced it); an observer
        // reading after the Stop is carried out sees it superseded by that Stop.
        let observed = start.wait(Duration::from_secs(60)).await.unwrap();
        assert!(
            matches!(
                observed,
                InitializeStatus::Completed | InitializeStatus::Superseded
            ),
            "{observed:?}"
        );
        until_operation(&owner, stop.operation_id(), "succeeded").await;
        let cleanup = follow_up(&owner, stop.operation_id());
        assert_eq!(
            operation_kind(&owner, &cleanup)
                .map(|(kind, _)| kind)
                .as_deref(),
            Some("ordinary_cleanup")
        );
        until_operation(&owner, &cleanup, "succeeded").await;
        assert_eq!(held_bindings(&dir, &id), 0);
        assert_eq!(
            deployment_states(&owner, &id),
            ("stopped".into(), "stopped".into())
        );
        assert!(owner.lock().unwrap().store().is_admin_stopped(&id).unwrap());
        let replay = w
            .commands()
            .administrative_stop("owner", &id, fence.revision, "stop-launching", 10_000)
            .unwrap();
        assert_eq!(replay, stop, "the receipt replays after resolution");
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// SPEC §6.1, §6.3 (live M47): a deferred operator Stop whose launch then
    /// fails is carried out once the failure is released with evidence; the
    /// deployment reads stopped, not failed. T10 T20
    // T10 T20
    #[tokio::test]
    async fn a_deferred_stop_whose_launch_fails_records_the_stop_after_the_release() {
        let (_dir, owner, fence, observations) = setup().await;
        let id = fence.deployment_id.clone();
        let gate = Gate::new(false);
        *gate.association.lock().unwrap() = Some(owner.clone());
        *gate.failure.lock().unwrap() = Some("engine exited before readiness".into());
        gate.api_only.store(true, Ordering::SeqCst);
        let tools = ScriptedTool::proving();
        let options = native_options();
        let factory_owner = owner.clone();
        let (driver_gate, driver_tools, driver_options) =
            (gate.clone(), tools.clone(), options.clone());
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            options,
            Arc::new(move |work| {
                {
                    let o = factory_owner.lock().unwrap();
                    o.store()
                        .store_engine_key(
                            work.binding_id(),
                            work.incarnation(),
                            &capyctl_store::secrets::new_engine_key(),
                            capyctl_store::secrets::SecretRole::Inference,
                        )
                        .unwrap();
                }
                Ok(native_driver(
                    driver_gate.clone(),
                    driver_tools.clone(),
                    &driver_options,
                ))
            }),
        )
        .unwrap();
        let start = w.start(&fence, 10_000).unwrap();
        gate.entered().await;
        let stop = w
            .commands()
            .administrative_stop("owner", &id, fence.revision, "stop-launching", 10_000)
            .expect("an operator Stop during a launch was refused");
        assert_eq!(
            operation_kind(&owner, stop.operation_id()),
            Some(("administrative_stop_deferred".into(), "pending".into()))
        );
        gate.release.add_permits(1);
        // Closed if observed before the deferred Stop is carried out,
        // superseded after it; never an error.
        let observed = start.wait(Duration::from_secs(60)).await.unwrap();
        assert!(
            matches!(
                observed,
                InitializeStatus::Closed | InitializeStatus::Superseded
            ),
            "{observed:?}"
        );
        until_operation(&owner, stop.operation_id(), "succeeded").await;
        let recorded = follow_up(&owner, stop.operation_id());
        assert_eq!(
            operation_kind(&owner, &recorded),
            Some(("administrative_stop_recorded".into(), "succeeded".into()))
        );
        assert_eq!(
            tools.terminations().len(),
            1,
            "the failed launch was proven gone"
        );
        assert_eq!(
            deployment_states(&owner, &id),
            ("stopped".into(), "stopped".into())
        );
        // The recorded Stop fenced the instance: an observer of the failed
        // start reads it superseded, never corrupt.
        assert_eq!(
            status(&owner, start.step_id()),
            InitializeStatus::Superseded
        );
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// SPEC §6.3: a Start accepted after a deferred Stop lifts the operator's
    /// Stop, so the deferred one stops nothing and closes superseded. T10
    // T10
    #[tokio::test]
    async fn a_start_after_a_deferred_stop_supersedes_it() {
        let (dir, owner, fence, observations) = setup().await;
        let id = fence.deployment_id.clone();
        let gate = Gate::new(false);
        let w = worker(
            owner.clone(),
            observations,
            gate.clone(),
            CoordinatorOptions::default(),
        );
        let start = w.start(&fence, 10_000).unwrap();
        gate.entered().await;
        let stop = w
            .commands()
            .administrative_stop("owner", &id, fence.revision, "stop-launching", 10_000)
            .unwrap();
        w.commands()
            .start("owner", &id, fence.revision, "start-again", 10_000)
            .expect("a start after a deferred stop was refused");
        assert!(!owner.lock().unwrap().store().is_admin_stopped(&id).unwrap());
        gate.release.add_permits(1);
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Completed
        );
        until_operation(&owner, stop.operation_id(), "failed").await;
        assert_eq!(
            held_bindings(&dir, &id),
            1,
            "the started runtime keeps running"
        );
        assert_eq!(deployment_states(&owner, &id).1, "ready");
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// Spec §3 and §6: a durable launcher persists the API identity the moment
    /// the process exists, before anything is associated. An engine that then
    /// exits, or refuses its readiness probe, fails its launch with exactly that
    /// one identity recorded and no association. That launch must still settle
    /// as a proven release, not stall as corrupt stored data with the process
    /// left running. Found live on host-a: every failed native launch hung.
    #[tokio::test]
    async fn a_launch_that_recorded_only_its_api_process_is_released_and_closed() {
        let (dir, owner, fence, observations) = setup().await;
        let gate = failing_gate(Some(owner.clone()), "engine exited before readiness");
        gate.api_only.store(true, Ordering::SeqCst);
        let tools = ScriptedTool::proving();
        let options = native_options();
        let factory_owner = owner.clone();
        let (driver_gate, driver_tools, driver_options) =
            (gate.clone(), tools.clone(), options.clone());
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            options,
            Arc::new(move |work| {
                {
                    let o = factory_owner.lock().unwrap();
                    o.store()
                        .store_engine_key(
                            work.binding_id(),
                            work.incarnation(),
                            &capyctl_store::secrets::new_engine_key(),
                            capyctl_store::secrets::SecretRole::Inference,
                        )
                        .unwrap();
                }
                Ok(native_driver(
                    driver_gate.clone(),
                    driver_tools.clone(),
                    &driver_options,
                ))
            }),
        )
        .unwrap();
        let start = w.start(&fence, 10_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Closed
        );
        // Exactly the one recorded process was terminated.
        let terminations = tools.terminations();
        assert_eq!(terminations.len(), 1, "the launch was not terminated");
        assert_eq!(terminations[0].len(), 1);
        assert_eq!(terminations[0][0].role, "api");
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        let (step_state, binding_state): (String, String) = sql
            .query_row(
                "SELECT s.state,b.state FROM lifecycle_steps s JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.id=?1",
                [start.step_id()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (step_state.as_str(), binding_state.as_str()),
            ("cancelled", "released")
        );
        let secrets: i64 = sql
            .query_row("SELECT COUNT(*) FROM engine_secrets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(secrets, 0, "the engine key outlived its binding");
        assert_eq!(status(&owner, start.step_id()), InitializeStatus::Closed);
        assert_eq!(w.status(), WorkerStatus::Running);
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// Spec §6 step 1: nothing was recorded, so the gate never opened and the
    /// launcher disposed of the gated child itself. That outcome is the evidence;
    /// there is nothing to terminate and no gone-proof to ask for.
    #[tokio::test]
    async fn a_launch_with_no_recorded_identity_is_released() {
        let (dir, owner, fence, observations) = setup().await;
        let gate = failing_gate(None, "the child was never released");
        let tools = ScriptedTool::proving();
        let options = native_options();
        let (driver_gate, driver_tools, driver_options) =
            (gate.clone(), tools.clone(), options.clone());
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            options,
            Arc::new(move |_| {
                Ok(native_driver(
                    driver_gate.clone(),
                    driver_tools.clone(),
                    &driver_options,
                ))
            }),
        )
        .unwrap();
        let start = w.start(&fence, 10_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Closed
        );
        assert!(
            tools.terminations().is_empty(),
            "a launch that recorded nothing was signalled anyway"
        );
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        let binding_state: String = sql
            .query_row(
                "SELECT state FROM runtime_bindings WHERE deployment_id=?1",
                [&fence.deployment_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(binding_state, "released");
        {
            let o = owner.lock().unwrap();
            assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        }
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// Spec §6 step 4: what cannot be proven is not released. The reservation is
    /// retained, the step reads Uncertain and an operator's Stop retries the
    /// termination.
    #[tokio::test]
    async fn an_unprovable_failure_pauses() {
        let (_dir, owner, fence, observations) = setup().await;
        let gate = failing_gate(Some(owner.clone()), "engine exited");
        let tools = ScriptedTool::unprovable();
        let options = native_options();
        let (driver_gate, driver_tools, driver_options) =
            (gate.clone(), tools.clone(), options.clone());
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            options,
            Arc::new(move |_| {
                Ok(native_driver(
                    driver_gate.clone(),
                    driver_tools.clone(),
                    &driver_options,
                ))
            }),
        )
        .unwrap();
        let start = w.start(&fence, 10_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Uncertain
        );
        assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
        assert_eq!(tools.terminations().len(), 1);
        {
            let o = owner.lock().unwrap();
            assert_eq!(
                o.store().resource_snapshot().unwrap().owners.len(),
                1,
                "an unprovable failure released the reservation"
            );
            assert_eq!(
                o.store()
                    .runtime_binding(&fence.deployment_id)
                    .unwrap()
                    .unwrap()
                    .state,
                "uncertain"
            );
        }
        let entries = journal(&owner, start.operation_id());
        assert!(
            entries
                .iter()
                .any(|entry| entry.contains("could not be proven gone")),
            "the uncertainty was not journaled: {entries:?}"
        );
        // SPEC §13.2: the retained uncertainty is reported to whoever waits on the
        // operation as soon as it is recorded, with the coordinator's reason, not
        // after their own wait runs out. Found live on host-a: a paused launch
        // read as "still running" for the full ten-minute bound.
        use crate::port::LifecyclePort;
        let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
        let handle = crate::operations::OperationHandle {
            operation_id: capyctl_domain::identity::OperationId(start.operation_id().to_owned()),
            deployment_id: fence.deployment_id.clone(),
        };
        let waited = tokio::time::timeout(Duration::from_secs(5), port.wait_terminal(&handle))
            .await
            .expect("a retained uncertainty is reported at once");
        match waited {
            Err(crate::fault::LifecycleFault::Uncertain(reason)) => {
                assert!(reason.contains("could not be proven gone"), "{reason}");
            }
            other => panic!("expected the retained uncertainty, got {other:?}"),
        }
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// Spec §5: Stop terminates the recorded group and only then proves it gone.
    /// An engine's own report that it shut down is not evidence, so the proof still
    /// runs — after the signal, not instead of it. T12
    // T12
    #[tokio::test]
    async fn stop_terminates_then_proves_gone() {
        let (_dir, owner, fence, observations) = setup().await;
        let gate = Gate::new(false);
        gate.release.add_permits(16);
        let tools = ScriptedTool::proving();
        let options = CoordinatorOptions::default();
        let (driver_gate, driver_tools, driver_options) =
            (gate.clone(), tools.clone(), options.clone());
        let w = OwnedCoordinator::spawn(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            options,
            Arc::new(move |_| {
                Ok(native_driver(
                    driver_gate.clone(),
                    driver_tools.clone(),
                    &driver_options,
                ))
            }),
        )
        .unwrap();
        let start = w.start(&fence, 10_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Completed
        );
        assert!(tools.terminations().is_empty());
        let stop = w.stop("owner", &fence, "native-stop", 10_000).unwrap();
        assert_eq!(
            stop.wait(Duration::from_secs(60)).await.unwrap(),
            OrdinaryCleanupStatus::Completed
        );
        let terminations = tools.terminations();
        assert_eq!(terminations.len(), 1, "Stop did not terminate the group");
        assert!(!terminations[0].is_empty());
        assert!(w.shared.retained.lock().unwrap().is_empty());
        drop(start);
        w.shutdown().await.unwrap();
    }

    /// Spec §5: cleanup terminates and then proves the group gone, all inside the
    /// protocol bound. A grace that leaves no room for the proof would turn every
    /// Stop into a timeout, so it is refused at construction rather than discovered
    /// on the first Stop. Spec §4 bounds the Initialize timeout the same way.
    #[tokio::test]
    async fn terminate_grace_must_fit_protocol_timeout() {
        let (_dir, owner, _fence, observations) = setup().await;
        let spawn = |options| {
            spawn_fake(
                owner.clone(),
                Arc::new(Observations(observations.clone())),
                Arc::new(|| Ok(1900)),
                options,
            )
        };
        for refused in [
            CoordinatorOptions {
                protocol_timeout: Duration::from_secs(10),
                terminate_grace: Duration::from_secs(8),
                ..Default::default()
            },
            CoordinatorOptions {
                terminate_grace: Duration::from_millis(999),
                ..Default::default()
            },
            CoordinatorOptions {
                initialize_timeout: Duration::from_secs(29),
                ..Default::default()
            },
            CoordinatorOptions {
                initialize_timeout: Duration::from_secs(7_201),
                ..Default::default()
            },
        ] {
            assert!(
                matches!(spawn(refused.clone()), Err(CoordinatorError::Invalid)),
                "these options were accepted: {refused:?}"
            );
        }
        // The same grace inside a protocol bound that fits it is accepted.
        let w = spawn(CoordinatorOptions {
            protocol_timeout: Duration::from_secs(20),
            terminate_grace: Duration::from_secs(8),
            ..Default::default()
        })
        .unwrap();
        w.shutdown().await.unwrap();
    }

    /// T16: the SGLang start path through the production bindings. The start
    /// arms through `ProfileBindings`' own SGLang branch; the resolved-spawn
    /// factory seals both roles before the builder can spawn (the stub tool
    /// checks the store at the moment of the spawn); the step reaches Ready
    /// through a stub engine surface presenting the sealed inference key; and
    /// the release deletes both roles. A stub engine and stub tools prove
    /// nothing about a native engine recipe — only about capyctl's own decisions.
    // T16
    #[tokio::test]
    async fn an_sglang_deployment_seals_both_roles_reaches_ready_and_releases_them() {
        use crate::engine_bindings::ProfileBindings;
        use capyctl_adapters::traits::RenderedCommand;
        use capyctl_config::effective::resolve_effective;
        use capyctl_domain::completion::ProcessIdentity;
        use capyctl_store::secrets::SecretRole;
        use serde_json::{json, Value};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct SealCheckTool {
            owner: SharedCoordinatorState,
            binding: String,
            incarnation: String,
            association: Mutex<Option<Arc<dyn LaunchAssociation + Send + Sync>>>,
            identity: ProcessIdentity,
            group: Vec<ProcessIdentity>,
            spawned: Mutex<Vec<RenderedCommand>>,
            /// What the store held for this binding the moment the builder
            /// spawned: the seal-before-spawn claim, recorded where it holds.
            sealed_at_spawn: Mutex<Option<SealedAtSpawn>>,
        }
        type SealedAtSpawn = (Option<[u8; 32]>, Option<[u8; 32]>);
        impl OwnedProcessLaunch for SealCheckTool {
            fn spawn_durable(
                &self,
                _incarnation: &str,
                _cmd: &RenderedCommand,
            ) -> Result<ProcessIdentity, capyctl_adapters::traits::RuntimeError> {
                Err(capyctl_adapters::traits::RuntimeError::Unsupported)
            }
            fn spawn_durable_protected(
                &self,
                incarnation: &str,
                cmd: &RenderedCommand,
                _descriptors: &capyctl_adapters::protected::ProtectedLaunchDescriptors,
            ) -> Result<ProcessIdentity, capyctl_adapters::traits::RuntimeError> {
                {
                    let owner = self.owner.lock().unwrap();
                    *self.sealed_at_spawn.lock().unwrap() = Some((
                        owner
                            .store()
                            .engine_key(&self.binding, &self.incarnation, SecretRole::Inference)
                            .unwrap(),
                        owner
                            .store()
                            .engine_key(&self.binding, &self.incarnation, SecretRole::Admin)
                            .unwrap(),
                    ));
                }
                self.spawned.lock().unwrap().push(cmd.clone());
                if let Some(association) = self.association.lock().unwrap().clone() {
                    association
                        .persist_api_identity(&self.identity)
                        .map_err(|error| {
                            capyctl_adapters::traits::RuntimeError::Uncertain(error.to_string())
                        })?;
                }
                let _ = incarnation;
                Ok(self.identity.clone())
            }
            fn present(&self, _identity: &ProcessIdentity) -> capyctl_domain::completion::Presence {
                capyctl_domain::completion::Presence::Alive
            }
            fn observe_group(
                &self,
                _api: &ProcessIdentity,
            ) -> Result<Vec<ProcessIdentity>, capyctl_adapters::traits::RuntimeError> {
                Ok(self.group.clone())
            }
            fn terminate_owned(
                &self,
                _identities: &[ProcessIdentity],
                _grace: Duration,
            ) -> Result<(), capyctl_adapters::traits::RuntimeError> {
                Ok(())
            }
        }

        /// A stub engine surface on the leased endpoint: a model list that
        /// names the served model, and a chat stream that answers. It records
        /// every presented authorization, so the test can prove the builder
        /// spoke with the sealed inference key.
        async fn stub_engine(
            listener: std::net::TcpListener,
            model: String,
            seen: Arc<Mutex<Vec<(String, String)>>>,
        ) {
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(pos) =
                                buf.windows(4).position(|window| window == b"\r\n\r\n")
                            {
                                break pos + 4;
                            }
                            if buf.len() > 64 * 1024 {
                                return;
                            }
                        }
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or_default().to_owned();
                let mut content_length = 0usize;
                let mut authorization = String::new();
                for line in lines {
                    if let Some(rest) = line.strip_prefix("content-length:") {
                        content_length = rest.trim().parse().unwrap_or(0);
                    }
                    if let Some(rest) = line.strip_prefix("authorization:") {
                        authorization = rest.trim().to_owned();
                    }
                }
                while buf.len() < head_end + content_length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let mut parts = request_line.split(' ');
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();
                seen.lock()
                    .unwrap()
                    .push((format!("{method} {path}"), authorization));
                let (body, content_type) = if path.starts_with("/v1/models") {
                    (
                        json!({"object":"list","data":[{"id":model,"object":"model"}]}).to_string(),
                        "application/json",
                    )
                } else {
                    let first = json!({
                        "id":"probe","object":"chat.completion.chunk","created":1,"model":model,
                        "choices":[{"index":0,"delta":{"role":"assistant","content":"ready"},
                                    "finish_reason":Value::Null}],
                    });
                    let second = json!({
                        "id":"probe","object":"chat.completion.chunk","created":1,"model":model,
                        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                    });
                    (
                        format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n"),
                        "text/event-stream",
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        }

        // The SGLang golden effective configuration, driven through the real
        // store the worker owns — the same admission production takes. The
        // service clock is real wall-clock time: the builder bounds its own
        // waits by the context deadline the store arms with, and the profile's
        // request deadline caps how far ahead acceptance may set it.
        fn realtime_ms() -> i64 {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        }
        let source: Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-sglang-golden.json"
        ))
        .unwrap();
        let mut host = source["input"]["host"].clone();
        let mut deployment = source["input"]["deployment"].clone();
        // ADR 0014 §7 (WE3): the embedded launch measures a real checkpoint.
        let models = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(models.path().join("toy")).unwrap();
        std::fs::write(models.path().join("toy/config.json"), "{}").unwrap();
        host["model_store"]["path"] = json!(models.path());
        deployment["model"]["path"] = json!(models.path().join("toy"));
        // The store probe-binds a free port of the host's range at lease time,
        // and the stub engine then binds it. The golden fixture's fixed
        // 8100-8199 range is shared with other tests running in parallel, so
        // one could take the leased port in between; this test leases from a
        // range of its own instead, chosen at random below 20000 (clear of the
        // CLI tests' ports and of the kernel's ephemeral range, where outbound
        // connections land).
        const RANGE: u16 = 100;
        let base = {
            let seed = (std::process::id() as u64) ^ (realtime_ms() as u64);
            10_000 + (seed % u64::from(10_000 - RANGE)) as u16
        };
        host["resource_policy"]["endpoint_port_range"] =
            json!({"start": base, "end": base + RANGE - 1});
        let (dir, owner) = {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let owner = Arc::new(Mutex::new(
                crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
            ));
            (dir, owner)
        };
        let (fence, work, observations) = {
            let now = realtime_ms();
            let o = owner.lock().unwrap();
            let session = o.session().clone();
            let store = o.store();
            let policy = resolve_effective(&deployment, &host)
                .expect("fixture resolves")
                .host;
            let observations: Vec<_> = policy
                .domains
                .keys()
                .map(|domain| MemoryObservation {
                    domain: domain.clone(),
                    capacity_bytes: 1_i64 << 50,
                    available_bytes: 1_i64 << 50,
                    sampled_at_ms: now,
                })
                .collect();
            store
                .import_resource_policy(&session, &policy, &observations, now)
                .unwrap();
            let receipt = store
                .create_stopped_managed_configuration(
                    &session,
                    "owner",
                    "toy",
                    &json!({ "config": deployment }).to_string(),
                    &host,
                    now,
                )
                .unwrap();
            let fence = DeploymentFence {
                deployment_id: receipt.deployment_id,
                revision: receipt.revision,
                generation: receipt.generation,
            };
            store
                .accept_start(&session, &fence, now, now + 240_000)
                .unwrap();
            let work = store
                .next_initialize(&session)
                .unwrap()
                .expect("an accepted start plans initialize work");
            (fence, work, observations)
        };
        let binding = work.binding_id().to_owned();
        let incarnation = work.incarnation().to_owned();
        let port: u16 = work
            .endpoint()
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .expect("the leased endpoint names a port");

        // The wrapper is the protected entrypoint this installation carries:
        // a regular, owner-only file in an owner-only directory, exactly what
        // the renderer's revalidation demands.
        let (runtime_dir, log_dir) = {
            use std::os::unix::fs::PermissionsExt;
            let runtime_dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
            std::fs::set_permissions(runtime_dir.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let wrapper = runtime_dir.path().join("sglang_entry.py");
            std::fs::write(
                &wrapper,
                b"# never executed; the stub tools spawn nothing\n",
            )
            .unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o600)).unwrap();
            let log_dir = tempfile::tempdir().unwrap();
            (runtime_dir, log_dir)
        };

        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", port)).expect("the leased endpoint is free");
        tokio::spawn(stub_engine(
            listener,
            // The golden fixture's first route: the served name is the
            // deployment's route name, which the stub must serve for the
            // readiness poll to settle.
            "toy".to_owned(),
            seen.clone(),
        ));

        let identity = ProcessIdentity {
            role: "api".into(),
            pid: 4242,
            boot_id: "boot".into(),
            start_ticks: 99,
        };
        let tool = Arc::new(SealCheckTool {
            owner: owner.clone(),
            binding: binding.clone(),
            incarnation: incarnation.clone(),
            association: Mutex::new(None),
            identity: identity.clone(),
            group: vec![
                identity,
                ProcessIdentity {
                    role: "worker-0".into(),
                    pid: 4243,
                    boot_id: "boot".into(),
                    start_ticks: 100,
                },
            ],
            spawned: Mutex::new(Vec::new()),
            sealed_at_spawn: Mutex::new(None),
        });
        let factory_tool = tool.clone();
        let w = OwnedCoordinator::spawn_resolved(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(realtime_ms())),
            CoordinatorOptions::default(),
            Arc::new(ProfileBindings::new(
                log_dir.path().to_path_buf(),
                runtime_dir.path().to_path_buf(),
            )),
            Arc::new(move |association| {
                *factory_tool.association.lock().unwrap() = Some(association);
                factory_tool.clone() as Arc<dyn capyctl_adapters::traits::OwnedProcessLaunch>
            }),
        )
        .unwrap();

        let step = work.step_id().to_owned();
        let outcome = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match status(&owner, &step) {
                    InitializeStatus::Planned
                    | InitializeStatus::Armed
                    | InitializeStatus::Expired => {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                    settled => return settled,
                }
            }
        })
        .await
        .expect("the SGLang start settles inside the test's bound");
        assert_eq!(
            outcome,
            InitializeStatus::Completed,
            "the SGLang start reached Ready through the stub engine"
        );

        // Both roles were sealed before the builder could spawn, and the
        // builder presented exactly the sealed inference key.
        let (sealed_inference, sealed_admin) =
            (*tool.sealed_at_spawn.lock().unwrap()).expect("spawned");
        assert!(
            sealed_inference.is_some() && sealed_admin.is_some(),
            "both roles must be recoverable from the store at spawn time"
        );
        let sealed_inference = sealed_inference.unwrap();
        let sealed_admin = sealed_admin.unwrap();
        assert_ne!(
            sealed_inference, sealed_admin,
            "the two roles carry distinct keys"
        );
        {
            let o = owner.lock().unwrap();
            assert_eq!(
                o.store()
                    .engine_key(&binding, &incarnation, SecretRole::Inference)
                    .unwrap(),
                Some(sealed_inference)
            );
            assert_eq!(
                o.store()
                    .engine_key(&binding, &incarnation, SecretRole::Admin)
                    .unwrap(),
                Some(sealed_admin)
            );
        }
        let presented: Vec<(String, String)> = seen.lock().unwrap().clone();
        let inference_bearer = format!("Bearer {}", hex::encode(sealed_inference));
        assert!(
            presented
                .iter()
                .any(|(request, authorization)| request.contains("/v1/models")
                    && *authorization == inference_bearer),
            "the readiness poll did not present the sealed inference key: {presented:?}"
        );
        assert!(
            presented.iter().any(|(request, authorization)| request
                .contains("/v1/chat/completions")
                && *authorization == inference_bearer),
            "the probe did not present the sealed inference key: {presented:?}"
        );
        {
            let spawned = tool.spawned.lock().unwrap();
            assert_eq!(spawned.len(), 1);
            assert_eq!(spawned[0].argv[0], "/bin/true");
            assert_eq!(
                spawned[0].argv[2],
                runtime_dir.path().join("sglang_entry.py").to_str().unwrap()
            );
        }

        // The release deletes both roles: no sealed key of any role outlives
        // the binding it was issued for.
        let stop = w
            .stop("owner", &fence, "sglang-stop", realtime_ms() + 10_000)
            .unwrap();
        assert_eq!(
            stop.wait(Duration::from_secs(60)).await.unwrap(),
            OrdinaryCleanupStatus::Completed
        );
        {
            let o = owner.lock().unwrap();
            assert_eq!(
                o.store()
                    .engine_key(&binding, &incarnation, SecretRole::Inference)
                    .unwrap(),
                None,
                "the inference key outlived its binding"
            );
            assert_eq!(
                o.store()
                    .engine_key(&binding, &incarnation, SecretRole::Admin)
                    .unwrap(),
                None,
                "the admin key outlived its binding"
            );
        }
        drop(dir);
        w.shutdown().await.unwrap();
    }

    /// Spec §3: the router must reach the engine the coordinator launched. The
    /// engine answers to the `--served-model-name` the plan rendered, and the
    /// forwarder rewrites every request's model to whatever `runtime_endpoint`
    /// reports, so the two have to be the same string for a deployment that serves
    /// aliases as well as its own name. Both now read the frozen revision's route
    /// list; the store used to order that list by route text instead, which named
    /// a different route as soon as the two orders differed.
    // T19
    #[tokio::test]
    async fn the_endpoint_names_the_model_the_plan_launched() {
        use crate::coordinator_port::CoordinatorLifecycle;
        use crate::engine_bindings::ProfileBindings;
        use crate::port::LifecyclePort;
        use capyctl_adapters::resolve::AdapterSpec;
        use std::os::unix::fs::PermissionsExt;

        let f = capyctl_testkit::fixture::fixture();
        // Written in an order that is not the alphabetical one, which is the case
        // the two reads used to disagree on.
        let fence = capyctl_testkit::fixture::managed_edit(&f, "aliased", |deployment, _| {
            deployment["routes"] = serde_json::json!(["zeta", "alpha"]);
        });
        f.store
            .accept_start(&f.session, &fence, 1100, 300_000)
            .unwrap();
        let work = f
            .store
            .next_initialize(&f.session)
            .unwrap()
            .expect("an accepted start plans initialize work");
        let spec = ProfileBindings::new(
            std::path::PathBuf::from("/tmp/capyctl-test-logs"),
            std::path::PathBuf::from("/tmp/capyctl-test-runtime"),
        )
        .spec(&work)
        .expect("the fixture profile is vllm");
        let AdapterSpec::Vllm { launch, .. } = spec else {
            panic!("the fixture profile declares vllm");
        };
        let launched = launch
            .expect("an owned vllm binding carries a launch plan")
            .served_model_name;

        let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("srv.sqlite3");
        f.sql
            .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = Arc::new(Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
        ));
        // The gate is never released, so the step stays armed and its binding
        // retained for the read; the launch itself is not what this test measures.
        let gate = Gate::new(false);
        let w = worker(
            owner,
            f.observations.clone(),
            gate.clone(),
            CoordinatorOptions::default(),
        );
        let endpoint = CoordinatorLifecycle::new(w.commands())
            .runtime_endpoint(&fence.deployment_id)
            .unwrap()
            .expect("the accepted start retains a binding");

        assert_eq!(
            endpoint.served_model, launched,
            "the router addresses the engine by the name it was launched with"
        );
        assert_eq!(endpoint.served_model, "alpha");
        gate.release.add_permits(16);
        w.shutdown().await.unwrap();
    }
}

/// SPEC §4.3 / §13.3, ADR 0012 migration: an adopted embedded vLLM launch is
/// rebuilt with exactly the keys the store sealed when it launched. A launch
/// that predates the admin role sealed only an inference key; its engine is
/// running with the single-key guard, so the rebuilt adapter keeps that one key
/// and no admin key is invented for it (a fresh one would not match the running
/// engine). A launch that sealed both roles is rebuilt with both. New keys are
/// only ever issued by a new launch.
// T21 T37
#[tokio::test]
async fn an_adopted_vllm_launch_uses_only_the_keys_it_launched_with() {
    use crate::engine_bindings::ProfileBindings;
    use capyctl_adapters::resolve::AdapterSpec;
    use capyctl_store::secrets::SecretRole;

    type SeenKeys = Vec<(Option<String>, Option<String>)>;
    struct Capture {
        profile: ProfileBindings,
        seen: Mutex<SeenKeys>,
    }
    impl EngineBindings for Capture {
        fn spec(&self, work: &InitializeWork) -> Result<AdapterSpec, CoordinatorError> {
            self.profile.spec(work)
        }
        fn adapter(
            &self,
            _declared: Engine,
            spec: AdapterSpec,
            _tools: Arc<dyn OwnedProcessLaunch>,
        ) -> Result<Arc<dyn EngineAdapter>, CoordinatorError> {
            let AdapterSpec::Vllm {
                engine_key,
                admin_key,
                ..
            } = spec
            else {
                panic!("the fixture profile declares vllm");
            };
            self.seen.lock().unwrap().push((engine_key, admin_key));
            Ok(Arc::new(FakeEngine::with_lifecycle()))
        }
    }

    let (_dir, owner, fence, _observations) = setup().await;
    let work = {
        let o = owner.lock().unwrap();
        o.store()
            .accept_start(o.session(), &fence, 1800, 10_000)
            .unwrap();
        o.store()
            .next_initialize(o.session())
            .unwrap()
            .expect("an accepted start plans initialize work")
    };
    let (inference, admin) = (
        capyctl_store::secrets::new_engine_key(),
        capyctl_store::secrets::new_engine_key(),
    );
    let seal = |key: &[u8; 32], role| {
        owner
            .lock()
            .unwrap()
            .store()
            .store_engine_key(work.binding_id(), work.incarnation(), key, role)
            .unwrap();
    };
    let bindings = Arc::new(Capture {
        profile: ProfileBindings::new(
            std::path::PathBuf::from("/tmp/capyctl-test-logs"),
            std::path::PathBuf::from("/tmp/capyctl-test-runtime"),
        ),
        seen: Mutex::new(Vec::new()),
    });
    let factory = super::local_adoption::factory(
        owner.clone(),
        bindings.clone(),
        capyctl_testkit::fake_tools_factory(),
        Arc::new(|| Ok(1900)),
        Duration::from_secs(1),
    );

    // A launch recorded before the admin role existed: one key, the single-key
    // guard, and nothing new sealed by adoption.
    seal(&inference, SecretRole::Inference);
    assert!(
        factory(&work).is_ok(),
        "a single-key launch is still adopted"
    );
    assert_eq!(
        bindings.seen.lock().unwrap().pop().unwrap(),
        (Some(hex::encode(inference)), None),
        "the pre-migration launch keeps its single key and is given no admin key"
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store()
            .engine_key(work.binding_id(), work.incarnation(), SecretRole::Admin)
            .unwrap(),
        None,
        "adoption must never seal a key the running engine was not given"
    );

    // A launch that sealed both roles is rebuilt with both.
    seal(&admin, SecretRole::Admin);
    assert!(factory(&work).is_ok());
    assert_eq!(
        bindings.seen.lock().unwrap().pop().unwrap(),
        (Some(hex::encode(inference)), Some(hex::encode(admin))),
    );
}
