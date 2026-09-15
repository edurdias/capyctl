use super::*;
use mllm_domain::resources::MemoryLimit;
use mllm_store::lifecycle::DeploymentFence;
use mllm_store::ordinary_lifecycle::QualifiedStart;

struct CleanupFixture {
    store: Store,
    session: mllm_store::dispatch::CoordinatorSession,
    sql: rusqlite::Connection,
    observations: Vec<MemoryObservation>,
    limits: Vec<MemoryLimit>,
    ttl: i64,
    max_parked: usize,
    _dir: tempfile::TempDir,
}
impl CleanupFixture {
    fn scalar(&self, sql: &str) -> i64 {
        self.sql.query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn counts(&self) -> Vec<i64> {
        [
            "operations",
            "lifecycle_runs",
            "lifecycle_steps",
            "lifecycle_claims",
            "lifecycle_evidence",
            "command_receipts",
            "qualifications",
            "resource_owners",
            "resource_grants",
            "request_leases",
            "endpoint_leases",
            "management_events",
        ]
        .map(|t| self.scalar(&format!("SELECT COUNT(*) FROM {t}")))
        .to_vec()
    }
    fn admission(&self) -> AdmissionContext<'_> {
        AdmissionContext::new(
            &self.observations,
            &self.limits,
            1900,
            self.ttl,
            self.max_parked,
        )
    }
}

async fn fixture() -> (CleanupFixture, DeploymentFence) {
    let source = fixture_support::owned_source().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    let store = Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let sql = rusqlite::Connection::open(path).unwrap();
    let raw: String = sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [&source.fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let e = mllm_config::effective::decode_effective_snapshot(&raw).unwrap();
    let controls = store
        .resource_policy(&e.host.name)
        .unwrap()
        .unwrap()
        .controls;
    let limits = controls
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
    (
        CleanupFixture {
            store,
            session,
            sql,
            observations: source.observations.clone(),
            limits,
            ttl: controls.observation_ttl_ms,
            max_parked: controls.max_parked as usize,
            _dir: dir,
        },
        source.fence.clone(),
    )
}

async fn started(
    ready: bool,
) -> (
    CleanupFixture,
    DeploymentFence,
    QualifiedStart,
    FakeEngine,
    CompletionEvidence,
) {
    let (f, fence) = fixture().await;
    let start = f
        .store
        .accept_qualified_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let mut admission = f.admission();
    admission.now_ms = 1900;
    f.store
        .arm_step(&f.session, &start.step_id, admission)
        .unwrap();
    let context = f
        .store
        .qualified_initialize_execution(&f.session, &start.step_id)
        .unwrap();
    let fake = FakeEngine::for_qualification();
    let observation = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &start.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            1950,
        )
        .unwrap();
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities,
        observed_at_ms: observation.observed_at_ms,
        control_receipt: Some(observation.receipt),
        milestones: observation.facts,
    };
    if ready {
        f.store
            .complete_step(&f.session, &start.step_id, &evidence, 1950, f.ttl)
            .unwrap();
    }
    (f, fence, start, fake, evidence)
}

#[tokio::test]
async fn ordinary_cleanup_exact_stop_replay_retains_then_releases_once() {
    let (f, fence, start, fake, _) = started(true).await;
    let ticket = f
        .store
        .grant_dispatch(
            &f.session,
            mllm_store::dispatch::DispatchRequest {
                deployment_id: &fence.deployment_id,
                revision: fence.revision,
                generation: fence.generation,
                max_per_deployment: 4,
                max_total: 8,
            },
        )
        .unwrap();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let replay = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2001, 10000)
        .unwrap();
    assert_eq!(stop, replay);
    let resolved = DeploymentFence {
        generation: fence.generation + 1,
        ..fence.clone()
    };
    assert_eq!(
        stop,
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &resolved, "stop", 2001, 10000)
            .unwrap()
    );
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &resolved, "different-key", 2001, 10000)
            .is_err()
    );
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2001, 11000)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store.pending_dispatches(&fence.deployment_id).unwrap()[0].id,
        ticket.id()
    );
    assert!(
        f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id)
    );
    assert!(
        f.store
            .qualified_initialize_execution(&f.session, &start.step_id)
            .is_err()
    );
    let (arm, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    assert!(matches!(arm, ArmResult::New { .. }));
    let (again, none) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2051)
        .unwrap();
    assert_eq!(again, ArmResult::AlreadyRecorded);
    assert!(none.is_none());
    let context = context.unwrap();
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 2100).unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
    assert!(
        !f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id)
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM management_events WHERE kind IN ('ordinary_cleanup_accepted','ordinary_cleanup_armed','ordinary_cleanup_completed')"), 3);
    assert!(
        f.store
            .pending_dispatches(&fence.deployment_id)
            .unwrap()
            .is_empty()
    );
    let counts = f.counts();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 90000, f.ttl)
        .unwrap();
    assert_eq!(f.counts(), counts);
    let mut changed = gone;
    changed.receipt.push('x');
    assert!(
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &changed, 2150, f.ttl)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
    let golden: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    let raw: String = f
        .sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let original = mllm_config::effective::decode_effective_snapshot(&raw).unwrap();
    config["name"] = json!("ordinary");
    config["routes"] = json!(["ordinary-replaced"]);
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://another-admin");
    host["runtime_profiles"]["local"]["qualification_id"] =
        json!(original.profile.qualification_id);
    let replaced = f
        .store
        .replace_stopped_managed_configuration(
            &f.session,
            "owner",
            "replace-after-stop",
            &fence.deployment_id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2200,
        )
        .unwrap();
    let next = DeploymentFence {
        deployment_id: fence.deployment_id.clone(),
        revision: replaced.revision,
        generation: replaced.generation,
    };
    let fresh = f
        .store
        .accept_qualified_start(&f.session, &next, 2200, 10000)
        .unwrap();
    assert_ne!(fresh.binding_id, start.binding_id);
    let mut admission = f.admission();
    admission.now_ms = 2350;
    f.store
        .arm_step(&f.session, &fresh.step_id, admission)
        .unwrap();
    let context = f
        .store
        .qualified_initialize_execution(&f.session, &fresh.step_id)
        .unwrap();
    let observation = FakeEngine::for_qualification()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &fresh.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            2400,
        )
        .unwrap();
    f.store
        .complete_step(
            &f.session,
            &fresh.step_id,
            &CompletionEvidence {
                token: observation.token,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                control_receipt: Some(observation.receipt),
                milestones: observation.facts,
            },
            2400,
            f.ttl,
        )
        .unwrap();
    assert_eq!(
        f.store
            .runtime_binding(&next.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "live"
    );
    assert_eq!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2300, 10000)
            .unwrap(),
        stop
    );
    assert_eq!(
        f.store
            .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2300)
            .unwrap(),
        (ArmResult::AlreadyRecorded, None)
    );
}

#[tokio::test]
async fn ordinary_cleanup_rejects_missing_ready_evidence_before_fencing() {
    let (f, fence, start, _, _) = started(true).await;
    // Named crash corruption: a completed source without its atomic evidence.
    f.sql
        .execute(
            "DELETE FROM lifecycle_evidence WHERE step_id=?1",
            [&start.step_id],
        )
        .unwrap();
    let before = f.counts();
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "live"
    );
}

#[tokio::test]
async fn ordinary_cleanup_armed_and_uncertain_handoff_settles_only_after_verified_exit() {
    for uncertain in [false, true] {
        let (f, fence, start, fake, evidence) = started(false).await;
        if uncertain {
            f.store
                .mark_qualified_initialize_uncertain(&f.session, &start.step_id, 2000)
                .unwrap();
        }
        let stop = f
            .store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .unwrap();
        let before = f.counts();
        assert!(
            f.store
                .complete_step(&f.session, &start.step_id, &evidence, 2010, f.ttl)
                .is_err()
        );
        assert_eq!(f.counts(), before);
        let (prior_state,history):(String,String)=f.sql.query_row("SELECT s.state,r.plan_json FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=?2 WHERE s.id=?1",rusqlite::params![start.step_id,stop.operation_id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(prior_state, if uncertain { "uncertain" } else { "armed" });
        assert!(history.contains(&start.step_id));
        assert!(history.contains(&start.operation_id));
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        let (_, context) = f
            .store
            .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
            .unwrap();
        let gone = mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100)
            .unwrap();
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
            .unwrap();
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
        assert_eq!(
            f.sql
                .query_row(
                    "SELECT state FROM lifecycle_steps WHERE id=?1",
                    [&start.step_id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "cancelled"
        );
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 999999, f.ttl)
            .unwrap();
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
    }
}

#[tokio::test]
async fn ordinary_cleanup_rejects_bad_evidence_and_rolls_back_release_failure() {
    let (f, fence, _, fake, _) = started(true).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    let before = f.counts();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    for mutation in 0..10 {
        let mut wrong = gone.clone();
        let mut now = 2150;
        let mut ttl = f.ttl;
        match mutation {
            0 => {
                wrong.identities.pop();
            }
            1 => wrong.identities[1].start_ticks += 1,
            2 => wrong.binding_id = ulid::Ulid::new().to_string(),
            3 => wrong.incarnation = ulid::Ulid::new().to_string(),
            4 => wrong.receipt = " ".into(),
            5 => wrong.receipt = "x".repeat(524289),
            6 => wrong.observed_at_ms = 2049,
            7 => now = 10001,
            8 => ttl += 1,
            9 => now = wrong.observed_at_ms + f.ttl + 1,
            _ => unreachable!(),
        }
        assert!(
            f.store
                .complete_cleanup(&f.session, &stop.step_id, &wrong, now, ttl)
                .is_err(),
            "mutation {mutation}"
        );
        assert_eq!(f.counts(), before);
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    }
    f.sql.execute_batch("CREATE TRIGGER ordinary_cleanup_release_failure BEFORE INSERT ON management_events WHEN NEW.kind='ordinary_cleanup_completed' BEGIN SELECT RAISE(ABORT,'cleanup release rollback'); END;").unwrap();
    assert!(
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
    f.sql
        .execute_batch("DROP TRIGGER ordinary_cleanup_release_failure;")
        .unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
}

#[tokio::test]
async fn ordinary_cleanup_policy_revocation_and_stale_sessions_keep_original_authority_bounded() {
    let (f, fence, start, fake, _) = started(true).await;
    let raw: String = f
        .sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut host = mllm_config::effective::decode_effective_snapshot(&raw)
        .unwrap()
        .host;
    host.qualification_policy = None;
    f.store
        .import_qualification_policy(&f.session, &host)
        .unwrap();
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    assert_eq!(
        f.sql
            .query_row(
                "SELECT state FROM lifecycle_steps WHERE id=?1",
                [&start.step_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "completed"
    );

    let (f, fence, _, fake, _) = started(false).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    let current = f.store.begin_coordinator_session().unwrap();
    let before = f.counts();
    assert_eq!(
        f.store
            .accept_ordinary_cleanup(&current, "owner", &fence, "stop", 2200, 10000)
            .unwrap(),
        stop
    );
    for session in [&f.session, &current] {
        assert!(
            f.store
                .complete_cleanup(session, &stop.step_id, &gone, 2150, f.ttl)
                .is_err()
        );
        assert!(
            f.store
                .arm_ordinary_cleanup_with_context(session, &stop.step_id, 2200)
                .is_err()
        );
    }
    assert_eq!(f.counts(), before);
    assert!(
        f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id)
    );
}

#[tokio::test]
async fn ordinary_cleanup_unproven_lease_blocks_all_release_and_other_deployment_is_untouched() {
    let (f, fence, _, fake, _) = started(true).await;
    let other = fixture_support::owned_source().await.other.clone();
    let start = f
        .store
        .accept_qualified_start(&f.session, &other, 1800, 10000)
        .unwrap();
    f.store
        .arm_step(&f.session, &start.step_id, f.admission())
        .unwrap();
    let context = f
        .store
        .qualified_initialize_execution(&f.session, &start.step_id)
        .unwrap();
    let observation = FakeEngine::for_qualification()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &start.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            1950,
        )
        .unwrap();
    f.store
        .complete_step(
            &f.session,
            &start.step_id,
            &CompletionEvidence {
                token: observation.token,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                control_receipt: Some(observation.receipt),
                milestones: observation.facts,
            },
            1950,
            f.ttl,
        )
        .unwrap();
    let ticket = f
        .store
        .grant_dispatch(
            &f.session,
            mllm_store::dispatch::DispatchRequest {
                deployment_id: &other.deployment_id,
                revision: other.revision,
                generation: other.generation,
                max_per_deployment: 2,
                max_total: 8,
            },
        )
        .unwrap();
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    // Named corruption: an unexplained older incarnation/session lease.
    f.sql.execute("INSERT INTO request_leases VALUES('unproven-prior-incarnation',?1,1,999,'unknown-session','uncertain')",[&fence.deployment_id]).unwrap();
    let before = f.counts();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    assert!(
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
    f.sql
        .execute(
            "DELETE FROM request_leases WHERE id='unproven-prior-incarnation'",
            [],
        )
        .unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    assert_eq!(
        f.store.pending_dispatches(&other.deployment_id).unwrap()[0].id,
        ticket.id()
    );
    assert!(
        f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&other.deployment_id)
    );
    assert_eq!(
        f.store
            .runtime_binding(&other.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "live"
    );
}

#[tokio::test]
async fn ordinary_cleanup_missing_ownership_and_unarmed_reservations_stay_retained() {
    let (f, fence) = fixture().await;
    let start = f
        .store
        .accept_qualified_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let before = f.counts();
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    f.store
        .arm_step(&f.session, &start.step_id, f.admission())
        .unwrap();
    let before = f.counts();
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
}

#[tokio::test]
async fn ordinary_cleanup_terminal_corruption_never_becomes_recorded_arm_authority() {
    let (f, fence, _, fake, _) = started(true).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    // Named crash corruption: terminal rows without the committed evidence.
    f.sql
        .execute(
            "DELETE FROM lifecycle_evidence WHERE step_id=?1",
            [&stop.step_id],
        )
        .unwrap();
    let before = f.counts();
    assert!(
        f.store
            .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2300)
            .is_err()
    );
    assert_eq!(f.counts(), before);
}

#[tokio::test]
async fn ordinary_cleanup_races_ready_completion_and_duplicate_accept_and_arm() {
    let (f, fence, start, fake, evidence) = started(false).await;
    let path = f._dir.path().join("srv.sqlite3");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let session = f.session.clone();
        let fence = fence.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let store = Store::open(&path).unwrap();
            barrier.wait();
            store.accept_ordinary_cleanup(&session, "owner", &fence, "race-stop", 2000, 10000)
        }));
    }
    let session = f.session.clone();
    let id = start.step_id.clone();
    let gate = barrier.clone();
    let ttl = f.ttl;
    let completion = std::thread::spawn(move || {
        let store = Store::open(&path).unwrap();
        gate.wait();
        store.complete_step(&session, &id, &evidence, 1950, ttl)
    });
    let receipts: Vec<_> = threads
        .into_iter()
        .map(|t| t.join().unwrap().unwrap())
        .collect();
    let _ready_won = completion.join().unwrap().is_ok();
    assert_eq!(receipts[0], receipts[1]);
    let stop = &receipts[0];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let arms: Vec<_> = (0..2)
        .map(|_| {
            let path = f._dir.path().join("srv.sqlite3");
            let session = f.session.clone();
            let id = stop.step_id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store.arm_ordinary_cleanup_with_context(&session, &id, 2050)
            })
        })
        .collect();
    let results: Vec<_> = arms
        .into_iter()
        .map(|t| t.join().unwrap().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|(a, _)| matches!(a, ArmResult::New { .. }))
            .count(),
        1
    );
    assert_eq!(results.iter().filter(|(_, c)| c.is_some()).count(), 1);
    let context = results.into_iter().find_map(|(_, c)| c).unwrap();
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 2100).unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    assert!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn ordinary_cleanup_strict_identity_kind_and_fence_corruption_retains_all_charges() {
    let (f, fence, start, fake, _) = started(true).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    let original_ids: String = f
        .sql
        .query_row(
            "SELECT identities_json FROM runtime_bindings WHERE id=?1",
            [&start.binding_id],
            |r| r.get(0),
        )
        .unwrap();
    let association: String = f
        .sql
        .query_row(
            "SELECT association_json FROM owned_launch_associations WHERE step_id=?1",
            [&start.step_id],
            |r| r.get(0),
        )
        .unwrap();
    for mutation in 0..7 {
        // Named corruption matrix; all starting authority comes from real writers.
        match mutation {
            0 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET identities_json='[]' WHERE id=?1",
                        [&start.binding_id],
                    )
                    .unwrap();
            }
            1 => {
                let mut ids: Value = serde_json::from_str(&original_ids).unwrap();
                ids.as_array_mut().unwrap().pop();
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET identities_json=?2 WHERE id=?1",
                        rusqlite::params![start.binding_id, ids.to_string()],
                    )
                    .unwrap();
            }
            2 => {
                let mut a: Value = serde_json::from_str(&association).unwrap();
                a["identities"][1]["start_ticks"] = json!(123456789);
                f.sql
                    .execute(
                        "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
                        rusqlite::params![start.step_id, a.to_string()],
                    )
                    .unwrap();
            }
            3 => {
                f.sql
                    .execute(
                        "UPDATE operations SET kind='candidate_cleanup' WHERE id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            4 => {
                f.sql
                    .execute(
                        "UPDATE operations SET kind='stop' WHERE id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            5 => {
                f.sql
                    .execute(
                        "UPDATE lifecycle_claims SET generation=generation+1 WHERE operation_id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            6 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET ownership='attached' WHERE id=?1",
                        [&start.binding_id],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = f.counts();
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        assert!(
            f.store
                .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
                .is_err(),
            "mutation {mutation}"
        );
        assert!(
            f.store
                .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2150)
                .is_err(),
            "mutation {mutation}"
        );
        assert_eq!(f.counts(), before);
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
        match mutation {
            0 | 1 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET identities_json=?2 WHERE id=?1",
                        rusqlite::params![start.binding_id, original_ids],
                    )
                    .unwrap();
            }
            2 => {
                f.sql
                    .execute(
                        "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
                        rusqlite::params![start.step_id, association],
                    )
                    .unwrap();
            }
            3 | 4 => {
                f.sql
                    .execute(
                        "UPDATE operations SET kind='ordinary_cleanup' WHERE id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            5 => {
                f.sql
                    .execute(
                        "UPDATE lifecycle_claims SET generation=generation-1 WHERE operation_id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            6 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET ownership='managed' WHERE id=?1",
                        [&start.binding_id],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
    }
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
}
