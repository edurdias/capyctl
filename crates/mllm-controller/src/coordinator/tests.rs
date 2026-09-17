use super::*;
use mllm_adapters::traits::*;
use mllm_domain::resources::ResourcePhase;
use std::sync::{atomic::AtomicI64, Mutex};

#[path = "tests_cleanup.rs"]
mod cleanup;

#[path = "tests_start_command.rs"]
mod start_command;

#[path = "../../tests/support/fixture.rs"]
mod fixture;

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
    lost_reply: AtomicBool,
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
            lost_reply: AtomicBool::new(false),
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
    ) -> Result<mllm_domain::completion::EffectObservation, RuntimeError> {
        self.calls.lock().unwrap().push(command.action);
        self.active.store(true, Ordering::SeqCst);
        let _active = Active(&self.active);
        let result = self.engine.execute_persisted(command).await;
        if let (Some(owner), Ok(observation)) = (&*self.association.lock().unwrap(), &result) {
            let o = owner.lock().unwrap();
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
        self.entered.add_permits(1);
        assert!(!self.panic, "injected adapter panic after effect");
        self.release.acquire().await.unwrap().forget();
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
            CoordinatorOptions {
                protocol_timeout: Duration::from_millis(30),
                ..Default::default()
            },
        );
        let a = w.start(&fence, 10000).unwrap();
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
        revoked.device_sharing = mllm_config::effective::Sharing::Exclusive;
        for sharing in revoked.device_sharing_overrides.values_mut() {
            *sharing = mllm_config::effective::Sharing::Exclusive;
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
    assert!(OwnedCoordinator::spawn_fake(
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
                protocol_timeout: Duration::from_millis(200),
                // The denial is retried until the budget is spent; the test does
                // not wait for the policy default between attempts.
                retry_cooldown: Duration::from_millis(20),
                ..Default::default()
            },
            Arc::new(move |_| Ok(test_driver(driver.clone()))),
        )
        .unwrap();
        let a = w.start(&fence, 10000).unwrap();
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
    let (_dir, owner, fence, observations) = setup().await;
    let armed = Arc::new(AtomicBool::new(false));
    let arm_seen = armed.clone();
    let clock_owner = owner.clone();
    // Service time advances only at the final clock read outside Store. This
    // simulates costly provenance validation consuming the observation window.
    let clock = Arc::new(move || {
        if let Ok(o) = clock_owner.try_lock() {
            if !o.store().resource_snapshot().unwrap().owners.is_empty() {
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
    // retained with no tracked cleanup for it, so this deployment's admission
    // closes — but that is this deployment's own accounting, not a process-wide
    // fault, so the coordinator keeps running for every other deployment.
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
    let w = OwnedCoordinator::spawn_fake(
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
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    let admission = || {
        mllm_scheduler::residency::AdmissionContext::new(
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
            "UPDATE deployments SET dispatch_enabled=1 WHERE id=(SELECT deployment_id FROM lifecycle_steps WHERE id=?1)",
            "UPDATE deployments SET dispatch_enabled=0 WHERE id=(SELECT deployment_id FROM lifecycle_steps WHERE id=?1)",
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
    let w = OwnedCoordinator::spawn_fake(
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
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect();
        let result = o
            .store()
            .arm_step(
                o.session(),
                &accepted.step_id,
                mllm_scheduler::residency::AdmissionContext::new(
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
        assert!(o
            .store()
            .next_initialize(o.session())
            .unwrap()
            .is_some());
    });
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
    for observation in &mut observations {
        observation.sampled_at_ms = now();
    }
    let (arm, context) = stage(&owner, "arm-and-context", |o| {
        o.store()
            .arm_initialize_with_context(
                o.session(),
                &accepted.step_id,
                mllm_scheduler::residency::AdmissionContext::new(
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

/// Retrying an effect that may have landed can start a second engine while the
/// first still holds memory. SPEC §13.2 and ADR 0011 decision 5: an uncertain
/// attempt is not counted and not retried until the recorded processes are proven
/// gone. T20
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
        mllm_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Completed
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

mod cleanup_evidence {
    use super::super::*;
    use mllm_domain::completion::ProcessIdentity;
    use mllm_store::ordinary_lifecycle::cleanup::{CleanupExecutionContext, CleanupMode};

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
            fence: mllm_store::lifecycle::DeploymentFence {
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
