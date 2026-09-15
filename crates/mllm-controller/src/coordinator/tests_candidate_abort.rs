use super::*;

const BODY: &str = r#"{"expected_revision":1,"action":"abort","deadline_ms":400000}"#;

#[tokio::test]
async fn candidate_abort_pregrant_request_conflict_preserves_observation_and_shutdown_errors() {
    struct RequestObservation {
        values: Vec<MemoryObservation>,
        calls: AtomicI64,
        entered: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }
    impl ServiceObservation for RequestObservation {
        fn observe(&self, _: String) -> ObservationFuture {
            let values = self.values.clone();
            let entered = self.entered.clone();
            let release = self.release.clone();
            let request = self.calls.fetch_add(1, Ordering::SeqCst) > 0;
            Box::pin(async move {
                if request {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    return Err(CoordinatorError::Service("observation unavailable".into()));
                }
                Ok(values)
            })
        }
    }
    for mode in ["abort", "observation", "shutdown"] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let worker = OwnedCoordinator::spawn_fake(
            owner.clone(),
            Arc::new(RequestObservation {
                values: observations,
                calls: AtomicI64::new(0),
                entered: entered.clone(),
                release: release.clone(),
            }),
            Arc::new(|| Ok(1900)),
            CoordinatorOptions::default(),
        )
        .unwrap();
        let init = worker
            .commands()
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        settled(&worker, &sql, init.operation_id()).await;
        let body=json!({"model":format!("candidate-{}",init.deployment_id()),"messages":[{"role":"user","content":"Repeat exactly: MLLM_ALPHA_71"}],"temperature":0,"max_tokens":16,"stream":false}).to_string();
        let commands = worker.commands();
        let request_run = run.clone();
        let pending = tokio::task::spawn_blocking(move || {
            commands.candidate_inference("owner", &request_run, 1, "queued", &body)
        });
        tokio::time::timeout(Duration::from_secs(5), entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        if mode == "abort" {
            abort_and_wait(&worker, &sql, &run).await;
        } else if mode == "observation" {
            release.add_permits(1);
        }
        if mode == "shutdown" {
            worker.shutdown().await.unwrap();
        } else {
            let result = tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .unwrap()
                .unwrap();
            if mode == "abort" {
                assert!(matches!(
                    result,
                    Err(CoordinatorCommandError::Lifecycle(LifecycleError::Conflict))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(CoordinatorCommandError::Coordinator(
                        CoordinatorError::Stopped(_)
                    ))
                ));
            }
            worker.shutdown().await.unwrap();
            assert_eq!(
                sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            continue;
        }
        assert!(matches!(
            pending.await.unwrap(),
            Err(CoordinatorCommandError::Coordinator(
                CoordinatorError::Stopped(_)
            ))
        ));
        assert_eq!(
            sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn candidate_abort_drops_required_baseline_and_post_wake_probe_futures_with_leases() {
    for target in [1, 2, 9] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let real = candidate::CandidateDriver::fake(Arc::new(|| Ok(1900)));
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let active = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicI64::new(0));
        let late = Arc::new(Mutex::new(
            None::<mllm_domain::qualification::CandidateRequestObservation>,
        ));
        let driver = Arc::new(candidate::CandidateDriver {
            engine: real.engine.clone(),
            parked_status: real.parked_status.clone(),
            security_control: real.security_control.clone(),
            probe: Arc::new({
                let entered = entered.clone();
                let release = release.clone();
                let active = active.clone();
                let calls = calls.clone();
                let late = late.clone();
                move |dispatch| {
                    let real = real.clone();
                    let entered = entered.clone();
                    let release = release.clone();
                    let active = active.clone();
                    let calls = calls.clone();
                    let late = late.clone();
                    Box::pin(async move {
                        let index = calls.fetch_add(1, Ordering::SeqCst) + 1;
                        let result = (real.probe)(dispatch).await;
                        if index == target {
                            *late.lock().unwrap() = Some(result.as_ref().unwrap().clone());
                            active.store(true, Ordering::SeqCst);
                            let _active = Active(&active);
                            entered.add_permits(1);
                            release.acquire().await.unwrap().forget();
                        }
                        result
                    })
                }
            }),
        });
        let factory = driver.clone();
        let worker = OwnedCoordinator::spawn_with_candidate_factory(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            CoordinatorOptions::default(),
            Arc::new(|_| Err(CoordinatorError::Invalid)),
            Arc::new(move || Ok(factory.clone())),
        )
        .unwrap();
        let init = worker
            .commands()
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        if target > 1 {
            settled(&worker, &sql, init.operation_id()).await;
            if target == 9 {
                security_tests::baseline_requests(&worker, &sql, &run, init.deployment_id(), 4)
                    .await;
                tokio::time::timeout(Duration::from_secs(30),async {while !sql.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_runs WHERE json_extract(plan_json,'$.action')='security' AND state='succeeded')",[],|r|r.get::<_,bool>(0)).unwrap(){assert_eq!(worker.status(),WorkerStatus::Running);tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
                for (key, action) in [
                    ("park", CandidateLifecycleAction::Park),
                    ("restore", CandidateLifecycleAction::Restore),
                ] {
                    let accepted = worker
                        .commands()
                        .candidate_action("owner", &run, 1, key, 400000, action)
                        .unwrap();
                    settled(&worker, &sql, accepted.operation_id()).await;
                }
            }
            let body=json!({"model":format!("candidate-{}",init.deployment_id()),"messages":[{"role":"user","content":"Repeat exactly: MLLM_ALPHA_71"}],"temperature":0,"max_tokens":16,"stream":false}).to_string();
            let commands = worker.commands();
            let run = run.clone();
            tokio::task::spawn_blocking(move || {
                commands.candidate_inference("owner", &run, 1, "marker", &body)
            })
            .await
            .unwrap()
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(30), entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(
            sql.query_row("SELECT count(*) FROM owned_launch_associations", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
        assert_eq!(
            sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        abort_and_wait(&worker, &sql, &run).await;
        let before_late = retained(&sql);
        {
            let state = owner.lock().unwrap();
            let collector = state
                .store()
                .candidate_collector(state.session(), "owner", &run)
                .unwrap();
            assert!(
                state
                    .store()
                    .record_candidate_result(
                        state.session(),
                        &collector,
                        late.lock().unwrap().as_ref().unwrap(),
                        1900
                    )
                    .is_err(),
                "late success cannot publish Ready or settle an aborted lease"
            );
        }
        assert_eq!(retained(&sql), before_late);
        assert!(!active.load(Ordering::SeqCst));
        assert_eq!(calls.load(Ordering::SeqCst), target);
        assert!(Arc::ptr_eq(
            &driver,
            worker
                .shared
                .retained_candidates
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
        ));
        release.add_permits(1);
        worker.shutdown().await.unwrap();
    }
}

fn retained(sql: &rusqlite::Connection) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    [
        "resource_owners",
        "resource_grants",
        "owners",
        "runtime_bindings",
        "owned_launch_associations",
        "endpoint_leases",
        "request_leases",
        "lifecycle_steps",
        "lifecycle_runs",
        "lifecycle_claims",
        "qualification_evidence_refs",
        "qualification_request_attempts",
        "qualification_request_results",
        "qualification_parked_status",
        "qualifications",
    ]
    .into_iter()
    .map(|table| {
        let mut statement = sql
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
    .collect()
}

pub(super) async fn abort_and_wait(
    worker: &OwnedCoordinator,
    sql: &rusqlite::Connection,
    run: &str,
) {
    let before = retained(sql);
    let resources = worker
        .shared
        .owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap();
    let cmd = worker.commands();
    let run = run.to_owned();
    let key_run = run.clone();
    let receipt = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            cmd.abort_candidate("owner", &key_run, 1, "abort", 400000)
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while worker.shared.active_candidate.lock().unwrap().is_some() {
            assert_eq!(worker.status(), WorkerStatus::Running);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        worker.status(),
        WorkerStatus::Running,
        "Abort must leave unrelated admission alive"
    );
    assert_eq!(
        retained(sql),
        before,
        "Abort/cancel must not alter retained authority, leases, evidence or child states"
    );
    assert_eq!(
        worker
            .shared
            .owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap(),
        resources,
        "Abort is not a resource completion epoch"
    );
    assert_eq!(
        worker
            .commands()
            .abort_candidate("owner", &run, 1, "abort", 400000)
            .unwrap(),
        receipt
    );
    assert!(worker
        .commands()
        .finish_candidate("owner", &run, 1, "finish", 400000)
        .is_err());
    assert_eq!(
        sql.query_row(
            "SELECT state FROM qualification_runs WHERE id=?1",
            [&run],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "aborted"
    );
}

#[tokio::test]
async fn candidate_abort_exits_initialize_after_applied_effect_and_preserves_original_driver() {
    {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let gate = Gate::new(false);
        let probes = Arc::new(AtomicI64::new(0));
        let worker = worker_candidate(
            owner.clone(),
            observations,
            gate.clone(),
            probes.clone(),
            Duration::from_secs(30),
        );
        let receipt = worker
            .commands()
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        gate.entered().await;
        assert!(
            worker
                .commands()
                .abort_candidate("owner", &run, 1, "init", 400000)
                .is_err(),
            "Initialize's key cannot become Abort"
        );
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        let original = worker
            .shared
            .retained_candidates
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        abort_and_wait(&worker, &sql, &run).await;
        assert!(
            !gate.active.load(Ordering::SeqCst),
            "the actual child future must exit"
        );
        gate.release.add_permits(1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert!(Arc::ptr_eq(
            &original,
            worker
                .shared
                .retained_candidates
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
        ));
        assert_eq!(
            worker
                .commands()
                .initialize_candidate("owner", &run, 1, "init", 400000)
                .unwrap()
                .operation_id(),
            receipt.operation_id()
        );
        worker.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn candidate_abort_prevents_new_arm_waiting_for_store_capacity() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let observed = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    struct GatedObservation {
        values: Vec<MemoryObservation>,
        entered: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }
    impl ServiceObservation for GatedObservation {
        fn observe(&self, _: String) -> ObservationFuture {
            let values = self.values.clone();
            let entered = self.entered.clone();
            let release = self.release.clone();
            Box::pin(async move {
                entered.add_permits(1);
                release.acquire().await.unwrap().forget();
                Ok(values)
            })
        }
    }
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(GatedObservation {
            values: observations,
            entered: observed.clone(),
            release: release.clone(),
        }),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    worker
        .commands()
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), observed.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let permits = worker
        .shared
        .store_jobs
        .clone()
        .acquire_many_owned((worker.shared.options.max_observers + 1) as u32)
        .await
        .unwrap();
    release.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    abort_and_wait(&worker, &sql, &run).await;
    drop(permits);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        sql.query_row("SELECT count(*) FROM resource_grants", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM lifecycle_steps WHERE state='armed'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(worker.shared.retained_candidates.lock().unwrap().is_empty());
    worker.shutdown().await.unwrap();
}

#[test]
fn candidate_abort_store_rolls_back_receipt_state_event_and_clock_failures() {
    for boundary in [
        "receipt",
        "state",
        "event",
        "clock-regression",
        "clock-expiry",
    ] {
        let (dir, owner, run, _, _) = candidate_fixture::fixture();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        match boundary {
            "receipt" => sql.execute_batch("CREATE TRIGGER fail_abort BEFORE INSERT ON command_receipts WHEN NEW.command_scope LIKE '%/actions' BEGIN SELECT RAISE(ABORT,'receipt'); END;").unwrap(),
            "state" => sql.execute_batch("CREATE TRIGGER fail_abort BEFORE UPDATE ON qualification_runs BEGIN SELECT RAISE(ABORT,'state'); END;").unwrap(),
            "event" => sql.execute_batch("CREATE TRIGGER fail_abort BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'event'); END;").unwrap(),
            _=>{},
        }
        let before = retained(&sql);
        let state = owner.lock().unwrap();
        let mut calls = 0;
        let result = state.store().abort_candidate_run_with_clock(
            state.session(),
            "owner",
            &run,
            "abort",
            BODY,
            || {
                calls += 1;
                Ok(if calls == 2 {
                    match boundary {
                        "clock-regression" => 1199,
                        "clock-expiry" => 400000,
                        _ => 1200,
                    }
                } else {
                    1200
                })
            },
        );
        assert!(result.is_err(), "{boundary}");
        assert_eq!(retained(&sql), before);
        assert_eq!(
            sql.query_row("SELECT state FROM qualification_runs", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "accepted"
        );
        assert_eq!(
            sql.query_row(
                "SELECT count(*) FROM operations WHERE kind='candidate_abort_v1'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert!(state
            .store()
            .candidate_abort_command_receipt(state.session(), "owner", &run, "abort", BODY)
            .unwrap()
            .is_none());
    }
}

#[test]
fn candidate_abort_store_independent_connection_races_share_only_exact_receipt() {
    for same_key in [true, false] {
        let (dir, owner, run, _, _) = candidate_fixture::fixture();
        let session = owner.lock().unwrap().session().clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let jobs: Vec<_> = (0..2)
            .map(|index| {
                let path = dir.path().join("srv.sqlite3");
                let session = session.clone();
                let barrier = barrier.clone();
                let run = run.clone();
                std::thread::spawn(move || {
                    let store = mllm_store::Store::open(&path).unwrap();
                    barrier.wait();
                    store.abort_candidate_run_with_clock(
                        &session,
                        "owner",
                        &run,
                        if same_key || index == 0 { "a" } else { "b" },
                        BODY,
                        || Ok(1200),
                    )
                })
            })
            .collect();
        let results: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            if same_key { 2 } else { 1 }
        );
        if same_key {
            assert_eq!(results[0].as_ref().unwrap(), results[1].as_ref().unwrap());
        }
    }
}

#[test]
fn candidate_abort_store_rejects_scope_clocks_terminal_states_and_changed_keys() {
    for (principal, revision, deadline, now, state, generation) in [
        ("other", 1, 400000, 1200, "accepted", 1),
        ("owner", 2, 400000, 1200, "accepted", 1),
        ("owner", 1, 500001, 1200, "accepted", 1),
        ("owner", 1, 1200, 1200, "accepted", 1),
        ("owner", 1, 400000, 999, "accepted", 1),
        ("owner", 1, 400000, 500000, "accepted", 1),
        ("owner", 1, 400000, 1200, "accepted", 2),
        ("owner", 1, 400000, 1200, "failed", 1),
        ("owner", 1, 400000, 1200, "expired", 1),
        ("owner", 1, 400000, 1200, "passed", 1),
        ("owner", 1, 400000, 1200, "aborted", 1),
    ] {
        let (dir, owner, run, _, _) = candidate_fixture::fixture();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        sql.execute("UPDATE qualification_runs SET state=?1", [state])
            .unwrap();
        sql.execute("UPDATE deployments SET current_generation=?1", [generation])
            .unwrap();
        let s = owner.lock().unwrap();
        let body = json!({"expected_revision":revision,"action":"abort","deadline_ms":deadline})
            .to_string();
        assert!(s
            .store()
            .abort_candidate_run_with_clock(s.session(), principal, &run, "abort", &body, || Ok(
                now
            ))
            .is_err());
        assert_eq!(
            sql.query_row("SELECT state FROM qualification_runs", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            state
        );
        assert_eq!(
            sql.query_row(
                "SELECT count(*) FROM operations WHERE kind='candidate_abort_v1'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
    let (_dir, owner, run, _, _) = candidate_fixture::fixture();
    let s = owner.lock().unwrap();
    let r = s
        .store()
        .abort_candidate_run_with_clock(s.session(), "owner", &run, "abort", BODY, || Ok(1200))
        .unwrap();
    for body in [
        BODY.replace("400000", "399999"),
        BODY.replace("expected_revision\":1", "expected_revision\":2"),
    ] {
        assert!(matches!(
            s.store()
                .candidate_abort_command_receipt(s.session(), "owner", &run, "abort", &body),
            Err(LifecycleError::IdempotencyConflict)
        ));
    }
    let stale = s.session().clone();
    let next = s.store().begin_coordinator_session().unwrap();
    assert!(s
        .store()
        .candidate_abort_command_receipt(&stale, "owner", &run, "abort", BODY)
        .is_err());
    assert_eq!(
        s.store()
            .abort_candidate_run_with_clock(&next, "owner", &run, "abort", BODY, || panic!(
                "history never renews a clock"
            ))
            .unwrap(),
        r
    );
    assert!(s
        .store()
        .abort_candidate_run_with_clock(&next, "owner", &run, "new-key", BODY, || Ok(1200))
        .is_err());
}

#[test]
fn candidate_abort_history_rejects_corrupt_oversized_sources_and_exact_column_mismatches() {
    for mutation in [
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.version',2) WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.deadline_ms',399999) WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.source_state','passed') WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.source.generation',2) WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.source.deadline_ms',500001) WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.session_epoch',99) WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.unexpected',true) WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=CAST(zeroblob(1048577) AS TEXT) WHERE idempotency_key='abort'",
        "UPDATE command_receipts SET response_json=zeroblob(10) WHERE idempotency_key='abort'",
        "UPDATE operations SET state='running' WHERE kind='candidate_abort_v1'",
        "UPDATE operations SET kind='candidate_finish_v4' WHERE kind='candidate_abort_v1'",
        "INSERT INTO command_receipts SELECT principal_id,command_scope,'duplicate',request_hash,operation_id,response_json FROM command_receipts WHERE idempotency_key='abort'",
        "UPDATE qualification_runs SET state='passed'",
        "UPDATE qualification_runs SET authorization_json=json_remove(authorization_json,'$.deadline_ms')",
        "UPDATE qualification_runs SET authorization_json=CAST(zeroblob(1048577) AS TEXT)",
        "UPDATE effective_revisions SET effective_json=CAST(zeroblob(1048577) AS TEXT)",
        "UPDATE runtime_bindings SET binding_json=CAST(zeroblob(1048577) AS TEXT)",
        "UPDATE command_receipts SET response_json=CAST(zeroblob(1048577) AS TEXT) WHERE idempotency_key='create'",
    ] {
        let (dir,owner,run,_,_)=candidate_fixture::fixture();
        let s=owner.lock().unwrap();s.store().abort_candidate_run_with_clock(s.session(),"owner",&run,"abort",BODY,||Ok(1200)).unwrap();
        let sql=rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();sql.execute_batch(mutation).unwrap();
        assert!(s.store().candidate_abort_command_receipt(s.session(),"owner",&run,"abort",BODY).is_err(),"{mutation}");
    }
}

#[tokio::test]
async fn candidate_abort_is_run_scoped_and_unrelated_candidate_progresses_with_retained_charge() {
    let (dir, owner, first, observations, host) = candidate_fixture::fixture();
    let create = |key: &str| {
        let manifest: serde_json::Value = serde_json::from_str(include_str!(
            "../../../mllm-config/tests/fixtures/candidate-fake-qualification.json"
        ))
        .unwrap();
        let reviewed =
            mllm_config::effective::candidate::validate_candidate_reviewed_snapshot_text(
                &manifest.to_string(),
            )
            .unwrap();
        let body = json!({"host_id":"lab","expected_host_revision":1,"recipe_digest":reviewed.manifest_digest(),"manifest":manifest,"deadline_ms":500000,"allow_owned_abort_cleanup":true});
        let state = owner.lock().unwrap();
        state
            .store()
            .create_candidate_run(
                state.session(),
                "owner",
                key,
                &body.to_string(),
                &host,
                1000,
            )
            .unwrap()
            .run_id()
            .to_owned()
    };
    let second = create("second");
    let gate = Gate::new(false);
    let builds = Arc::new(AtomicI64::new(0));
    let factory_gate = gate.clone();
    let factory_builds = builds.clone();
    let worker = OwnedCoordinator::spawn_with_candidate_factory(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(|_| Err(CoordinatorError::Invalid)),
        Arc::new(move || {
            if factory_builds.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Arc::new(candidate::CandidateDriver {
                    engine: factory_gate.clone(),
                    parked_status: Arc::new(|_| Box::pin(async { Err(CoordinatorError::Invalid) })),
                    probe: Arc::new(|_| Box::pin(async { Err(CoordinatorError::Invalid) })),
                    security_control: Arc::new(|_| {
                        Box::pin(async { Err(CoordinatorError::Invalid) })
                    }),
                }))
            } else {
                Ok(candidate::CandidateDriver::fake(Arc::new(|| Ok(1900))))
            }
        }),
    )
    .unwrap();
    worker
        .commands()
        .initialize_candidate("owner", &first, 1, "first-init", 400000)
        .unwrap();
    gate.entered().await;
    worker
        .commands()
        .initialize_candidate("owner", &second, 1, "second-init", 400000)
        .unwrap();
    worker
        .commands()
        .abort_candidate("owner", &second, 1, "second-abort", 400000)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        gate.active.load(Ordering::SeqCst),
        "another run's Abort must not cancel this child"
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    abort_and_wait(&worker, &sql, &first).await;
    let before = owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners;
    let third = create("third");
    let init = worker
        .commands()
        .initialize_candidate("owner", &third, 1, "third-init", 400000)
        .unwrap();
    settled(&worker, &sql, init.operation_id()).await;
    let after = owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners;
    for (id, charge) in before {
        assert_eq!(after.get(&id), Some(&charge));
    }
    assert_eq!(after.len(), 2);
    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "aborted queued run must never construct a driver"
    );
    assert_eq!(
        sql.query_row(
            "SELECT state FROM qualification_runs WHERE id=?1",
            [third],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "running"
    );
    worker.shutdown().await.unwrap();
}
