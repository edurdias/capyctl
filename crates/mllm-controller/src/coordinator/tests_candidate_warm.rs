use super::*;

#[tokio::test]
async fn candidate_abort_cancels_each_warm_child_and_parked_read_without_release() {
    for target in [RuntimeAction::Drain,RuntimeAction::Park,RuntimeAction::Inspect,RuntimeAction::Restore,RuntimeAction::ReloadWeights,RuntimeAction::InvalidateCache,RuntimeAction::Probe] {
        eprintln!("Abort warm target: {target:?}");
        failure_case(target,"abort").await;
    }
}

struct WarmFault {
    target: RuntimeAction,
    failure: &'static str,
    entered: Semaphore,
    release: Semaphore,
    database: std::path::PathBuf,
}
struct WarmEngine {
    fake: Arc<FakeEngine>,
    fault: Arc<WarmFault>,
    calls: Mutex<Vec<RuntimeAction>>,
}
#[async_trait::async_trait]
impl EngineAdapter for WarmEngine {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<mllm_domain::qualification::EffectObservation, RuntimeError> {
        self.calls.lock().unwrap().push(command.action);
        let result = self.fake.execute_persisted(command).await;
        if command.action == self.fault.target {
            self.fault.entered.add_permits(1);
            self.fault.release.acquire().await.unwrap().forget();
            assert_ne!(self.fault.failure, "panic", "injected warm panic");
            if self.fault.failure == "lost" {
                return Err(RuntimeError::Uncertain("lost warm reply".into()));
            }
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

async fn security_done(worker: &OwnedCoordinator, sql: &rusqlite::Connection) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let complete: bool = sql.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_runs WHERE json_extract(plan_json,'$.action')='security' AND state='succeeded')", [], |r|r.get(0)).unwrap();
            if complete { break; }
            assert_eq!(worker.status(), WorkerStatus::Running);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

async fn failure_case(target: RuntimeAction, failure: &'static str) {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let fake = Arc::new(FakeEngine::for_qualification_with_clock(Arc::new(|| {
        Ok(1900)
    })));
    let fault = Arc::new(WarmFault {
        target,
        failure,
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
        database: dir.path().join("srv.sqlite3"),
    });
    let engine = Arc::new(WarmEngine {
        fake: fake.clone(),
        fault: fault.clone(),
        calls: Mutex::new(vec![]),
    });
    let factory_engine = engine.clone();
    let factory_fake = fake.clone();
    let factory_fault = fault.clone();
    let builds = Arc::new(AtomicI64::new(0));
    let factory_builds = builds.clone();
    let worker = OwnedCoordinator::spawn_with_candidate_factory(owner.clone(), Arc::new(Observations(observations)), Arc::new(|| Ok(1900)),
        CoordinatorOptions { protocol_timeout:Duration::from_secs(if failure=="timeout" { 3 } else { 30 }), ..CoordinatorOptions::default() }, Arc::new(|_| Err(CoordinatorError::Invalid)),
        Arc::new(move || {
            factory_builds.fetch_add(1,Ordering::SeqCst);
            let probe = factory_fake.clone(); let security = factory_fake.clone(); let status = factory_fake.clone();
            let probe_fault = factory_fault.clone(); let status_fault = factory_fault.clone();
            Ok(Arc::new(candidate::CandidateDriver {
                engine:factory_engine.clone(),
                probe:Arc::new(move |d| { let fake=probe.clone(); let fault=probe_fault.clone(); Box::pin(async move {
                    let warm = d.security_endpoint().is_none() && d.request().contains("MLLM_READY_13");
                    let result = crate::qualification::collect_probe_with_clock(&fake,d,&||Ok(1900)).await.map_err(|e|CoordinatorError::Service(e.to_string()));
                    // Only the second Ready request follows a completed warm cycle.
                    let restoring = fake.qualification_activity().unwrap().1 >= 7;
                    if fault.target==RuntimeAction::Probe && warm && restoring {
                        fault.entered.add_permits(1); fault.release.acquire().await.unwrap().forget();
                        if fault.failure=="lost" { return Err(CoordinatorError::Service("lost warm probe reply".into())); }
                    }
                    result
                }) }),
                security_control:Arc::new(move |d| { let fake=security.clone(); Box::pin(async move { crate::qualification::collect_security_control_with_clock(&fake,d,&||Ok(1900)).await.map_err(|e|CoordinatorError::Service(e.to_string())) }) }),
                parked_status:Arc::new(move |c| {
                    let status = status.clone(); let status_fault = status_fault.clone();
                    Box::pin(async move {
                    let result = crate::qualification::collect_parked_status_with_clock(&status,&c,&||Ok(1900)).map_err(|e|CoordinatorError::Service(e.to_string()));
                    if status_fault.target==RuntimeAction::Inspect {
                        if status_fault.failure=="abort" {
                            status_fault.entered.add_permits(1);
                            status_fault.release.acquire().await.unwrap().forget();
                        } else if status_fault.failure=="commit" {
                            rusqlite::Connection::open(&status_fault.database).unwrap().execute_batch("CREATE TRIGGER fail_status BEFORE INSERT ON qualification_parked_status BEGIN SELECT RAISE(ABORT,'parked status commit failure'); END;").unwrap();
                        } else { return Err(CoordinatorError::Service("parked status unavailable".into())); }
                    }
                    result
                    })
                }),
            }))
        })).unwrap();
    let commands = worker.commands();
    let init = commands
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    settled(&worker, &sql, init.operation_id()).await;
    security_tests::baseline_requests(&worker, &sql, &run, init.deployment_id(), 4).await;
    security_done(&worker, &sql).await;
    let before = owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners;
    let retained = worker
        .shared
        .retained_candidates
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let restore = matches!(
        target,
        RuntimeAction::Restore
            | RuntimeAction::ReloadWeights
            | RuntimeAction::InvalidateCache
            | RuntimeAction::Probe
    );
    if restore {
        let park = commands
            .candidate_action(
                "owner",
                &run,
                1,
                "park",
                400000,
                CandidateLifecycleAction::Park,
            )
            .unwrap();
        settled(&worker, &sql, park.operation_id()).await;
    }
    let action = if restore {
        CandidateLifecycleAction::Restore
    } else {
        CandidateLifecycleAction::Park
    };
    if failure == "missing" {
        worker.shared.retained_candidates.lock().unwrap().clear();
    }
    let accepted = commands
        .candidate_action("owner", &run, 1, "fault", 400000, action)
        .unwrap();
    if failure != "missing" && (target != RuntimeAction::Inspect || failure == "abort") {
        tokio::time::timeout(Duration::from_secs(90), fault.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
    if failure != "missing" && target != RuntimeAction::Inspect {
        assert!(owner.try_lock().is_ok(), "Store guard held over warm await");
    }
    assert_eq!(
        commands
            .candidate_action("owner", &run, 1, "fault", 400000, action)
            .unwrap()
            .operation_id(),
        accepted.operation_id()
    );
    if failure == "abort" {
        super::abort_tests::abort_and_wait(&worker,&sql,&run).await;
        assert_eq!(commands.candidate_action("owner",&run,1,"fault",400000,action).unwrap().operation_id(),accepted.operation_id(),"warm history remains an observation after Abort");
        assert!(Arc::ptr_eq(&retained,worker.shared.retained_candidates.lock().unwrap().values().next().unwrap()));
        let calls = engine.calls.lock().unwrap().clone();
        fault.release.add_permits(1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*engine.calls.lock().unwrap(),calls);
        assert_eq!(builds.load(Ordering::SeqCst),1);
        worker.shutdown().await.unwrap();
        return;
    }
    if failure == "commit" && target != RuntimeAction::Inspect {
        sql.execute_batch("CREATE TRIGGER fail_warm_effect BEFORE INSERT ON lifecycle_evidence BEGIN SELECT RAISE(ABORT,'warm commit failure'); END;").unwrap();
    }
    if failure == "session" {
        owner
            .lock()
            .unwrap()
            .store()
            .begin_coordinator_session()
            .unwrap();
    }
    if !matches!(failure, "shutdown" | "timeout") {
        fault.release.add_permits(1);
    }
    if failure == "caller" {
        drop(commands);
        settled(&worker, &sql, accepted.operation_id()).await;
        worker.shutdown().await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(
            owner
                .lock()
                .unwrap()
                .store()
                .resource_snapshot()
                .unwrap()
                .owners,
            before
        );
        return;
    }
    if failure != "shutdown" {
        tokio::time::timeout(Duration::from_secs(90), async {
            while worker.status() == WorkerStatus::Running {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let shared = worker.shared.clone();
    let status = worker.shutdown().await.unwrap();
    assert!(
        matches!(
            status,
            WorkerStatus::Uncertain { .. } | WorkerStatus::Failed(_)
        ),
        "{target:?} {failure}: {status:?}"
    );
    if failure == "commit" {
        assert!(
            format!("{status:?}").contains("commit failure"),
            "commit scenario must reach actual failed commit: {status:?}"
        );
    }
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    if failure != "missing" {
        assert!(Arc::ptr_eq(
            &retained,
            shared
                .retained_candidates
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
        ));
    }
    let last = if failure == "missing" {
        RuntimeAction::Initialize
    } else if target == RuntimeAction::Probe {
        RuntimeAction::InvalidateCache
    } else if target == RuntimeAction::Inspect {
        RuntimeAction::Park
    } else {
        target
    };
    assert_eq!(engine.calls.lock().unwrap().last(), Some(&last));
    assert_eq!(
        engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|a| **a == last)
            .count(),
        1
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .owners,
        before
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM lifecycle_claims", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM qualifications", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        i64::from(target == RuntimeAction::Probe)
    );
    if failure != "session" {
        assert_eq!(
            commands
                .candidate_action("owner", &run, 1, "fault", 400000, action)
                .unwrap()
                .operation_id(),
            accepted.operation_id()
        );
    }
    assert!(commands
        .candidate_action("owner", &run, 1, "new", 400000, action)
        .is_err());
}

#[tokio::test]
async fn candidate_warm_drain_lost_reply_and_commit_stop_children() {
    for failure in ["lost", "commit"] {
        failure_case(RuntimeAction::Drain, failure).await;
    }
}
#[tokio::test]
async fn candidate_warm_park_lost_reply_and_commit_stop_children() {
    for failure in ["lost", "commit"] {
        failure_case(RuntimeAction::Park, failure).await;
    }
}
#[tokio::test]
async fn candidate_warm_restore_lost_reply_and_commit_stop_children() {
    for failure in ["lost", "commit"] {
        failure_case(RuntimeAction::Restore, failure).await;
    }
}
#[tokio::test]
async fn candidate_warm_reload_lost_reply_and_commit_stop_children() {
    for failure in ["lost", "commit"] {
        failure_case(RuntimeAction::ReloadWeights, failure).await;
    }
}
#[tokio::test]
async fn candidate_warm_invalidate_lost_reply_and_commit_stop_probe() {
    for failure in ["lost", "commit"] {
        failure_case(RuntimeAction::InvalidateCache, failure).await;
    }
}
#[tokio::test]
async fn candidate_warm_shutdown_timeout_panic_and_session_do_not_replay() {
    for failure in ["shutdown", "timeout", "panic", "session"] {
        failure_case(RuntimeAction::Drain, failure).await;
    }
}

#[tokio::test]
async fn candidate_warm_required_probe_lost_reply_and_commit_keep_lease() {
    for failure in ["lost", "commit"] {
        failure_case(RuntimeAction::Probe, failure).await;
    }
}
#[tokio::test]
async fn candidate_warm_parked_status_failure_prevents_restore() {
    for failure in ["lost", "commit"] {
        failure_case(RuntimeAction::Inspect, failure).await;
    }
}
#[tokio::test]
async fn candidate_warm_missing_runtime_and_caller_loss_preserve_authority() {
    failure_case(RuntimeAction::Drain, "missing").await;
    failure_case(RuntimeAction::Park, "caller").await;
}

struct ArmObservationGate {
    values: Vec<MemoryObservation>,
    remaining: Arc<AtomicI64>,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
}
impl ServiceObservation for ArmObservationGate {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.values.clone();
        let remaining = self.remaining.clone();
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            if remaining.fetch_sub(1, Ordering::SeqCst) == 0 {
                entered.add_permits(1);
                release.acquire().await.unwrap().forget();
            }
            Ok(values)
        })
    }
}

#[tokio::test]
async fn candidate_warm_shutdown_while_waiting_for_store_never_creates_new_arm() {
    for probe in [false, true] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let source = Arc::new(ArmObservationGate {
            values: observations,
            remaining: Arc::new(AtomicI64::new(-1)),
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        });
        let worker = OwnedCoordinator::spawn_fake(
            owner.clone(),
            source.clone(),
            Arc::new(|| Ok(1900)),
            CoordinatorOptions::default(),
        )
        .unwrap();
        let commands = worker.commands();
        let init = commands
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        settled(&worker, &sql, init.operation_id()).await;
        security_tests::baseline_requests(&worker, &sql, &run, init.deployment_id(), 4).await;
        security_done(&worker, &sql).await;
        if probe {
            let park = commands
                .candidate_action(
                    "owner",
                    &run,
                    1,
                    "park",
                    400000,
                    CandidateLifecycleAction::Park,
                )
                .unwrap();
            settled(&worker, &sql, park.operation_id()).await;
        }
        source
            .remaining
            .store(if probe { 3 } else { 0 }, Ordering::SeqCst);
        let accepted = commands
            .candidate_action(
                "owner",
                &run,
                1,
                "closing",
                400000,
                if probe {
                    CandidateLifecycleAction::Restore
                } else {
                    CandidateLifecycleAction::Park
                },
            )
            .unwrap();
        source.entered.acquire().await.unwrap().forget();
        let shared = worker.shared.clone();
        let permits = shared
            .store_jobs
            .clone()
            .acquire_many_owned((shared.options.max_observers + 1) as u32)
            .await
            .unwrap();
        source.release.add_permits(1);
        // The worker passes its first admission fence, then waits for Store capacity.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shared.close_admission();
        drop(permits);
        worker.shutdown().await.unwrap();
        let child = if probe {
            accepted.effect_ids().last().unwrap()
        } else {
            &accepted.effect_ids()[0]
        };
        assert_eq!(
            sql.query_row(
                "SELECT state FROM lifecycle_steps WHERE id=?1",
                [child],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "planned",
            "shutdown must prevent a new durable arm for probe={probe}"
        );
        assert_eq!(
            sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
