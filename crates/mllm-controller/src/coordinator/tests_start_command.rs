use super::*;
use mllm_store::ordinary_lifecycle::StartReceipt;
use std::sync::atomic::AtomicUsize;

#[tokio::test]
async fn unarmed_stop_waits_for_old_observation_and_worker_completes_later_work() {
    let (_dir, owner, fence, observations) = setup().await;
    let source = fixture::owned_source().await;
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let gate = Gate::new(false);
    let adapter = gate.clone();
    let original = fence.deployment_id.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(HeldObservation {
            entered: entered.clone(),
            release: release.clone(),
            observations,
        }),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(move |work| {
            // Driver construction may race the Stop fence, but only a fresh arm
            // can produce any adapter command. Keep both calls observable.
            assert!(
                work.fence().deployment_id == original
                    || work.fence().deployment_id == source.other.deployment_id
            );
            Ok(test_driver(adapter.clone()))
        }),
    )
    .unwrap();
    let commands = w.commands();
    let start = commands
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let stop = commands
        .stop("owner", &fence.deployment_id, 1, "stop", 10000)
        .unwrap();
    let later = commands
        .start("owner", &source.other.deployment_id, 1, "later", 10000)
        .unwrap();
    {
        let o = owner.lock().unwrap();
        assert_eq!(
            o.store()
                .runtime_binding(&fence.deployment_id)
                .unwrap()
                .unwrap()
                .state,
            "reserved"
        );
        assert_eq!(
            o.store()
                .initialize_status(o.session(), start.step_id(), 1900)
                .unwrap(),
            InitializeStatus::Superseded
        );
    }
    release.add_permits(2);
    gate.entered().await;
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    gate.release.add_permits(1);
    completed(&owner, &later).await;
    {
        let o = owner.lock().unwrap();
        assert!(o
            .store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_none());
        assert_eq!(
            o.store()
                .snapshot()
                .unwrap()
                .operations
                .iter()
                .find(|r| r.id == stop.operation_id)
                .unwrap()
                .state,
            "succeeded"
        );
        assert_eq!(
            o.store()
                .initialize_status(o.session(), start.step_id(), 1900)
                .unwrap(),
            InitializeStatus::Superseded
        );
    }
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    w.shutdown().await.unwrap();
    assert_eq!(
        commands
            .stop("owner", &fence.deployment_id, 1, "stop", 10000)
            .unwrap(),
        stop
    );
}

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

async fn completed(owner: &SharedCoordinatorState, receipt: &StartReceipt) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = {
                let o = owner.lock().unwrap();
                o.store()
                    .initialize_status(o.session(), receipt.step_id(), 1900)
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
async fn scoped_stop_resolves_generation_and_replays_after_worker_shutdown() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    let factories = Arc::new(AtomicUsize::new(0));
    let w = command_worker(
        owner.clone(),
        observations,
        gate.clone(),
        factories.clone(),
        Arc::new(|| Ok(1900)),
    );
    let commands = w.commands();
    let start = commands
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap();
    gate.entered().await;
    gate.release.add_permits(1);
    completed(&owner, &start).await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    for (table, condition) in [
        ("command_receipts", "NEW.idempotency_key='stop'"),
        ("management_events", "NEW.kind='ordinary_cleanup_accepted'"),
    ] {
        sql.execute_batch(&format!("CREATE TRIGGER fail_stop BEFORE INSERT ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT,'stop rollback'); END;")).unwrap();
        let before = counts(&dir);
        let o = owner.lock().unwrap();
        assert!(matches!(
            o.store().accept_ordinary_stop_command(
                o.session(),
                "owner",
                &fence.deployment_id,
                1,
                "stop",
                1900,
                10000
            ),
            Err(LifecycleError::Sql(_))
        ));
        drop(o);
        assert_eq!(counts(&dir), before);
        sql.execute_batch("DROP TRIGGER fail_stop").unwrap();
    }
    let stop = commands
        .stop("owner", &fence.deployment_id, 1, "stop", 10000)
        .unwrap();
    assert_eq!(stop.generation, 2);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = {
                let o = owner.lock().unwrap();
                o.store()
                    .ordinary_cleanup_status(o.session(), &stop.step_id, 1900)
                    .unwrap()
            };
            if status == OrdinaryCleanupStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    config["name"] = serde_json::json!("ordinary");
    config["routes"] = serde_json::json!(["ordinary-replaced"]);
    let mut host = golden["input"]["host"].clone();
    host["runtime_profiles"]["local"]["build_fingerprint"] =
        serde_json::json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        serde_json::json!("secret://another-admin");
    {
        let o = owner.lock().unwrap();
        o.store()
            .replace_stopped_managed_configuration(
                o.session(),
                "owner",
                "replace",
                &fence.deployment_id,
                &serde_json::json!({"expected_revision":1,"config":config}).to_string(),
                &host,
                1900,
            )
            .unwrap();
    }
    w.shutdown().await.unwrap();
    let before = counts(&dir);
    assert_eq!(
        commands
            .stop("owner", &fence.deployment_id, 1, "stop", 10000)
            .unwrap(),
        stop
    );
    assert!(matches!(
        commands.stop("owner", &fence.deployment_id, 2, "stop", 10000),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::IdempotencyConflict
        ))
    ));
    assert!(matches!(
        commands.stop("owner", &fence.deployment_id, 1, "stop", 10001),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::IdempotencyConflict
        ))
    ));
    assert!(commands
        .stop("owner", &fence.deployment_id, 1, "new", 10000)
        .is_err());
    assert_eq!(counts(&dir), before);
    assert_eq!(factories.load(Ordering::SeqCst), 1);
    sql.execute("UPDATE command_receipts SET response_json=json_set(response_json,'$.step_id',?1) WHERE idempotency_key='stop'",[ulid::Ulid::new().to_string()]).unwrap();
    assert!(matches!(
        commands.stop("owner", &fence.deployment_id, 1, "stop", 10000),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::CorruptStoredData
        ))
    ));
    for corruption in [
        "UPDATE command_receipts SET response_json=printf('%1048577s','x') WHERE idempotency_key='stop'",
        "UPDATE command_receipts SET response_json=CAST('{}' AS BLOB) WHERE idempotency_key='stop'",
        "UPDATE command_receipts SET request_hash=printf('%65s','x') WHERE idempotency_key='stop'",
    ] {
        sql.execute_batch(corruption).unwrap();
        assert!(matches!(commands.stop("owner",&fence.deployment_id,1,"stop",10000),Err(CoordinatorCommandError::Lifecycle(LifecycleError::CorruptStoredData))));
    }
}

#[tokio::test]
async fn scoped_stop_is_available_while_initialize_is_paused_but_not_after_fatal_closure() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.lost_reply.store(true, Ordering::SeqCst);
    let w = command_worker(
        owner.clone(),
        observations,
        gate.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|| Ok(1900)),
    );
    let handle = w.commands();
    handle
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap();
    gate.entered().await;
    gate.release.add_permits(1);
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    let receipt = handle
        .stop("owner", &fence.deployment_id, 1, "stop", 10000)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = {
                let o = owner.lock().unwrap();
                o.store()
                    .ordinary_cleanup_status(o.session(), &receipt.step_id, 1900)
                    .unwrap()
            };
            if status == OrdinaryCleanupStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    w.shared.fail("fatal private-path credential-string");
    assert_eq!(
        handle
            .stop("owner", &fence.deployment_id, 1, "stop", 10000)
            .unwrap(),
        receipt
    );
    assert!(matches!(
        handle.stop("owner", &fence.deployment_id, 1, "new", 10000),
        Err(CoordinatorCommandError::Coordinator(
            CoordinatorError::Stopped(_)
        ))
    ));
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    w.shutdown().await.unwrap();
}

#[tokio::test]
async fn command_policy_and_queue_rejections_are_typed_at_acceptance() {
    let (dir, owner, fence, observations) = setup().await;
    let source = fixture::owned_source().await;
    let w = command_worker(
        owner.clone(),
        observations.clone(),
        Gate::new(false),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|| Ok(1900)),
    );
    let handle = w.commands();
    let policy = owner
        .lock()
        .unwrap()
        .store()
        .resource_policy("lab")
        .unwrap()
        .unwrap();
    let mut denied = policy.controls.clone();
    denied.device_sharing = mllm_config::effective::Sharing::Exclusive;
    for sharing in denied.device_sharing_overrides.values_mut() {
        *sharing = mllm_config::effective::Sharing::Exclusive;
    }
    {
        let o = owner.lock().unwrap();
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                policy.revision,
                "deny",
                &denied,
                &observations,
                1900,
            )
            .unwrap();
    }
    let before = counts(&dir);
    assert!(matches!(
        handle.start("owner", &fence.deployment_id, 1, "denied", 10000),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::HostPolicyDenied
        ))
    ));
    assert_eq!(counts(&dir), before);
    let mut allowed = policy.controls;
    allowed.queue.max_pending_total = 1;
    allowed.queue.max_pending_per_deployment = 1;
    {
        let o = owner.lock().unwrap();
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                policy.revision + 1,
                "allow",
                &allowed,
                &observations,
                1900,
            )
            .unwrap();
    }
    handle
        .start("owner", &fence.deployment_id, 1, "start", 10000)
        .unwrap();
    let before = counts(&dir);
    assert!(matches!(
        handle.start("owner", &source.other.deployment_id, 1, "full", 10000),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::QueueFull
        ))
    ));
    assert_eq!(counts(&dir), before);
    w.shutdown().await.unwrap();
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
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::IdempotencyConflict
        ))
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
