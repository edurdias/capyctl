use super::*;

fn body(deployment: &str) -> String {
    json!({"model":format!("candidate-{deployment}"),"messages":[{"role":"user","content":"Repeat exactly: MLLM_ALPHA_71"}],"temperature":0,"max_tokens":16,"stream":false}).to_string()
}

#[tokio::test]
async fn candidate_inference_uncertain_terminal_commit_timeout_and_shutdown_never_replay() {
    for failure in [
        "uncertain",
        "commit",
        "timeout",
        "shutdown",
        "terminal-clock",
        "session",
    ] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let sends = Arc::new(AtomicI64::new(0));
        let factory_entered = entered.clone();
        let factory_release = release.clone();
        let factory_sends = sends.clone();
        let worker = OwnedCoordinator::spawn_with_candidate_factory(
            owner.clone(),
            Arc::new(Observations(observations)),
            Arc::new(|| Ok(1900)),
            CoordinatorOptions {
                protocol_timeout: Duration::from_secs(3),
                ..CoordinatorOptions::default()
            },
            Arc::new(|_| Err(CoordinatorError::Invalid)),
            Arc::new(move || {
                let engine = Arc::new(FakeEngine::for_qualification());
                let probe_engine = engine.clone();
                let entered = factory_entered.clone();
                let release = factory_release.clone();
                let sends = factory_sends.clone();
                Ok(Arc::new(candidate::CandidateDriver {
                    parked_status: Arc::new(|_| Err(CoordinatorError::Invalid)),
                    engine,
                    security_control: Arc::new(|_| Box::pin(async { Err(CoordinatorError::Invalid) })),
                    probe: Arc::new(move |dispatch| {
                        let engine = probe_engine.clone();
                        let entered = entered.clone();
                        let release = release.clone();
                        let sends = sends.clone();
                        Box::pin(async move {
                            let marker = dispatch.request().contains("MLLM_ALPHA_71");
                            if marker {
                                sends.fetch_add(1, Ordering::SeqCst);
                            }
                            let mut result = crate::qualification::collect_probe_with_clock(
                                &engine,
                                dispatch,
                                &|| Ok(1900),
                            )
                            .await
                            .map_err(|e| CoordinatorError::Service(e.to_string()))?;
                            if marker {
                                entered.add_permits(1);
                                release.acquire().await.unwrap().forget();
                                if failure == "uncertain" {
                                    result.terminal =
                                        mllm_domain::qualification::CandidateTerminal::Uncertain;
                                }
                                if failure == "terminal-clock" {
                                    result.observed_at_ms = 500001;
                                }
                            }
                            Ok(result)
                        })
                    }),
                }))
            }),
        )
        .unwrap();
        let commands = worker.commands();
        let init = commands
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        settled(&worker, &sql, init.operation_id()).await;
        let request = body(init.deployment_id());
        let commands_copy = commands.clone();
        let run_copy = run.clone();
        let body_copy = request.clone();
        let accepted = tokio::task::spawn_blocking(move || {
            commands_copy.candidate_inference("owner", &run_copy, 1, "marker", &body_copy)
        })
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(Duration::from_secs(30), entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert!(
            owner.try_lock().is_ok(),
            "Store guard across inference await"
        );
        assert_eq!(
            commands
                .candidate_inference("owner", &run, 1, "marker", &request)
                .unwrap(),
            accepted
        );
        let queued = if failure == "shutdown" {
            let cmd = commands.clone();
            let r = run.clone();
            let b = request.clone();
            let pending = tokio::task::spawn_blocking(move || {
                cmd.candidate_inference("owner", &r, 1, "queued", &b)
            });
            tokio::time::timeout(Duration::from_secs(30), async {
                while worker.shared.candidate_requests.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                worker.shared.observers.available_permits(),
                worker.shared.options.max_observers - 2
            );
            Some(pending)
        } else {
            None
        };
        match failure {
            "commit" => {
                sql.execute_batch("CREATE TRIGGER fail_marker_result BEFORE INSERT ON qualification_request_results BEGIN SELECT RAISE(ABORT,'injected marker commit failure'); END;").unwrap();
                release.add_permits(1);
            }
            "session" => {
                owner
                    .lock()
                    .unwrap()
                    .store()
                    .begin_coordinator_session()
                    .unwrap();
                release.add_permits(1);
            }
            "uncertain" | "terminal-clock" => release.add_permits(1),
            _ => {}
        }
        if failure != "shutdown" {
            tokio::time::timeout(Duration::from_secs(30), async {
                while worker.status() == WorkerStatus::Running {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        worker.shutdown().await.unwrap();
        if let Some(queued) = queued {
            assert!(matches!(
                queued.await.unwrap(),
                Err(CoordinatorCommandError::Coordinator(
                    CoordinatorError::Stopped(_)
                ))
            ));
        }
        assert_eq!(sends.load(Ordering::SeqCst), 1, "{failure}");
        assert_eq!(
            sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "{failure}"
        );
        assert_eq!(sql.query_row("SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3' AND state='succeeded'",[],|r|r.get::<_,i64>(0)).unwrap(),0,"{failure}");
        assert_eq!(sql.query_row("SELECT COUNT(*) FROM qualification_evidence_refs WHERE json_extract(metadata_json,'$.source_id')=?1",[&accepted.operation_id],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(
            sql.query_row("SELECT COUNT(*) FROM resource_grants", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        if failure != "session" {
            assert_eq!(
                commands
                    .candidate_inference("owner", &run, 1, "marker", &request)
                    .unwrap(),
                accepted
            );
        }
        assert!(commands
            .candidate_inference("owner", &run, 1, "next", &request)
            .is_err());
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn candidate_inference_lost_handoff_history_needs_exact_lease_and_never_resends() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations.clone())),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let commands = worker.commands();
    let accepted = commands
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    settled(&worker, &sql, accepted.operation_id()).await;
    worker.shutdown().await.unwrap();
    let body = body(accepted.deployment_id());
    let dispatch = {
        let state = owner.lock().unwrap();
        let work = state
            .store()
            .candidate_inference_work(state.session(), "owner", &run, 1, 1900)
            .unwrap();
        let limits: Vec<_> = work
            .policy
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
        let grant = state
            .store()
            .grant_candidate_inference(
                state.session(),
                "owner",
                &run,
                "lost",
                &body,
                mllm_scheduler::residency::AdmissionContext::new(
                    &observations,
                    &limits,
                    1900,
                    work.policy.controls.observation_ttl_ms,
                    work.policy.controls.max_parked as usize,
                ),
            )
            .unwrap();
        let mllm_store::candidate_creation::progression::CandidateDispatchResult::New(dispatch) =
            grant
        else {
            panic!("first grant")
        };
        dispatch
    };
    // The only New permission was lost before worker handoff. History is still
    // observable after shutdown, but must neither execute nor settle this lease.
    let retry = commands
        .candidate_inference("owner", &run, 1, "lost", &body)
        .unwrap();
    assert_eq!(retry.operation_id, dispatch.request_operation_id());
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM qualification_request_results",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(dispatch);
    sql.execute("DELETE FROM request_leases", []).unwrap();
    assert!(matches!(
        commands.candidate_inference("owner", &run, 1, "lost", &body),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::CorruptStoredData
        ))
    ));
}

#[tokio::test]
async fn candidate_inference_history_rejects_nontext_and_oversized_result_before_allocation() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
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
    let request = body(init.deployment_id());
    let cmd = commands.clone();
    let r = run.clone();
    let b = request.clone();
    let accepted =
        tokio::task::spawn_blocking(move || cmd.candidate_inference("owner", &r, 1, "marker", &b))
            .await
            .unwrap()
            .unwrap();
    settled(&worker, &sql, &accepted.operation_id).await;
    worker.shutdown().await.unwrap();
    let original: String = sql
        .query_row(
            "SELECT evidence_json FROM qualification_request_results WHERE request_operation_id=?1",
            [&accepted.operation_id],
            |r| r.get(0),
        )
        .unwrap();
    sql.execute_batch("PRAGMA ignore_check_constraints=ON;")
        .unwrap();
    for change in ["blob", "oversized"] {
        if change == "blob" {
            sql.execute("UPDATE qualification_request_results SET evidence_json=CAST(?1 AS BLOB) WHERE request_operation_id=?2",rusqlite::params![original,accepted.operation_id]).unwrap();
        } else {
            sql.execute("UPDATE qualification_request_results SET evidence_json=?1 WHERE request_operation_id=?2",rusqlite::params![" ".repeat((1<<20)+1),accepted.operation_id]).unwrap();
        }
        let error = commands
            .candidate_inference("owner", &run, 1, "marker", &request)
            .unwrap_err();
        assert!(
            matches!(
                error,
                CoordinatorCommandError::Lifecycle(LifecycleError::CorruptStoredData)
            ),
            "{change}: {error:?}"
        );
    }
}

#[tokio::test]
async fn candidate_inference_exact_grant_revalidates_scope_policy_deadline_and_session() {
    let (dir, owner, run, observations, mut host) = candidate_fixture::fixture();
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations.clone())),
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
    worker.shutdown().await.unwrap();
    let state = owner.lock().unwrap();
    let request = body(init.deployment_id());
    let work = state
        .store()
        .candidate_inference_work(state.session(), "owner", &run, 1, 1900)
        .unwrap();
    let limits: Vec<_> = work
        .policy
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
    let admission = |now| {
        mllm_scheduler::residency::AdmissionContext::new(
            &observations,
            &limits,
            now,
            work.policy.controls.observation_ttl_ms,
            work.policy.controls.max_parked as usize,
        )
    };
    let mut wrong_revision = work.clone();
    wrong_revision.revision += 1;
    assert!(matches!(
        state.store().grant_candidate_inference_work(
            state.session(),
            &wrong_revision,
            "marker",
            &request,
            admission(1900)
        ),
        Err(LifecycleError::RevisionConflict)
    ));
    let granted = state
        .store()
        .grant_candidate_inference_work(state.session(), &work, "marker", &request, admission(1900))
        .unwrap();
    let mllm_store::candidate_creation::progression::CandidateDispatchResult::New(dispatch) =
        granted
    else {
        panic!("new grant")
    };
    state
        .store()
        .revalidate_candidate_inference_send(state.session(), &work, &dispatch, admission(1900))
        .unwrap();
    for field in [
        "principal",
        "run",
        "binding",
        "incarnation",
        "host",
        "revision",
        "deadline",
        "policy",
    ] {
        let mut wrong = work.clone();
        match field {
            "principal" => wrong.principal = "other".into(),
            "run" => wrong.run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            "binding" => wrong.binding_id = "other".into(),
            "incarnation" => wrong.incarnation = "other".into(),
            "host" => wrong.host_id = "other".into(),
            "revision" => wrong.revision += 1,
            "deadline" => wrong.deadline_ms += 1,
            _ => wrong.policy.revision += 1,
        }
        assert!(
            state
                .store()
                .revalidate_candidate_inference_send(
                    state.session(),
                    &wrong,
                    &dispatch,
                    admission(1900)
                )
                .is_err(),
            "{field}"
        );
    }
    for now in [1899, 500000, 500001] {
        assert!(state
            .store()
            .revalidate_candidate_inference_send(state.session(), &work, &dispatch, admission(now))
            .is_err());
    }
    assert!(matches!(
        state
            .store()
            .grant_candidate_inference(
                state.session(),
                "owner",
                &run,
                "marker",
                &request,
                admission(1900)
            )
            .unwrap(),
        mllm_store::candidate_creation::progression::CandidateDispatchResult::AlreadyRecorded { .. }
    ));
    host["qualification_policy"]["revision"] = json!(2);
    host["qualification_policy"]["allow_qualification_runs"] = json!(false);
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let denied = mllm_config::effective::resolve_effective(&golden["input"]["deployment"], &host)
        .unwrap()
        .host;
    state
        .store()
        .import_qualification_policy(state.session(), &denied)
        .unwrap();
    assert!(state
        .store()
        .revalidate_candidate_inference_send(state.session(), &work, &dispatch, admission(1900))
        .is_err());
    let session = state.store().begin_coordinator_session().unwrap();
    assert!(state
        .store()
        .revalidate_candidate_inference_send(&session, &work, &dispatch, admission(1900))
        .is_err());
    assert!(state
        .store()
        .candidate_inference_command_receipt(state.session(), "owner", &run, 1, "marker", &request)
        .is_err());
    assert_eq!(
        sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM qualification_request_results",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn candidate_inference_missing_original_runtime_never_constructs_or_grants() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let worker = OwnedCoordinator::spawn_fake(
        owner,
        Arc::new(Observations(observations)),
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
    worker.shared.retained_candidates.lock().unwrap().clear();
    let request = body(init.deployment_id());
    let result = tokio::task::spawn_blocking(move || {
        commands.candidate_inference("owner", &run, 1, "marker", &request)
    })
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::RuntimeRetained
        ))
    ));
    assert!(worker.shared.retained_candidates.lock().unwrap().is_empty());
    assert_eq!(
        sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    worker.shutdown().await.unwrap();
}
