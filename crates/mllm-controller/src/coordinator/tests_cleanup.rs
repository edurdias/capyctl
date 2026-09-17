use super::*;
use mllm_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus;

#[derive(Clone, Copy)]
enum CleanupOutcome {
    Success,
    Error,
    Panic,
    Timeout,
    WrongEvidence,
}
struct CleanupGate {
    init: Arc<Gate>,
    entered: Semaphore,
    release: Semaphore,
    calls: std::sync::atomic::AtomicUsize,
    active: AtomicBool,
    outcome: CleanupOutcome,
    context: Mutex<Option<CleanupExecutionContext>>,
}
impl CleanupGate {
    fn new(outcome: CleanupOutcome) -> Arc<Self> {
        let init = Gate::new(false);
        init.release.add_permits(1);
        Arc::new(Self {
            init,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
            active: AtomicBool::new(false),
            outcome,
            context: Mutex::new(None),
        })
    }
    fn driver(self: &Arc<Self>) -> Arc<Driver> {
        let gate = self.clone();
        Arc::new(Driver {
            engine: self.init.clone(),
            cleanup: Arc::new(move |context| {
                let gate = gate.clone();
                Box::pin(async move {
                    assert!(!gate.init.active.load(Ordering::SeqCst));
                    gate.calls.fetch_add(1, Ordering::SeqCst);
                    gate.active.store(true, Ordering::SeqCst);
                    *gate.context.lock().unwrap() = Some(context.clone());
                    let _active = Active(&gate.active);
                    let mut evidence = gate
                        .init
                        .engine
                        .lifecycle_cleanup(
                            &context.binding_id,
                            &context.incarnation,
                            &context.identities,
                            true,
                            1900,
                        )
                        .map_err(|e| CoordinatorError::Service(e.to_string()))?;
                    gate.entered.add_permits(1);
                    gate.release.acquire().await.unwrap().forget();
                    match gate.outcome {
                        CleanupOutcome::Error => {
                            return Err(CoordinatorError::Service("lost cleanup reply".into()));
                        }
                        CleanupOutcome::Panic => {
                            panic!("injected cleanup panic after actual effect")
                        }
                        CleanupOutcome::Timeout => std::future::pending::<()>().await,
                        CleanupOutcome::WrongEvidence => {
                            evidence.incarnation = ulid::Ulid::new().to_string()
                        }
                        CleanupOutcome::Success => {}
                    }
                    Ok(evidence)
                })
            }),
        })
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(60), self.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
}
fn cleanup_worker(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    gate: Arc<CleanupGate>,
    options: CoordinatorOptions,
) -> OwnedCoordinator {
    OwnedCoordinator::spawn(
        owner,
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        options,
        Arc::new(move |_| Ok(gate.driver())),
    )
    .unwrap()
}
fn retained_cleanup(owner: &SharedCoordinatorState, fence: &DeploymentFence, step: &str) {
    let o = owner.lock().unwrap();
    assert_eq!(
        o.store().resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Ready
    );
    assert_eq!(
        o.store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
    assert_eq!(
        o.store()
            .ordinary_cleanup_status(o.session(), step, 1900)
            .unwrap(),
        OrdinaryCleanupStatus::Armed
    );
}

#[tokio::test]
async fn cleanup_failures_keep_arm_accounting_instance_and_never_resend() {
    for outcome in [
        CleanupOutcome::Error,
        CleanupOutcome::Panic,
        CleanupOutcome::Timeout,
        CleanupOutcome::WrongEvidence,
    ] {
        let (_dir, owner, fence, observations) = setup().await;
        let gate = CleanupGate::new(outcome);
        let w = cleanup_worker(
            owner.clone(),
            observations,
            gate.clone(),
            CoordinatorOptions {
                protocol_timeout: Duration::from_secs(1),
                ..Default::default()
            },
        );
        let start = w.start(&fence, 10000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(60)).await.unwrap(),
            InitializeStatus::Completed
        );
        let stop = w.stop("owner", &fence, "failure", 10000).unwrap();
        gate.entered().await;
        gate.release.add_permits(1);
        assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
        retained_cleanup(&owner, &fence, stop.step_id());
        assert_eq!(w.shared.retained.lock().unwrap().len(), 1);
        assert!(!gate.active.load(Ordering::SeqCst));
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
        {
            let o = owner.lock().unwrap();
            let retry = o
                .store()
                .accept_ordinary_cleanup(o.session(), "owner", &fence, "failure", 1900, 10000)
                .unwrap();
            assert_eq!(&retry, stop.receipt());
            assert!(o
                .store()
                .next_ordinary_cleanup(o.session())
                .unwrap()
                .is_none());
            assert_eq!(
                o.store()
                    .arm_ordinary_cleanup_with_context(o.session(), stop.step_id(), 1900)
                    .unwrap(),
                (
                    mllm_store::lifecycle::ArmResult::AlreadyRecorded,
                    None
                )
            );
        }
        w.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn stop_at_failure_annotation_boundary_does_not_strand_cleanup() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.lost_reply.store(true, Ordering::SeqCst);
    let session = owner.lock().unwrap().session().clone();
    let store = Arc::new(Mutex::new(
        mllm_store::Store::open(&dir.path().join("srv.sqlite3")).unwrap(),
    ));
    let injected = Arc::new(Mutex::new(None));
    let receipt = injected.clone();
    let target = fence.clone();
    let source = gate.clone();
    let armed = Arc::new(AtomicBool::new(false));
    let armed_clock = armed.clone();
    let failure_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let clock = Arc::new(move || {
        // Inject valid Stop acceptance at the second Store-read clock boundary
        // after the effect exits. Previously this fell between the separate
        // status read and uncertainty annotation, invalidating the latter.
        if armed_clock.load(Ordering::SeqCst)
            && !source.active.load(Ordering::SeqCst)
            && failure_reads.fetch_add(1, Ordering::SeqCst) == 1
        {
            *receipt.lock().unwrap() = Some(
                store
                    .lock()
                    .unwrap()
                    .accept_ordinary_cleanup(
                        &session,
                        "owner",
                        &target,
                        "annotation-race",
                        1900,
                        10000,
                    )
                    .unwrap(),
            );
        }
        Ok(1900)
    });
    let driver = gate.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        clock,
        CoordinatorOptions::default(),
        Arc::new(move |_| Ok(test_driver(driver.clone()))),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    armed.store(true, Ordering::SeqCst);
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let receipt = injected.lock().unwrap().clone();
            if let Some(receipt) = receipt {
                let o = owner.lock().unwrap();
                if o.store()
                    .ordinary_cleanup_status(o.session(), &receipt.step_id, 1900)
                    .unwrap()
                    == OrdinaryCleanupStatus::Completed
                {
                    break;
                }
                assert!(
                    !matches!(w.status(), WorkerStatus::Failed(_)),
                    "annotation race stranded accepted cleanup: {:?}",
                    w.status()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(start);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropped_stop_observer_and_store_poll_still_complete_once() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = CleanupGate::new(CleanupOutcome::Success);
    let w = cleanup_worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions {
            max_observers: 1,
            ..Default::default()
        },
    );
    let start = w.start(&fence, 10000).unwrap();
    start.wait(Duration::from_secs(60)).await.unwrap();
    assert!(matches!(
        w.stop("owner", &fence, "drop", 10000),
        Err(CoordinatorError::Busy)
    ));
    drop(start);
    // Accept through Store without notifying the worker.
    let receipt = {
        let o = owner.lock().unwrap();
        o.store()
            .accept_ordinary_cleanup(o.session(), "owner", &fence, "drop", 1900, 10000)
            .unwrap()
    };
    gate.entered().await;
    let stop = w.stop("owner", &fence, "drop", 10000).unwrap();
    assert_eq!(stop.receipt(), &receipt);
    assert!(matches!(
        w.stop("owner", &fence, "drop", 10000),
        Err(CoordinatorError::Busy)
    ));
    assert!(matches!(
        stop.wait(Duration::from_millis(20)).await,
        Err(CoordinatorError::CallerTimeout)
    ));
    drop(stop);
    gate.release.add_permits(1);
    let retry = w.stop("owner", &fence, "drop", 10000).unwrap();
    assert_eq!(
        retry.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_shutdown_exits_effect_before_releasing_process_lock() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = CleanupGate::new(CleanupOutcome::Success);
    let w = cleanup_worker(
        owner.clone(),
        observations.clone(),
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let start = w.start(&fence, 10000).unwrap();
    start.wait(Duration::from_secs(60)).await.unwrap();
    let stop = w.stop("owner", &fence, "shutdown", 10000).unwrap();
    gate.entered().await;
    drop(start);
    drop(owner);
    assert!(crate::ownership::OwnedCoordinatorState::open(dir.path()).is_err());
    assert!(matches!(
        w.shutdown().await.unwrap(),
        WorkerStatus::Uncertain { .. }
    ));
    assert!(!gate.active.load(Ordering::SeqCst));
    // The observer keeps the exact ownership alive through its outstanding reads.
    assert!(crate::ownership::OwnedCoordinatorState::open(dir.path()).is_err());
    let original = stop.receipt().clone();
    drop(stop);
    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    let restarted = cleanup_worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    let replay = restarted.stop("owner", &fence, "shutdown", 10000).unwrap();
    assert_eq!(replay.receipt(), &original);
    assert_eq!(
        replay.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Uncertain
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .owners[&fence.deployment_id]
            .phase,
        ResourcePhase::Ready
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_completion_rollback_halts_without_releasing_any_authority() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = CleanupGate::new(CleanupOutcome::Success);
    let w = cleanup_worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let start = w.start(&fence, 10000).unwrap();
    start.wait(Duration::from_secs(60)).await.unwrap();
    let stop = w.stop("owner", &fence, "rollback", 10000).unwrap();
    gate.entered().await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_cleanup_event BEFORE INSERT ON management_events WHEN NEW.kind='ordinary_cleanup_completed' BEGIN SELECT RAISE(ABORT,'injected completion rollback'); END;").unwrap();
    let epoch = owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .epoch;
    gate.release.add_permits(1);
    assert!(matches!(stopped(&w).await, WorkerStatus::Failed(_)));
    retained_cleanup(&owner, &fence, stop.step_id());
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .epoch,
        epoch
    );
    assert_eq!(w.shared.retained.lock().unwrap().len(), 1);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert!(w.stop("owner", &fence, "rollback", 10000).is_err());
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_final_clock_expiry_denies_control_after_valid_store_check() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = CleanupGate::new(CleanupOutcome::Success);
    let driver = gate.clone();
    let clock_owner = owner.clone();
    let expired = Arc::new(AtomicBool::new(false));
    let hit = expired.clone();
    let sql = Arc::new(Mutex::new(
        rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap(),
    ));
    let clock = Arc::new(move || {
        if clock_owner.try_lock().is_ok() {
            let armed: bool = sql.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE o.kind='ordinary_cleanup' AND s.state='armed')", [], |r| r.get(0)).unwrap();
            if armed {
                hit.store(true, Ordering::SeqCst);
                return Ok(10000);
            }
        }
        Ok(1900)
    });
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        clock,
        CoordinatorOptions::default(),
        Arc::new(move |_| Ok(driver.driver())),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    start.wait(Duration::from_secs(60)).await.unwrap();
    let stop = w.stop("owner", &fence, "deadline", 10000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert!(expired.load(Ordering::SeqCst));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    retained_cleanup(&owner, &fence, stop.step_id());
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_observer_rejects_corrupt_terminal_evidence_epoch_and_released_binding() {
    let (dir, owner, fence, observations) = setup().await;
    let w = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    start.wait(Duration::from_secs(60)).await.unwrap();
    let stop = w.stop("owner", &fence, "terminal", 10000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    for (select, update, corrupt) in [
        (
            "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
            "UPDATE lifecycle_evidence SET evidence_json=?2 WHERE step_id=?1",
            "{}",
        ),
        (
            "SELECT CAST(committed_epoch AS TEXT) FROM lifecycle_evidence WHERE step_id=?1",
            "UPDATE lifecycle_evidence SET committed_epoch=?2 WHERE step_id=?1",
            "0",
        ),
        (
            "SELECT b.state FROM runtime_bindings b JOIN lifecycle_steps s ON s.binding_id=b.id WHERE s.id=?1",
            "UPDATE runtime_bindings SET state=?2 WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE id=?1)",
            "uncertain",
        ),
    ] {
        let original: String = sql
            .query_row(select, [stop.step_id()], |r| r.get(0))
            .unwrap();
        sql.execute(update, [stop.step_id(), corrupt]).unwrap();
        {
            let o = owner.lock().unwrap();
            assert!(
                o.store()
                    .ordinary_cleanup_status(o.session(), stop.step_id(), 1900)
                    .is_err(),
                "{update}"
            );
        }
        sql.execute(update, [stop.step_id(), &original]).unwrap();
    }
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_send_revalidation_rejects_changed_context_and_current_fences() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = CleanupGate::new(CleanupOutcome::Success);
    let w = cleanup_worker(
        owner.clone(),
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let start = w.start(&fence, 10000).unwrap();
    start.wait(Duration::from_secs(60)).await.unwrap();
    let stop = w.stop("owner", &fence, "revalidate", 10000).unwrap();
    gate.entered().await;
    let context = gate.context.lock().unwrap().clone().unwrap();
    {
        let o = owner.lock().unwrap();
        assert_eq!(
            o.store()
                .revalidate_ordinary_cleanup_send(o.session(), stop.step_id(), &context, 1900)
                .unwrap(),
            2000
        );
        for i in 0..6 {
            let mut changed = context.clone();
            match i {
                0 => changed.binding_id = ulid::Ulid::new().to_string(),
                1 => changed.incarnation = ulid::Ulid::new().to_string(),
                2 => changed.fence.generation += 1,
                3 => changed.identities.clear(),
                4 => changed.operation_id = ulid::Ulid::new().to_string(),
                _ => changed.deadline_ms += 1,
            }
            assert!(o
                .store()
                .revalidate_ordinary_cleanup_send(o.session(), stop.step_id(), &changed, 1900)
                .is_err());
        }
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        for (mutation, restore) in [
            (
                "UPDATE deployments SET current_generation=current_generation+1 WHERE id=?1",
                "UPDATE deployments SET current_generation=current_generation-1 WHERE id=?1",
            ),
            (
                "UPDATE lifecycle_claims SET generation=generation+1 WHERE deployment_id=?1",
                "UPDATE lifecycle_claims SET generation=generation-1 WHERE deployment_id=?1",
            ),
            (
                "UPDATE endpoint_leases SET host='foreign' WHERE binding_id=(SELECT id FROM runtime_bindings WHERE deployment_id=?1 AND state!='released')",
                "UPDATE endpoint_leases SET host='127.0.0.1' WHERE binding_id=(SELECT id FROM runtime_bindings WHERE deployment_id=?1 AND state!='released')",
            ),
        ] {
            sql.execute(mutation, [&fence.deployment_id]).unwrap();
            assert!(
                o.store()
                    .revalidate_ordinary_cleanup_send(o.session(), stop.step_id(), &context, 1900)
                    .is_err(),
                "{mutation}"
            );
            sql.execute(restore, [&fence.deployment_id]).unwrap();
            assert!(
                o.store()
                    .revalidate_ordinary_cleanup_send(o.session(), stop.step_id(), &context, 1900)
                    .is_ok()
            );
        }
    }
    gate.release.add_permits(1);
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_does_not_refresh_expired_default_ttl_evidence() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = CleanupGate::new(CleanupOutcome::Success);
    let driver = gate.clone();
    let now = Arc::new(AtomicI64::new(1900));
    let clock = now.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(move || Ok(clock.load(Ordering::SeqCst))),
        CoordinatorOptions::default(),
        Arc::new(move |_| Ok(driver.driver())),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    start.wait(Duration::from_secs(60)).await.unwrap();
    let stop = w.stop("owner", &fence, "expired-evidence", 10000).unwrap();
    gate.entered().await;
    now.store(3901, Ordering::SeqCst);
    gate.release.add_permits(1);
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    retained_cleanup(&owner, &fence, stop.step_id());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_after_associated_initialize_uncertainty_keeps_original_worker() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.lost_reply.store(true, Ordering::SeqCst);
    let w = worker(
        owner.clone(),
        observations.clone(),
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let start = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    gate.release.add_permits(1);
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert_eq!(
        start.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Uncertain
    );
    assert!(w.start(&fence, 10000).is_err());
    assert!(OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default()
    )
    .is_err());
    let stop = w.stop("owner", &fence, "uncertain-stop", 10000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert!(!gate.active.load(Ordering::SeqCst));
    assert!(w.shared.retained.lock().unwrap().is_empty());
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_claim_handoff_waits_for_running_initialize_exit() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    let cleanup_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let init = gate.clone();
    let calls = cleanup_calls.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(move |_| {
            let driver = init.clone();
            let calls = calls.clone();
            Ok(Arc::new(Driver {
                engine: init.clone(),
                cleanup: Arc::new(move |context| {
                    let driver = driver.clone();
                    let calls = calls.clone();
                    Box::pin(async move {
                        assert!(
                            !driver.active.load(Ordering::SeqCst),
                            "cleanup overlapped Initialize"
                        );
                        calls.fetch_add(1, Ordering::SeqCst);
                        driver
                            .engine
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
            }))
        }),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    let stop = w.stop("owner", &fence, "during-initialize", 10000).unwrap();
    assert!(matches!(
        stop.wait(Duration::from_millis(20)).await,
        Err(CoordinatorError::CallerTimeout)
    ));
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 0);
    assert!(gate.active.load(Ordering::SeqCst));
    assert_peak(&owner, &fence);
    gate.release.add_permits(1);
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    assert!(!gate.active.load(Ordering::SeqCst));
    drop(start);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn earlier_other_cleanup_does_not_hide_running_initialize_successor() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let first = Gate::new(false);
    first.release.add_permits(1);
    let second = Gate::new(false);
    *second.association.lock().unwrap() = Some(owner.clone());
    let first_id = fence.deployment_id.clone();
    let second_driver = second.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(move |work| {
            Ok(test_driver(if work.fence().deployment_id == first_id {
                first.clone()
            } else {
                second_driver.clone()
            }))
        }),
    )
    .unwrap();
    let a = w.start(&fence, 10000).unwrap();
    assert_eq!(
        a.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    let b = w.start(&other, 10000).unwrap();
    second.entered().await;
    let stop_a = w.stop("owner", &fence, "first-stop", 10000).unwrap();
    let stop_b = w.stop("owner", &other, "second-stop", 10000).unwrap();
    second.release.add_permits(1);
    assert_eq!(
        stop_a.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert_eq!(
        stop_b.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert!(w.shared.retained.lock().unwrap().is_empty());
    drop(b);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_start_ready_stop_releases_and_replays_original_receipt() {
    let (_dir, owner, fence, observations) = setup().await;
    let w = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    let stop = w.stop("owner", &fence, "stop-once", 10000).unwrap();
    let receipt = stop.receipt().clone();
    assert_eq!(receipt.generation, fence.generation + 1);
    let retry = w.stop("owner", &fence, "stop-once", 10000).unwrap();
    assert_eq!(retry.receipt(), &receipt);
    assert!(w.stop("owner", &fence, "different", 10000).is_err());
    drop(stop);
    assert_eq!(
        retry.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert!(w.shared.retained.lock().unwrap().is_empty());
    assert!(!owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(&fence.deployment_id));
    let retry = w.stop("owner", &fence, "stop-once", 10000).unwrap();
    assert_eq!(retry.receipt(), &receipt);
    assert_eq!(
        retry.wait(Duration::from_secs(10)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    let next = {
        use serde_json::{json, Value};
        let golden: Value = serde_json::from_str(include_str!(
            "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
        ))
        .unwrap();
        let mut config = golden["input"]["deployment"].clone();
        let mut host = golden["input"]["host"].clone();
        config["name"] = json!("ordinary");
        config["routes"] = json!(["ordinary-replaced"]);
        host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
        host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
            json!("secret://another-admin");
        let o = owner.lock().unwrap();
        let replacement = o
            .store()
            .replace_stopped_managed_configuration(
                o.session(),
                "owner",
                "replace-worker-cleaned",
                &fence.deployment_id,
                &json!({"expected_revision":1,"config":config}).to_string(),
                &host,
                1900,
            )
            .unwrap();
        DeploymentFence {
            deployment_id: fence.deployment_id.clone(),
            revision: replacement.revision,
            generation: replacement.generation,
        }
    };
    let fresh = w.start(&next, 10000).unwrap();
    assert_eq!(
        fresh.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert!(!w
        .shared
        .retained
        .lock()
        .unwrap()
        .contains_key(&receipt.binding_id));
    assert_eq!(w.shared.retained.lock().unwrap().len(), 1);
    let old = w.stop("owner", &fence, "stop-once", 10000).unwrap();
    assert_eq!(old.receipt(), &receipt);
    assert_eq!(
        old.wait(Duration::from_secs(10)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    let new_stop = w.stop("owner", &next, "fresh-stop", 10000).unwrap();
    assert_ne!(new_stop.receipt().binding_id, receipt.binding_id);
    assert_ne!(new_stop.receipt().incarnation, receipt.incarnation);
    assert_eq!(
        new_stop.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    w.shutdown().await.unwrap();
}

/// ADR 0011 decision 4: a deployment that fails closes its own admission, not the
/// host's. Before this, the worker returned on any failed step and the task closed
/// admission for every deployment, so one bad configuration stopped everything.
#[tokio::test]
async fn a_failed_deployment_does_not_stop_the_others() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let failing_id = fence.deployment_id.clone();
    let gate = Gate::new(false);
    // The healthy deployment's Initialize is allowed to finish; only the first
    // deployment is misconfigured.
    gate.release.add_permits(1);
    let driver = gate.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        // ADR 0011 decision 5: the misconfigured deployment is retried until its
        // budget is spent before its admission closes. The cooldown is shortened
        // so the test does not wait for the policy default.
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
        Arc::new(move |work| {
            // Only the first deployment's binding is ever misconfigured. The
            // driver is never even constructed for it.
            if work.fence().deployment_id == failing_id {
                return Err(CoordinatorError::Service(
                    "injected qualification failure".into(),
                ));
            }
            Ok(test_driver(driver.clone()))
        }),
    )
    .unwrap();
    let failing = w.start(&fence, 10000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    // The failing deployment's own Initialize never arms; it stays Planned, and
    // this deployment's own admission closes. Nothing about the host does.
    tokio::time::timeout(Duration::from_secs(10), async {
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
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Read the durable state rather than the observer: a fenced status read
    // requires an admitting deployment, and this one just closed its own
    // admission. The step is still planned, holds no grant and produced no
    // evidence, so nothing was executed for it.
    let durable: (String, bool, i64, i64) = sql
        .query_row(
            "SELECT s.state,s.grant_id IS NULL,(SELECT COUNT(*) FROM lifecycle_evidence WHERE step_id=s.id),d.admission_enabled FROM lifecycle_steps s JOIN deployments d ON d.id=s.deployment_id WHERE s.id=?1",
            [failing.step_id()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(durable, ("planned".into(), true, 0, 0));
    // The coordinator itself is unaffected: it is still Running, not Stopped.
    assert_eq!(w.status(), WorkerStatus::Running);
    // A second, healthy deployment still starts and reaches Ready.
    let healthy = w.start(&other, 10000).unwrap();
    assert_eq!(
        healthy.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    w.shutdown().await.unwrap();
}
