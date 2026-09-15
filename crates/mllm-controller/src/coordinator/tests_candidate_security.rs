use super::*;

struct SecurityFault {
    child: usize,
    failure: &'static str,
    entered: Semaphore,
    release: Semaphore,
}

impl SecurityFault {
    async fn after_terminal(&self, child: usize) {
        if self.child != child {
            return;
        }
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        assert_ne!(
            self.failure, "panic",
            "injected Security panic after terminal check"
        );
    }
    fn time(&self, child: usize) -> Result<i64, LifecycleError> {
        if self.child == child && self.failure == "clock" {
            Err(LifecycleError::Invalid)
        } else {
            Ok(1900)
        }
    }
}

pub(super) async fn baseline_requests(
    worker: &OwnedCoordinator,
    sql: &rusqlite::Connection,
    run: &str,
    deployment: &str,
    count: usize,
) {
    for (index, (marker, stream)) in [
        ("MLLM_ALPHA_71", false),
        ("MLLM_BETA_29", false),
        ("MLLM_ALPHA_71", true),
        ("MLLM_BETA_29", true),
    ]
    .into_iter()
    .take(count)
    .enumerate()
    {
        let body = json!({"model":format!("candidate-{deployment}"),"messages":[{"role":"user","content":format!("Repeat exactly: {marker}")}],"temperature":0,"max_tokens":16,"stream":stream}).to_string();
        let commands = worker.commands();
        let run = run.to_owned();
        let receipt = tokio::task::spawn_blocking(move || {
            commands.candidate_inference("owner", &run, 1, &format!("baseline-{index}"), &body)
        })
        .await
        .unwrap()
        .unwrap();
        settled(worker, sql, &receipt.operation_id).await;
    }
}

#[tokio::test]
async fn candidate_security_owned_progresses_only_after_complete_baseline() {
    let (dir, owner, run, observations, _) = candidate_fixture::fixture();
    let worker = OwnedCoordinator::spawn_with_candidate_factory(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(|_| Err(CoordinatorError::Invalid)),
        Arc::new(|| Ok(candidate::CandidateDriver::fake(Arc::new(|| Ok(1900))))),
    )
    .unwrap();
    let receipt = worker
        .commands()
        .initialize_candidate("owner", &run, 1, "init", 400000)
        .unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    settled(&worker, &sql, receipt.operation_id()).await;
    let before = owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners;
    baseline_requests(&worker, &sql, &run, receipt.deployment_id(), 3).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let count = || {
        sql.query_row("SELECT count(*) FROM lifecycle_runs WHERE json_extract(plan_json,'$.action')='security'", [], |r| r.get::<_, i64>(0)).unwrap()
    };
    assert_eq!(count(), 0, "partial baseline must not arm Security");
    baseline_requests(&worker, &sql, &run, receipt.deployment_id(), 4).await;
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let done: i64 = sql.query_row("SELECT count(*) FROM lifecycle_runs WHERE json_extract(plan_json,'$.action')='security' AND state='succeeded'", [], |r| r.get(0)).unwrap();
            if done == 1 { break; }
            assert_eq!(worker.status(), WorkerStatus::Running);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
    let status = worker.shutdown().await.unwrap();
    let runs: Vec<(String, String)> = sql
        .prepare("SELECT state,plan_json FROM lifecycle_runs")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(completed.is_ok(), "owned worker never advanced the internal three-subcheck Security case: {status:?}, {runs:?}");
    assert_eq!(count(), 1);
    assert_eq!(
        sql.query_row("SELECT requests_used FROM qualification_runs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM deployments WHERE admission_enabled=0 AND dispatch_enabled=0",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
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
}

async fn security_failure_matrix(child: usize) {
    for failure in [
        "shutdown",
        "timeout",
        "panic",
        "commit",
        "clock",
        "uncertain",
        "session",
    ] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let fake = Arc::new(FakeEngine::for_qualification());
        let factory_fake = fake.clone();
        let builds = Arc::new(AtomicI64::new(0));
        let factory_builds = builds.clone();
        let fault = Arc::new(SecurityFault {
            child,
            failure,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        let factory_fault = fault.clone();
        let worker = OwnedCoordinator::spawn_with_candidate_factory(
            owner.clone(), Arc::new(Observations(observations)), Arc::new(|| Ok(1900)),
            CoordinatorOptions { protocol_timeout:Duration::from_secs(2), ..CoordinatorOptions::default() },
            Arc::new(|_| Err(CoordinatorError::Invalid)),
            Arc::new(move || {
                factory_builds.fetch_add(1, Ordering::SeqCst);
                let control_engine = factory_fake.clone();
                let probe_engine = factory_fake.clone();
                let control_fault = factory_fault.clone();
                let probe_fault = factory_fault.clone();
                Ok(Arc::new(candidate::CandidateDriver {
                    parked_status: Arc::new(|_| Err(CoordinatorError::Invalid)),
                    engine: factory_fake.clone(),
                    security_control: Arc::new(move |d| {
                        let engine = control_engine.clone(); let fault = control_fault.clone();
                        Box::pin(async move {
                            let result = crate::qualification::collect_security_control_with_clock(&engine,d,&|| fault.time(0)).await;
                            fault.after_terminal(0).await;
                            let mut result = result.map_err(|e| CoordinatorError::Service(e.to_string()))?;
                            if fault.child == 0 && fault.failure == "uncertain" { result.terminal=mllm_domain::qualification::CandidateTerminal::Uncertain; }
                            Ok(result)
                        })
                    }),
                    probe: Arc::new(move |d| {
                        let engine=probe_engine.clone(); let fault=probe_fault.clone();
                        Box::pin(async move {
                            let child = d.security_endpoint().map(|e| match e { mllm_domain::qualification::CandidateSecurityEndpoint::Inference=>1, _=>2 });
                            let result=crate::qualification::collect_probe_with_clock(&engine,d,&|| child.map_or(Ok(1900), |child| fault.time(child))).await;
                            if let Some(child)=child { fault.after_terminal(child).await; }
                            let mut result=result.map_err(|e| CoordinatorError::Service(e.to_string()))?;
                            if child==Some(fault.child) && fault.failure=="uncertain" { result.terminal=mllm_domain::qualification::CandidateTerminal::Uncertain; }
                            Ok(result)
                        })
                    }),
                }))
            }),
        ).unwrap();
        let commands = worker.commands();
        let init = commands
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        settled(&worker, &sql, init.operation_id()).await;
        let owners = owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .owners;
        baseline_requests(&worker, &sql, &run, init.deployment_id(), 4).await;
        tokio::time::timeout(Duration::from_secs(30), fault.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert!(
            owner.try_lock().is_ok(),
            "Store guard across Security child {child} {failure}"
        );
        match failure {
            "commit" => {
                sql.execute_batch("CREATE TRIGGER fail_security_result BEFORE INSERT ON lifecycle_evidence BEGIN SELECT RAISE(ABORT,'injected Security evidence failure'); END;").unwrap();
                fault.release.add_permits(1);
            }
            "session" => {
                owner
                    .lock()
                    .unwrap()
                    .store()
                    .begin_coordinator_session()
                    .unwrap();
                fault.release.add_permits(1);
            }
            "shutdown" | "timeout" => {}
            _ => fault.release.add_permits(1),
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
        let status = worker.shutdown().await.unwrap();
        assert!(
            matches!(
                status,
                WorkerStatus::Uncertain { .. } | WorkerStatus::Failed(_)
            ),
            "{child} {failure}: {status:?}"
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.qualification_activity().unwrap(),
            (5 + child as u64, 2, 5),
            "{child} {failure}"
        );
        assert_eq!(
            sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            i64::from(child > 0),
            "{child} {failure}"
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
        assert_eq!(sql.query_row("SELECT count(*) FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id WHERE json_extract(r.plan_json,'$.action')='security' AND s.ordinal>0 AND s.state='completed'",[],|r|r.get::<_,i64>(0)).unwrap(),child as i64);
        assert_eq!(
            owner
                .lock()
                .unwrap()
                .store()
                .resource_snapshot()
                .unwrap()
                .owners,
            owners
        );
        assert!(commands
            .candidate_inference("owner", &run, 1, "later", "{}")
            .is_err());
    }
}

#[tokio::test]
async fn candidate_security_owned_control_failures_retain_authority() {
    security_failure_matrix(0).await;
}
#[tokio::test]
async fn candidate_security_owned_inference_failures_retain_authority() {
    security_failure_matrix(1).await;
}
#[tokio::test]
async fn candidate_security_owned_health_failures_retain_authority() {
    security_failure_matrix(2).await;
}

#[tokio::test]
async fn candidate_security_owned_discovery_never_recreates_or_replays() {
    for already_armed in [false, true] {
        let (dir, owner, run, observations, _) = candidate_fixture::fixture();
        let fake = Arc::new(FakeEngine::for_qualification());
        let engine = fake.clone();
        let builds = Arc::new(AtomicI64::new(0));
        let factory_builds = builds.clone();
        let worker = OwnedCoordinator::spawn_with_candidate_factory(
            owner.clone(),
            Arc::new(Observations(observations.clone())),
            Arc::new(|| Ok(1900)),
            CoordinatorOptions::default(),
            Arc::new(|_| Err(CoordinatorError::Invalid)),
            Arc::new(move || {
                factory_builds.fetch_add(1, Ordering::SeqCst);
                let probe = engine.clone();
                let control = engine.clone();
                Ok(Arc::new(candidate::CandidateDriver {
                    parked_status: Arc::new(|_| Err(CoordinatorError::Invalid)),
                    engine: engine.clone(),
                    probe: Arc::new(move |d| {
                        let engine = probe.clone();
                        Box::pin(async move {
                            crate::qualification::collect_probe_with_clock(&engine, d, &|| Ok(1900))
                                .await
                                .map_err(|e| CoordinatorError::Service(e.to_string()))
                        })
                    }),
                    security_control: Arc::new(move |d| {
                        let engine = control.clone();
                        Box::pin(async move {
                            crate::qualification::collect_security_control_with_clock(
                                &engine,
                                d,
                                &|| Ok(1900),
                            )
                            .await
                            .map_err(|e| CoordinatorError::Service(e.to_string()))
                        })
                    }),
                }))
            }),
        )
        .unwrap();
        let init = worker
            .commands()
            .initialize_candidate("owner", &run, 1, "init", 400000)
            .unwrap();
        let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
        settled(&worker, &sql, init.operation_id()).await;
        let work = {
            let s = owner.lock().unwrap();
            s.store()
                .candidate_inference_work(s.session(), "owner", &run, 1, 1900)
                .unwrap()
        };
        let limits: Vec<_> = work
            .policy
            .controls
            .domains
            .iter()
            .map(|(domain, p)| MemoryLimit {
                domain: domain.clone(),
                managed_bytes: p.managed_limit,
                free_reserve_bytes: p.free_reserve,
                host_kv_bytes: p.host_kv_limit,
                parked_bytes: p.parked_limit,
            })
            .collect();
        let admission = || {
            mllm_scheduler::residency::AdmissionContext::new(
                &observations,
                &limits,
                1900,
                work.policy.controls.observation_ttl_ms,
                work.policy.controls.max_parked as usize,
            )
        };
        for (index, (marker, stream)) in [
            ("MLLM_ALPHA_71", false),
            ("MLLM_BETA_29", false),
            ("MLLM_ALPHA_71", true),
            ("MLLM_BETA_29", true),
        ]
        .into_iter()
        .enumerate()
        {
            let body=json!({"model":format!("candidate-{}",init.deployment_id()),"messages":[{"role":"user","content":format!("Repeat exactly: {marker}")}],"temperature":0,"max_tokens":16,"stream":stream}).to_string();
            let dispatch = {
                let s = owner.lock().unwrap();
                s.store()
                    .grant_candidate_inference_work(
                        s.session(),
                        &work,
                        &format!("baseline-{index}"),
                        &body,
                        admission(),
                    )
                    .unwrap()
            };
            let mllm_store::candidate_creation::progression::CandidateDispatchResult::New(d) =
                dispatch
            else {
                panic!()
            };
            let o = crate::qualification::collect_probe_with_clock(&fake, *d, &|| Ok(1900))
                .await
                .unwrap();
            let s = owner.lock().unwrap();
            let c = s
                .store()
                .candidate_collector(s.session(), "owner", &run)
                .unwrap();
            s.store()
                .record_candidate_result(s.session(), &c, &o, 1900)
                .unwrap();
            // Preserve one worker/session. Hold its Store fence until the
            // durable last marker and injected discovery condition coexist.
            if index == 3 {
                if already_armed {
                    assert!(matches!(
                        s.store().advance_candidate_security(s.session(), &c, admission()).unwrap(),
                        mllm_store::candidate_creation::progression::CandidateSecurityDispatch::NewControl(_)
                    ));
                } else {
                    worker.shared.retained_candidates.lock().unwrap().clear();
                }
            }
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            while worker.status() == WorkerStatus::Running {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        worker.shutdown().await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(fake.qualification_activity().unwrap(), (5, 1, 5));
        assert_eq!(
            sql.query_row("SELECT count(*) FROM resource_grants", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            if already_armed { 2 } else { 1 }
        );
        assert_eq!(
            sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn candidate_security_owned_final_clock_fences_each_child_before_send() {
    for child in 0..3_i64 {
        for invalid_now in [1899, 500000] {
            let (dir, owner, run, observations, _) = candidate_fixture::fixture();
            let path = dir.path().join("srv.sqlite3");
            let armed_samples = Arc::new(AtomicI64::new(0));
            let samples = armed_samples.clone();
            let clock: ServiceClock = Arc::new(move || {
                let sql = rusqlite::Connection::open(&path).unwrap();
                let armed:bool=sql.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id WHERE json_extract(r.plan_json,'$.action')='security' AND s.ordinal=?1 AND s.state='armed')",[child+1],|r|r.get(0)).unwrap();
                if armed && samples.fetch_add(1, Ordering::SeqCst) >= 1 {
                    Ok(invalid_now)
                } else {
                    Ok(1900)
                }
            });
            let real = candidate::CandidateDriver::fake(clock.clone());
            let controls = Arc::new(AtomicI64::new(0));
            let requests = Arc::new(AtomicI64::new(0));
            let probe = real.probe.clone();
            let control = real.security_control.clone();
            let probe_calls = requests.clone();
            let control_calls = controls.clone();
            let driver = Arc::new(candidate::CandidateDriver {
                parked_status: Arc::new(|_| Err(CoordinatorError::Invalid)),
                engine: real.engine.clone(),
                probe: Arc::new(move |d| {
                    if d.security_endpoint().is_some() {
                        probe_calls.fetch_add(1, Ordering::SeqCst);
                    }
                    probe(d)
                }),
                security_control: Arc::new(move |d| {
                    control_calls.fetch_add(1, Ordering::SeqCst);
                    control(d)
                }),
            });
            let worker = OwnedCoordinator::spawn_with_candidate_factory(
                owner.clone(),
                Arc::new(Observations(observations)),
                clock,
                CoordinatorOptions::default(),
                Arc::new(|_| Err(CoordinatorError::Invalid)),
                Arc::new(move || Ok(driver.clone())),
            )
            .unwrap();
            let init = worker
                .commands()
                .initialize_candidate("owner", &run, 1, "init", 400000)
                .unwrap();
            let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
            settled(&worker, &sql, init.operation_id()).await;
            baseline_requests(&worker, &sql, &run, init.deployment_id(), 4).await;
            tokio::time::timeout(Duration::from_secs(30), async {
                while worker.status() == WorkerStatus::Running {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(matches!(
                worker.shutdown().await.unwrap(),
                WorkerStatus::Uncertain { .. }
            ));
            assert_eq!(
                armed_samples.load(Ordering::SeqCst),
                2,
                "one arm revalidation and one final clock sample"
            );
            assert_eq!(controls.load(Ordering::SeqCst), i64::from(child > 0));
            assert_eq!(requests.load(Ordering::SeqCst), (child - 1).max(0));
            assert_eq!(
                sql.query_row("SELECT count(*) FROM request_leases", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                i64::from(child > 0)
            );
            assert_eq!(
                sql.query_row("SELECT count(*) FROM resource_owners", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }
}
