use super::*;
use mllm_store::ordinary_lifecycle::QualifiedStartReceipt;
use std::sync::atomic::AtomicUsize;

fn command_worker(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    gate: Arc<Gate>,
    factories: Arc<AtomicUsize>,
    clock: ServiceClock,
) -> OwnedCoordinator {
    OwnedCoordinator::spawn(
        owner,
        Arc::new(Observations(observations)),
        clock,
        CoordinatorOptions::default(),
        Arc::new(move |_| {
            factories.fetch_add(1, Ordering::SeqCst);
            Ok(test_driver(gate.clone()))
        }),
    )
    .unwrap()
}

async fn completed(owner: &SharedCoordinatorState, receipt: &QualifiedStartReceipt) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = {
                let o = owner.lock().unwrap();
                o.store()
                    .qualified_initialize_status(o.session(), receipt.step_id(), 1900)
                    .unwrap()
            };
            if status == QualifiedInitializeStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

fn counts(dir: &tempfile::TempDir) -> (i64, i64, i64, i64) {
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.query_row(
        "SELECT (SELECT COUNT(*) FROM command_receipts), (SELECT COUNT(*) FROM operations), (SELECT COUNT(*) FROM lifecycle_steps), (SELECT SUM(current_generation) FROM deployments)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

#[tokio::test]
async fn scoped_start_joins_once_and_history_survives_cleanup_shutdown_and_handle_drop() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let factories = Arc::new(AtomicUsize::new(0));
    let now = Arc::new(AtomicI64::new(1900));
    let service_now = now.clone();
    let w = command_worker(
        owner.clone(),
        observations,
        gate.clone(),
        factories.clone(),
        Arc::new(move || Ok(service_now.load(Ordering::SeqCst))),
    );
    let handle = w.commands();
    let receipt = handle
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap();
    assert!(!receipt.joined());
    assert_eq!(receipt.accepted_at_ms(), 1900);
    assert_eq!(receipt.deadline_ms(), 10000);
    assert_eq!(
        handle
            .start("owner", &fence.deployment_id, 1, "start", 10000)
            .unwrap(),
        receipt
    );
    let joined = handle
        .start("owner", &fence.deployment_id, 1, "join", 11000)
        .unwrap();
    assert!(joined.joined());
    assert_eq!(joined.operation_id(), receipt.operation_id());
    assert_eq!(joined.deadline_ms(), 10000);
    drop(handle);
    gate.entered().await;
    let handle = w.commands();
    gate.release.add_permits(1);
    completed(&owner, &receipt).await;
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
    let before = counts(&dir);
    assert_eq!(
        handle
            .start("owner", &fence.deployment_id, 1, "start", 10000)
            .unwrap(),
        receipt
    );
    assert_eq!(counts(&dir), before);
    let stop = w.stop("owner", &fence, "stop", 10000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    drop(stop);
    assert!(w.shared.retained.lock().unwrap().is_empty());
    let before = counts(&dir);
    assert_eq!(
        handle
            .start("owner", &fence.deployment_id, 1, "start", 10000)
            .unwrap(),
        receipt
    );
    assert_eq!(w.shutdown().await.unwrap(), WorkerStatus::Stopped);
    // Original absolute deadlines identify history even after they have elapsed.
    now.store(90000, Ordering::SeqCst);
    assert_eq!(
        handle
            .clone()
            .start("owner", &fence.deployment_id, 1, "join", 11000)
            .unwrap(),
        joined
    );
    assert!(matches!(
        handle.start("owner", &fence.deployment_id, 1, "new", 10000),
        Err(CoordinatorCommandError::Coordinator(
            CoordinatorError::Stopped(_)
        ))
    ));
    assert_eq!(counts(&dir), before);
    assert_eq!(factories.load(Ordering::SeqCst), 1);
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    assert!(!gate.active.load(Ordering::SeqCst));
    drop(owner);
    assert!(crate::ownership::OwnedCoordinatorState::open(dir.path()).is_err());
    drop(handle);
    assert!(crate::ownership::OwnedCoordinatorState::open(dir.path()).is_ok());
}

#[tokio::test]
async fn scoped_start_paused_history_is_read_only_and_conflicts_and_corruption_stay_strict() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.lost_reply.store(true, Ordering::SeqCst);
    let factories = Arc::new(AtomicUsize::new(0));
    let w = command_worker(
        owner.clone(),
        observations,
        gate.clone(),
        factories.clone(),
        Arc::new(|| Ok(1900)),
    );
    let handle = w.commands();
    let receipt = handle
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap();
    gate.entered().await;
    gate.release.add_permits(1);
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    let before = counts(&dir);
    assert_eq!(
        handle
            .start("owner", &fence.deployment_id, 1, "start", 10000)
            .unwrap(),
        receipt
    );
    assert!(matches!(
        handle.start("owner", &fence.deployment_id, 1, "new", 10000),
        Err(CoordinatorCommandError::Coordinator(
            CoordinatorError::Stopped(_)
        ))
    ));
    assert!(matches!(
        handle.start("owner", &fence.deployment_id, 1, "start", 10001),
        Err(CoordinatorCommandError::Lifecycle(LifecycleError::Conflict))
    ));
    assert_eq!(counts(&dir), before);
    w.shutdown().await.unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "UPDATE command_receipts SET response_json='{}' WHERE idempotency_key='start'",
        [],
    )
    .unwrap();
    assert!(matches!(
        handle.start("owner", &fence.deployment_id, 1, "start", 10000),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::CorruptStoredData
        ))
    ));
    assert_eq!(counts(&dir), before);
    assert_eq!(factories.load(Ordering::SeqCst), 1);
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn scoped_start_current_session_is_required_even_for_stopped_history() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner.clone(),
        observations,
        gate,
        CoordinatorOptions::default(),
    );
    let handle = w.commands();
    handle
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap();
    w.shutdown().await.unwrap();
    owner
        .lock()
        .unwrap()
        .store()
        .begin_coordinator_session()
        .unwrap();
    let before = counts(&dir);
    for key in ["start", "new"] {
        let error = handle
            .start("owner", &fence.deployment_id, 1, key, 10000)
            .unwrap_err();
        assert!(
            matches!(
                error,
                CoordinatorCommandError::Lifecycle(LifecycleError::Stale)
            ),
            "{error}"
        );
    }
    assert_eq!(counts(&dir), before);
}

#[tokio::test]
async fn scoped_start_capacity_rejects_without_mutation_and_releases_after_errors() {
    let (dir, owner, fence, observations) = setup().await;
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
    let handle = w.commands();
    let observer = w.start(&fence, 10000).unwrap();
    let before = counts(&dir);
    assert!(matches!(
        handle.start("owner", &fence.deployment_id, 1, "start", 10000),
        Err(CoordinatorCommandError::Coordinator(CoordinatorError::Busy))
    ));
    assert_eq!(counts(&dir), before);
    drop(observer);
    assert!(matches!(
        handle.start("", &fence.deployment_id, 1, "bad", 10000),
        Err(CoordinatorCommandError::Lifecycle(LifecycleError::Invalid))
    ));
    assert!(handle
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap()
        .joined());
    assert_eq!(w.shared.observers.available_permits(), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn scoped_start_same_key_concurrency_commits_one_receipt() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let w = worker(
        owner,
        observations,
        gate.clone(),
        CoordinatorOptions::default(),
    );
    let first = w.commands();
    let second = first.clone();
    let id = fence.deployment_id.clone();
    let other = id.clone();
    let before = counts(&dir);
    let (a, b) = tokio::join!(
        tokio::task::spawn_blocking(move || first.start("owner", &id, 1, "race", 10000)),
        tokio::task::spawn_blocking(move || second.start("owner", &other, 1, "race", 10000)),
    );
    assert_eq!(a.unwrap().unwrap(), b.unwrap().unwrap());
    let after = counts(&dir);
    assert_eq!(after.0, before.0 + 1);
    assert_eq!(after.1, before.1 + 1);
    assert_eq!(after.2, before.2 + 1);
    gate.entered().await;
    w.shutdown().await.unwrap();
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn scoped_start_shutdown_and_drop_wait_for_the_admission_transaction() {
    for explicit_shutdown in [true, false] {
        let (dir, owner, fence, observations) = setup().await;
        let gate = Gate::new(false);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        let clock = Arc::new(move || {
            if std::thread::current().name() == Some("scoped-command") {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
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
        let shared = w.shared.clone();
        let handle = w.commands();
        let command = handle.clone();
        let id = fence.deployment_id.clone();
        let command_thread = std::thread::Builder::new()
            .name("scoped-command".into())
            .spawn(move || command.start("owner", &id, 1, "race", 10000))
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (closing_tx, closing_rx) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Handle::current();
        let close = tokio::task::spawn_blocking(move || {
            closing_tx.send(()).unwrap();
            if explicit_shutdown {
                runtime.block_on(w.shutdown()).unwrap();
            } else {
                drop(w);
            }
        });
        closing_rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Admission is still inside the trusted clock under the Store mutex.
        // Closing admission must wait for that whole boundary, not race its commit.
        let still_accepting = shared.accepting.load(Ordering::Acquire);
        release_tx.send(()).unwrap();
        let receipt = command_thread.join().unwrap().unwrap();
        close.await.unwrap();
        // Drop requests shutdown, while explicit shutdown also joins the worker.
        if !explicit_shutdown {
            tokio::time::timeout(Duration::from_secs(10), async {
                while Arc::strong_count(&shared) > 2 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
        let before = counts(&dir);
        assert_eq!(
            handle
                .start("owner", &fence.deployment_id, 1, "race", 10000)
                .unwrap(),
            receipt
        );
        assert!(matches!(
            handle.start("owner", &fence.deployment_id, 1, "after", 10000),
            Err(CoordinatorCommandError::Coordinator(
                CoordinatorError::Stopped(_)
            ))
        ));
        assert_eq!(counts(&dir), before);
        assert!(!gate.active.load(Ordering::SeqCst));
        assert!(still_accepting, "shutdown closed admission before the in-progress command could commit; explicit={explicit_shutdown}");
    }
}

#[tokio::test]
async fn scoped_start_cancelled_caller_cannot_cancel_committed_execution() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let factories = Arc::new(AtomicUsize::new(0));
    let w = command_worker(
        owner.clone(),
        observations,
        gate.clone(),
        factories.clone(),
        Arc::new(|| Ok(1900)),
    );
    let handle = w.commands();
    let id = fence.deployment_id.clone();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(async move {
        let receipt = handle.start("owner", &id, 1, "cancel", 10000).unwrap();
        accepted_tx.send(receipt).unwrap();
        std::future::pending::<()>().await;
        drop(handle);
    });
    let receipt = accepted_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    gate.entered().await;
    gate.release.add_permits(1);
    completed(&owner, &receipt).await;
    assert_eq!(
        w.shared.observers.available_permits(),
        CoordinatorOptions::default().max_observers
    );
    assert_eq!(factories.load(Ordering::SeqCst), 1);
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn scoped_start_store_queue_failure_waits_for_in_progress_admission() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    let clock = Arc::new(move || {
        if std::thread::current().name() == Some("fatal-command") {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
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
    // Suspend the real worker inside Initialize, so its next Store read cannot
    // compete with the deliberately failed outside-lock read below.
    let observer = w.start(&fence, 10000).unwrap();
    gate.entered().await;
    drop(observer);
    let handle = w.commands();
    let command = handle.clone();
    let id = fence.deployment_id.clone();
    let command_thread = std::thread::Builder::new()
        .name("fatal-command".into())
        .spawn(move || command.start("owner", &id, 1, "race", 10000))
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    w.shared.store_jobs.close();
    let shared = w.shared.clone();
    let runtime = tokio::runtime::Handle::current();
    let (failing_tx, failing_rx) = tokio::sync::oneshot::channel();
    let failed_read = tokio::task::spawn_blocking(move || {
        failing_tx.send(()).unwrap();
        runtime.block_on(shared.read(|_, _| Ok(())))
    });
    failing_rx.await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let still_accepting = w.shared.accepting.load(Ordering::Acquire);
    release_tx.send(()).unwrap();
    let receipt = command_thread.join().unwrap().unwrap();
    assert!(receipt.joined());
    assert!(matches!(
        failed_read.await.unwrap(),
        Err(CoordinatorError::Service(_))
    ));
    let before = counts(&dir);
    assert_eq!(
        handle
            .start("owner", &fence.deployment_id, 1, "race", 10000)
            .unwrap(),
        receipt
    );
    assert!(matches!(
        handle.start("owner", &fence.deployment_id, 1, "new", 10000),
        Err(CoordinatorCommandError::Coordinator(
            CoordinatorError::Stopped(_)
        ))
    ));
    assert_eq!(counts(&dir), before);
    w.shutdown().await.unwrap();
    assert!(!gate.active.load(Ordering::SeqCst));
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    assert!(
        still_accepting,
        "fatal Store queue failure closed admission before the in-progress command could commit"
    );
}
