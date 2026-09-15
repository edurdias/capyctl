use super::*;

struct Active(Arc<AtomicBool>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn candidate_cleanup_isolates_other_runtime_and_missing_retained_driver_is_unsupported() {
    let (dir, owner, run, observations, host) = candidate_fixture::fixture();
    let manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/candidate-fake-qualification.json"
    ))
    .unwrap();
    let reviewed = mllm_config::effective::candidate::validate_candidate_reviewed_snapshot_text(
        &manifest.to_string(),
    )
    .unwrap();
    let body = json!({"host_id":"lab","expected_host_revision":1,"recipe_digest":reviewed.manifest_digest(),"manifest":manifest,"deadline_ms":500000,"allow_owned_abort_cleanup":true});
    let second = {
        let state = owner.lock().unwrap();
        state
            .store()
            .create_candidate_run(
                state.session(),
                "owner",
                "second",
                &body.to_string(),
                &host,
                1000,
            )
            .unwrap()
    };
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    for target in [&run, second.run_id()] {
        let init = worker
            .commands()
            .initialize_candidate("owner", target, 1, "init", 400000)
            .unwrap();
        settled(&worker, &sql, init.operation_id()).await;
    }
    let before = owner.lock().unwrap().store().resource_snapshot().unwrap();
    assert_eq!(before.owners.len(), 2);
    let other = worker
        .shared
        .retained_candidates
        .lock()
        .unwrap()
        .get(second.binding_id())
        .unwrap()
        .clone();
    let cleanup = worker
        .commands()
        .cleanup_candidate("owner", &run, 1, "cleanup", 12000)
        .unwrap();
    settled(&worker, &sql, cleanup.operation_id()).await;
    let after = owner.lock().unwrap().store().resource_snapshot().unwrap();
    assert_eq!(after.owners.len(), 1);
    assert_eq!(
        after.owners.get(second.deployment_id()),
        before.owners.get(second.deployment_id())
    );
    assert!(Arc::ptr_eq(
        &other,
        worker
            .shared
            .retained_candidates
            .lock()
            .unwrap()
            .get(second.binding_id())
            .unwrap()
    ));
    let retained = worker
        .shared
        .retained_candidates
        .lock()
        .unwrap()
        .remove(second.binding_id())
        .unwrap();
    assert!(matches!(
        worker
            .commands()
            .cleanup_candidate("owner", second.run_id(), 1, "missing", 12000),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::Unsupported
        ))
    ));
    assert_eq!(
        owner.lock().unwrap().store().resource_snapshot().unwrap(),
        after
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM candidate_cleanup_actions", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        1
    );
    worker
        .shared
        .retained_candidates
        .lock()
        .unwrap()
        .insert(second.binding_id().into(), retained);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn candidate_cleanup_original_future_exit_and_uncertainty_lane_never_replay() {
    for failure in [
        "cancel",
        "lost-probe",
        "clock",
        "regression",
        "timeout",
        "panic",
        "commit",
        "membership",
    ] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let active = Arc::new(AtomicBool::new(false));
        let clock_fault = Arc::new(AtomicBool::new(false));
        let clock: ServiceClock = {
            let fault = clock_fault.clone();
            Arc::new(move || {
                if fault.load(Ordering::SeqCst) {
                    if failure == "regression" {
                        Ok(1899)
                    } else {
                        Err(CoordinatorError::Service("cleanup clock failed".into()))
                    }
                } else {
                    Ok(1900)
                }
            })
        };
        let real = candidate::CandidateDriver::fake(clock.clone());
        let cleanup_calls = Arc::new(AtomicI64::new(0));
        let path = dir.path().join("srv.sqlite3");
        let driver = Arc::new(candidate::CandidateDriver {
            engine: real.engine.clone(),
            parked_status: real.parked_status.clone(),
            security_control: real.security_control.clone(),
            probe: {
                let real = real.clone();
                let entered = entered.clone();
                let release = release.clone();
                let active = active.clone();
                Arc::new(move |dispatch| {
                    let real = real.clone();
                    let entered = entered.clone();
                    let release = release.clone();
                    let active = active.clone();
                    Box::pin(async move {
                        let result = (real.probe)(dispatch).await;
                        active.store(true, Ordering::SeqCst);
                        let _active = Active(active);
                        entered.add_permits(1);
                        release.acquire().await.unwrap().forget();
                        if failure == "lost-probe" {
                            Err(CoordinatorError::Service("probe reply lost".into()))
                        } else {
                            result
                        }
                    })
                })
            },
            cleanup: {
                let real = real.clone();
                let active = active.clone();
                let calls = cleanup_calls.clone();
                let fault = clock_fault.clone();
                Arc::new(move |context| {
                    let real = real.clone();
                    let active = active.clone();
                    let calls = calls.clone();
                    let fault = fault.clone();
                    let path = path.clone();
                    Box::pin(async move {
                        assert!(
                            !active.load(Ordering::SeqCst),
                            "Cleanup cannot run until original probe future has exited"
                        );
                        calls.fetch_add(1, Ordering::SeqCst);
                        if matches!(failure, "clock" | "regression") {
                            fault.store(true, Ordering::SeqCst);
                        }
                        let mut evidence = (real.cleanup)(context).await?;
                        if failure == "timeout" {
                            std::future::pending::<()>().await;
                        }
                        if failure == "panic" {
                            panic!("cleanup reply panic");
                        }
                        if failure == "membership" {
                            evidence.identities.clear();
                        }
                        if failure == "commit" {
                            rusqlite::Connection::open(path).unwrap().execute_batch("CREATE TRIGGER cleanup_commit_failure BEFORE INSERT ON lifecycle_evidence BEGIN SELECT RAISE(ABORT,'cleanup evidence commit failure'); END;").unwrap();
                        }
                        Ok(evidence)
                    })
                })
            },
        });
        let factory = driver.clone();
        let worker = OwnedCoordinator::spawn_with_candidate_factory(
            owner.clone(),
            Arc::new(Observations(observations)),
            clock,
            CoordinatorOptions {
                protocol_timeout: Duration::from_secs(3),
                ..CoordinatorOptions::default()
            },
            Arc::new(|_| Err(CoordinatorError::Invalid)),
            Arc::new(move || Ok(factory.clone())),
        )
        .unwrap();
        let init = worker
            .commands()
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        if failure == "lost-probe" {
            release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(10), async {
                while worker.status() == WorkerStatus::Running {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(!worker.shared.accepting.load(Ordering::Acquire));
            assert!(worker.shared.cleanup_accepting.load(Ordering::Acquire));
            assert!(
                worker
                    .commands()
                    .initialize_candidate("owner", &run, 1, "different", 400000)
                    .is_err()
            );
            // Idle recovery has no time-bearing action. A broken clock must
            // neither prevent discovery nor retire this retained-runtime lane.
            clock_fault.store(true, Ordering::SeqCst);
            let (_stop_tx, mut stop) = watch::channel(false);
            assert!(
                !candidate::cleanup::next(&worker.shared, &mut stop)
                    .await
                    .expect("idle Cleanup discovery must not sample the clock")
            );
            assert!(worker.shared.cleanup_accepting.load(Ordering::Acquire));
            assert!(
                worker
                    .commands()
                    .cleanup_candidate("owner", &run, 1, "cleanup", 12000)
                    .is_err(),
                "a new Cleanup still requires a valid clock"
            );
            assert_eq!(cleanup_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                sql.query_row("SELECT count(*) FROM candidate_cleanup_actions", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
                0
            );
            assert!(worker.shared.cleanup_accepting.load(Ordering::Acquire));
            clock_fault.store(false, Ordering::SeqCst);
        }
        let before = owner.lock().unwrap().store().resource_snapshot().unwrap();
        let cleanup = worker
            .commands()
            .cleanup_candidate("owner", &run, 1, "cleanup", 12000)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let done: bool = sql
                    .query_row(
                        "SELECT state='succeeded' FROM operations WHERE id=?1",
                        [cleanup.operation_id()],
                        |r| r.get(0),
                    )
                    .unwrap();
                if done
                    || (cleanup_calls.load(Ordering::SeqCst) > 0
                        && !worker.shared.accepting.load(Ordering::Acquire)
                        && !matches!(failure, "cancel" | "lost-probe"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!active.load(Ordering::SeqCst));
        let after = owner.lock().unwrap().store().resource_snapshot().unwrap();
        if matches!(failure, "cancel" | "lost-probe") {
            assert!(after.owners.is_empty(), "{failure}");
            assert_eq!(after.epoch, before.epoch + 1);
            assert_eq!(
                sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        } else {
            assert_eq!(
                after, before,
                "{failure}: failure cannot release any authority"
            );
            assert_eq!(
                sql.query_row("SELECT count(*) FROM endpoint_leases", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        assert_eq!(
            worker
                .commands()
                .cleanup_candidate("owner", &run, 1, "cleanup", 12000)
                .unwrap(),
            cleanup
        );
        assert!(
            worker
                .commands()
                .cleanup_candidate("owner", &run, 1, "cleanup", 12001)
                .is_err()
        );
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(init.deployment_id(), cleanup.deployment_id());
        release.add_permits(1);
        worker.shutdown().await.unwrap();
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)]
async fn candidate_cleanup_shutdown_queued_behind_store_prevents_new_arm() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let init = worker
        .commands()
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    settled(&worker, &sql, init.operation_id()).await;
    let permits = worker
        .shared
        .store_jobs
        .clone()
        .acquire_many_owned(worker.shared.options.max_observers as u32 + 1)
        .await
        .unwrap();
    let cleanup = worker
        .commands()
        .cleanup_candidate("owner", &run, 1, "cleanup", 12000)
        .unwrap();
    let shared = worker.shared.clone();
    let guard = owner.lock().unwrap();
    drop(permits);
    let task = tokio::spawn(async move { worker.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !shared.shutdown_requested.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(guard);
    task.await.unwrap().unwrap();
    assert_eq!(
        sql.query_row(
            "SELECT state FROM lifecycle_steps WHERE id=?1",
            [cleanup.step_id()],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "planned"
    );
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM lifecycle_evidence WHERE step_id=?1",
            [cleanup.step_id()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(
        !owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .owners
            .is_empty()
    );
}
