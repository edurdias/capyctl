use super::*;
use serde_json::json;

#[path = "tests_candidate_inference.rs"]
mod inference_tests;

#[path = "../../tests/qualification_support/candidate_fixture.rs"]
mod candidate_fixture;

fn worker_candidate(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    gate: Arc<Gate>,
    probes: Arc<AtomicI64>,
    timeout: Duration,
) -> OwnedCoordinator {
    worker_candidate_clock(
        owner,
        observations,
        gate,
        probes,
        timeout,
        Arc::new(|| Ok(1900)),
    )
}
fn worker_candidate_clock(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    gate: Arc<Gate>,
    probes: Arc<AtomicI64>,
    timeout: Duration,
    clock: ServiceClock,
) -> OwnedCoordinator {
    let ordinary = gate.clone();
    OwnedCoordinator::spawn_with_candidate_factory(
        owner,
        Arc::new(Observations(observations)),
        clock,
        CoordinatorOptions {
            protocol_timeout: timeout,
            ..CoordinatorOptions::default()
        },
        Arc::new(move |_| Ok(test_driver(ordinary.clone()))),
        Arc::new(move || {
            let probe_engine = gate.clone();
            let probes = probes.clone();
            Ok(Arc::new(candidate::CandidateDriver {
                engine: gate.clone(),
                probe: Arc::new(move |dispatch| {
                    let gate = probe_engine.clone();
                    let probes = probes.clone();
                    Box::pin(async move {
                        probes.fetch_add(1, Ordering::SeqCst);
                        crate::qualification::collect_probe_with_clock(
                            &gate.engine,
                            dispatch,
                            &|| Ok(1900),
                        )
                        .await
                        .map_err(|e| CoordinatorError::Service(e.to_string()))
                    })
                }),
            }))
        }),
    )
    .unwrap()
}

async fn settled(worker: &OwnedCoordinator, sql: &rusqlite::Connection, operation: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let done: bool = sql
                .query_row(
                    "SELECT state='succeeded' FROM operations WHERE id=?1",
                    [operation],
                    |r| r.get(0),
                )
                .unwrap();
            if done {
                break;
            }
            assert_eq!(worker.status(), WorkerStatus::Running);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn candidate_owned_effect_survives_dropped_commands_and_unrelated_history_reads() {
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
    let commands = worker.commands();
    let accepted = commands
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    let retry = commands
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    assert_eq!(accepted.operation_id(), retry.operation_id());
    drop(commands);
    gate.entered().await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let grant_before: String = sql
        .query_row("SELECT request_json FROM resource_grants", [], |r| r.get(0))
        .unwrap();
    let owners_before = owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners;
    // Held actual effect must not hold the Store lock. A separate ordinary
    // deployment and history observation remain possible during this await.
    {
        let state = owner.try_lock().expect("Store mutex held across effect");
        let id = mllm_domain::DeploymentId::new();
        state
            .store()
            .accept_deployment(mllm_store::AcceptDeployment {
                id,
                name: "unrelated-stopped".into(),
                kind: "model".into(),
                route_model_id: None,
                desired_state: mllm_domain::LifecycleState::Stopped,
                schema_version: 1,
                idempotency_key: "unrelated".into(),
                initial_operation_id: mllm_domain::OperationId("unrelated-operation".into()),
            })
            .unwrap();
        assert!(state
            .store()
            .snapshot()
            .unwrap()
            .operations
            .iter()
            .any(|operation| operation.id == "unrelated-operation"));
    }
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert!(worker
        .shared
        .retained_candidates
        .lock()
        .unwrap()
        .contains_key(&{
            let state = owner.lock().unwrap();
            state
                .store()
                .candidate_run_snapshot("owner", &run)
                .unwrap()
                .unwrap()
                .receipt()
                .binding_id()
                .to_owned()
        }));
    gate.release.add_permits(1);
    settled(&worker, &sql, accepted.operation_id()).await;
    assert_eq!(
        sql.query_row("SELECT request_json FROM resource_grants", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        grant_before
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .owners,
        owners_before
    );
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    let commands = worker.commands();
    worker.shutdown().await.unwrap();
    assert_eq!(
        commands
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap()
            .operation_id(),
        accepted.operation_id()
    );
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
}

#[tokio::test]
async fn candidate_uncertain_effect_commit_failure_panic_and_shutdown_never_advance_or_replay() {
    for failure in ["timeout", "panic", "lost-reply", "commit", "shutdown"] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let gate = Gate::new(failure == "panic");
        let probes = Arc::new(AtomicI64::new(0));
        let worker = worker_candidate(
            owner.clone(),
            observations,
            gate.clone(),
            probes.clone(),
            if failure == "timeout" {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(30)
            },
        );
        let commands = worker.commands();
        let accepted = commands
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        gate.entered().await;
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        if failure == "commit" {
            sql.execute_batch("CREATE TRIGGER fail_candidate_evidence BEFORE INSERT ON lifecycle_evidence BEGIN SELECT RAISE(ABORT,'injected candidate commit failure'); END;").unwrap();
        }
        if failure == "lost-reply" {
            gate.lost_reply.store(true, Ordering::SeqCst);
        }
        if matches!(failure, "commit" | "lost-reply") {
            gate.release.add_permits(1);
        }
        if failure != "shutdown" {
            tokio::time::timeout(Duration::from_secs(30), async {
                while matches!(worker.status(), WorkerStatus::Running) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        let status = worker.shutdown().await.unwrap();
        assert!(
            matches!(
                status,
                WorkerStatus::Uncertain { .. } | WorkerStatus::Failed(_)
            ),
            "{failure}: {status:?}"
        );
        assert!(!gate.active.load(Ordering::SeqCst));
        assert_eq!(
            *gate.calls.lock().unwrap(),
            vec![RuntimeAction::Initialize],
            "{failure}"
        );
        assert_eq!(probes.load(Ordering::SeqCst), 0, "{failure}");
        assert_eq!(
            sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            sql.query_row(
                "SELECT COUNT(*) FROM lifecycle_steps WHERE state='planned'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            commands
                .initialize_candidate("owner", &run, 1, "init", 400000)
                .unwrap()
                .operation_id(),
            accepted.operation_id()
        );
        assert!(commands
            .initialize_candidate("owner", &run, 1, "new", 400000)
            .is_err());
        assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    }
}

#[test]
fn candidate_discovery_and_send_reject_stale_policy_context_and_session() {
    let f = fixture::fixture();
    let work = f
        .store
        .next_candidate_initialize(&f.session, 1200)
        .unwrap()
        .unwrap();
    assert_eq!(work.parent_step_id, f.init.step_id());
    assert_eq!(work.initialize_step_id, f.init.effect_ids()[0]);
    assert!(matches!(
        f.store
            .arm_candidate_effect(&f.session, &work.initialize_step_id, f.admission())
            .unwrap(),
        mllm_store::candidate_creation::initialize::ArmResult::New { .. }
    ));
    let (_, context) = f
        .store
        .candidate_effect_execution(&f.session, &work.initialize_step_id)
        .unwrap();
    for field in ["binding", "deadline", "token", "launch", "time"] {
        let mut wrong = context.clone();
        match field {
            "binding" => wrong.binding_id.push('x'),
            "deadline" => wrong.deadline_ms += 1,
            "token" => wrong.token.revision += 1,
            "launch" => wrong.launch_settings = None,
            _ => wrong.issued_at_ms += 1,
        }
        assert!(
            f.store
                .revalidate_candidate_initialize_send(
                    &f.session,
                    &work.initialize_step_id,
                    &wrong,
                    1200
                )
                .is_err(),
            "{field}"
        );
    }
    assert!(f
        .store
        .revalidate_candidate_initialize_send(&f.session, &work.initialize_step_id, &context, 1200)
        .is_ok());
    let session = f.store.begin_coordinator_session().unwrap();
    assert!(f.store.next_candidate_initialize(&f.session, 1200).is_err());
    assert!(f
        .store
        .next_candidate_initialize(&session, 1200)
        .unwrap()
        .is_none());
    assert!(f
        .store
        .revalidate_candidate_initialize_send(&session, &work.initialize_step_id, &context, 1200)
        .is_err());
    assert!(f
        .store
        .candidate_initialize_command_receipt(
            &session,
            "owner",
            f.created.run_id(),
            "init",
            &json!({"expected_revision":1,"action":"initialize","deadline_ms":400000}).to_string()
        )
        .is_err());
}

#[test]
fn candidate_worker_does_not_discover_historical_initialize_as_v3_work() {
    let (_dir, owner, run, _, _) = candidate_fixture::fixture();
    let state = owner.lock().unwrap();
    state
        .store()
        .accept_candidate_initialize(
            state.session(),
            "owner",
            &run,
            "legacy",
            r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#,
            1100,
        )
        .unwrap();
    assert!(state
        .store()
        .next_candidate_initialize(state.session(), 1200)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn candidate_binding_collision_preserves_original_immutable_instance() {
    let (_dir, owner, run, observations, _) = candidate_fixture::fixture();
    let binding = {
        let state = owner.lock().unwrap();
        state
            .store()
            .candidate_run_snapshot("owner", &run)
            .unwrap()
            .unwrap()
            .receipt()
            .binding_id()
            .to_owned()
    };
    let gate = Gate::new(false);
    let worker = worker_candidate(
        owner,
        observations,
        gate.clone(),
        Arc::new(AtomicI64::new(0)),
        Duration::from_secs(30),
    );
    let original = candidate::CandidateDriver::fake(Arc::new(|| Ok(1900)));
    worker
        .shared
        .retained_candidates
        .lock()
        .unwrap()
        .insert(binding.clone(), original.clone());
    worker
        .commands()
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while matches!(worker.status(), WorkerStatus::Running) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(Arc::ptr_eq(
        worker
            .shared
            .retained_candidates
            .lock()
            .unwrap()
            .get(&binding)
            .unwrap(),
        &original
    ));
    assert!(gate.calls.lock().unwrap().is_empty());
    worker.shutdown().await.unwrap();
}

#[test]
fn candidate_discovery_and_history_classify_oversized_or_blob_records_as_corruption() {
    for target in ["discovery", "history"] {
        for encoding in ["blob", "oversized", "invalid-json"] {
            let f = fixture::fixture();
            let column = if target == "discovery" {
                "plan_json"
            } else {
                "response_json"
            };
            let table = if target == "discovery" {
                "lifecycle_runs"
            } else {
                "command_receipts"
            };
            let filter = if target == "discovery" {
                "1=1"
            } else {
                "idempotency_key='init'"
            };
            let value = match encoding {
                "blob" => format!("CAST({column} AS BLOB)"),
                "oversized" => format!("{column} || replace(hex(zeroblob(1048576)),'0',' ')"),
                _ => "'{'".into(),
            };
            f.sql
                .execute_batch(&format!(
                    "UPDATE {table} SET {column}={value} WHERE {filter}"
                ))
                .unwrap();
            let error = if target == "discovery" {
                f.store
                    .next_candidate_initialize(&f.session, 1200)
                    .unwrap_err()
            } else {
                f.store
                    .candidate_initialize_command_receipt(
                        &f.session,
                        "owner",
                        f.created.run_id(),
                        "init",
                        r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#,
                    )
                    .unwrap_err()
            };
            assert!(
                matches!(error, LifecycleError::CorruptStoredData),
                "{target}/{encoding}: {error:?}"
            );
        }
    }
}

#[tokio::test]
async fn candidate_policy_revoked_during_effect_prevents_required_probe_and_keeps_history() {
    let (dir, owner, run, observations, mut host) = candidate_fixture::fixture();
    let gate = Gate::new(false);
    let probes = Arc::new(AtomicI64::new(0));
    let worker = worker_candidate(
        owner.clone(),
        observations,
        gate.clone(),
        probes.clone(),
        Duration::from_secs(30),
    );
    let commands = worker.commands();
    let accepted = commands
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    gate.entered().await;
    host["qualification_policy"]["revision"] = json!(2);
    host["qualification_policy"]["allow_qualification_runs"] = json!(false);
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let denied = mllm_config::effective::resolve_effective(&golden["input"]["deployment"], &host)
        .unwrap()
        .host;
    {
        let state = owner.lock().unwrap();
        state
            .store()
            .import_qualification_policy(state.session(), &denied)
            .unwrap();
        assert!(state
            .store()
            .next_candidate_initialize(state.session(), 1900)
            .is_err());
    }
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(30), async {
        while matches!(worker.status(), WorkerStatus::Running) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.shutdown().await.unwrap();
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert_eq!(*gate.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    assert_eq!(
        commands
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap()
            .operation_id(),
        accepted.operation_id()
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    assert_eq!(
        sql.query_row("SELECT COUNT(*) FROM resource_grants", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn candidate_owned_service_clock_advances_effect_and_probe_observations() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let now = Arc::new(AtomicI64::new(1200));
    let clock = now.clone();
    let worker = OwnedCoordinator::spawn_fake(
        owner,
        Arc::new(Observations(observations)),
        Arc::new(move || Ok(clock.fetch_add(1, Ordering::SeqCst))),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let accepted = worker
        .commands()
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    settled(&worker, &sql, accepted.operation_id()).await;
    let times:Vec<(i64,i64)>=sql.prepare("SELECT json_extract(s.step_json,'$.issued_at_ms'),json_extract(e.evidence_json,'$.observed_at_ms') FROM lifecycle_steps s JOIN lifecycle_evidence e ON e.step_id=s.id WHERE s.ordinal>0 ORDER BY s.ordinal").unwrap()
        .query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(times.len(), 2);
    assert!(times[0].1 > times[0].0);
    assert!(times[1].1 > times[1].0);
    assert!(times[1].0 > times[0].1);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn candidate_clock_regression_after_send_validation_prevents_initialize_effect() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let sql = Arc::new(Mutex::new(
        rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap(),
    ));
    let armed_reads = Arc::new(AtomicI64::new(0));
    let reads = armed_reads.clone();
    let clock: ServiceClock = Arc::new(move || {
        let armed: bool = sql
            .lock()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE ordinal=1 AND state='armed')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if armed && reads.fetch_add(1, Ordering::SeqCst) > 0 {
            Ok(1100)
        } else {
            Ok(1900)
        }
    });
    let gate = Gate::new(false);
    let worker = worker_candidate_clock(
        owner,
        observations,
        gate.clone(),
        Arc::new(AtomicI64::new(0)),
        Duration::from_millis(250),
        clock,
    );
    worker
        .commands()
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while matches!(worker.status(), WorkerStatus::Running) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.shutdown().await.unwrap();
    assert!(armed_reads.load(Ordering::SeqCst) >= 2);
    assert!(
        gate.calls.lock().unwrap().is_empty(),
        "clock regressed behind durable arm before effect"
    );
}

#[tokio::test]
async fn candidate_required_probe_failure_preserves_lease_and_never_replays() {
    for failure in ["clock", "panic", "timeout", "shutdown", "commit"] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let gate = Gate::new(false);
        gate.release.add_permits(1);
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let probes = Arc::new(AtomicI64::new(0));
        let candidate_gate = gate.clone();
        let probe_entered = entered.clone();
        let probe_release = release.clone();
        let calls = probes.clone();
        let worker = OwnedCoordinator::spawn_with_candidate_factory(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            CoordinatorOptions {
                protocol_timeout: if failure == "timeout" {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(30)
                },
                ..CoordinatorOptions::default()
            },
            Arc::new(move |_| Ok(test_driver(candidate_gate.clone()))),
            Arc::new(move || {
                let engine = gate.clone();
                let entered = probe_entered.clone();
                let release = probe_release.clone();
                let calls = calls.clone();
                Ok(Arc::new(candidate::CandidateDriver {
                    engine: gate.clone(),
                    probe: Arc::new(move |dispatch| {
                        let engine = engine.clone();
                        let entered = entered.clone();
                        let release = release.clone();
                        let calls = calls.clone();
                        Box::pin(async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            let observed = crate::qualification::collect_probe_with_clock(
                                &engine.engine,
                                dispatch,
                                &|| {
                                    if failure == "clock" {
                                        Err(LifecycleError::Invalid)
                                    } else {
                                        Ok(1900)
                                    }
                                },
                            )
                            .await;
                            entered.add_permits(1);
                            assert_ne!(
                                failure, "panic",
                                "injected probe panic after terminal response"
                            );
                            if failure != "clock" {
                                release.acquire().await.unwrap().forget();
                            }
                            observed.map_err(|e| CoordinatorError::Service(e.to_string()))
                        })
                    }),
                }))
            }),
        )
        .unwrap();
        let commands = worker.commands();
        let accepted = commands
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        if failure == "commit" {
            sql.execute_batch("CREATE TRIGGER fail_probe_result BEFORE INSERT ON lifecycle_evidence BEGIN SELECT RAISE(ABORT,'injected probe commit failure'); END;").unwrap();
            release.add_permits(1);
        }
        if failure != "shutdown" {
            tokio::time::timeout(Duration::from_secs(30), async {
                while matches!(worker.status(), WorkerStatus::Running) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        worker.shutdown().await.unwrap();
        assert_eq!(probes.load(Ordering::SeqCst), 1, "{failure}");
        assert_eq!(
            sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "{failure}"
        );
        assert_eq!(
            sql.query_row(
                "SELECT COUNT(*) FROM lifecycle_steps WHERE ordinal=1 AND state='completed'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            sql.query_row(
                "SELECT COUNT(*) FROM lifecycle_steps WHERE ordinal=2 AND state='armed'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            commands
                .initialize_candidate("owner", &run, 1, "init", 400000)
                .unwrap()
                .operation_id(),
            accepted.operation_id()
        );
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }
}
