use super::*;
use mllm_store::lifecycle::LifecycleError;

async fn fixture() -> (
    Store,
    mllm_store::dispatch::CoordinatorSession,
    rusqlite::Connection,
    tempfile::TempDir,
) {
    let source = fixture_support::owned_source().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("start.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    let store = Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let sql = rusqlite::Connection::open(path).unwrap();
    (store, session, sql, dir)
}

fn counts(sql: &rusqlite::Connection) -> Vec<i64> {
    [
        "operations",
        "lifecycle_runs",
        "lifecycle_steps",
        "runtime_bindings",
        "endpoint_leases",
        "management_events",
        "command_receipts",
        "resource_grants",
        "resource_owners",
        "request_leases",
    ]
    .map(|table| {
        sql.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    })
    .to_vec()
}

#[tokio::test]
async fn start_receipt_faults_are_atomic_and_historical_corruption_is_rejected() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    for (table, condition) in [
        ("command_receipts", "NEW.idempotency_key='start'"),
        (
            "management_events",
            "NEW.kind='qualified_initialize_accepted'",
        ),
    ] {
        sql.execute_batch(&format!("CREATE TRIGGER fail_start BEFORE INSERT ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT,'start rollback'); END;")).unwrap();
        let before = counts(&sql);
        assert!(matches!(
            store.accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 10000),
            Err(LifecycleError::Sql(_))
        ));
        assert_eq!(counts(&sql), before);
        sql.execute_batch("DROP TRIGGER fail_start").unwrap();
    }
    let accepted = store
        .accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    sql.execute_batch("CREATE TRIGGER fail_join BEFORE INSERT ON command_receipts WHEN NEW.idempotency_key='join' BEGIN SELECT RAISE(ABORT,'join rollback'); END;").unwrap();
    let before_join = counts(&sql);
    assert!(matches!(
        store.accept_qualified_start_command(&session, "owner", id, 1, "join", 1801, 20000),
        Err(LifecycleError::Sql(_))
    ));
    assert_eq!(counts(&sql), before_join);
    sql.execute_batch("DROP TRIGGER fail_join").unwrap();
    for corruption in [
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.unknown',1) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.method','GET') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.action','stop') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.principal','someone') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.scope','wrong') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.request_hash','wrong') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET operation_id=(SELECT id FROM operations WHERE kind='managed_configuration_create' LIMIT 1) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.receipt.generation',2) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.receipt.joined',json('true')) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=response_json || printf('%1048577s',' ') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=CAST(response_json AS BLOB) WHERE idempotency_key='start'",
        "UPDATE lifecycle_steps SET step_json=CAST(step_json AS BLOB) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_steps SET step_json=step_json || printf('%1048577s',' ') WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.unknown',1) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.accepted_at_ms',1801) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_runs SET plan_json=json_set(plan_json,'$.unknown',1) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_runs SET deadline_ms=9999 WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE runtime_bindings SET incarnation='bad' WHERE id IN (SELECT binding_id FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start'))",
        "UPDATE operations SET kind='ordinary_cleanup' WHERE id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "DELETE FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "DELETE FROM lifecycle_runs WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) SELECT 'ambiguous',operation_id,1,deployment_id,binding_id,session_id,state,step_json FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
    ] {
        let corrupted_dir = tempfile::tempdir().unwrap();
        let corrupted_path = corrupted_dir.path().join("corrupt.sqlite3");
        sql.execute("VACUUM INTO ?1", [corrupted_path.to_str().unwrap()])
            .unwrap();
        let corrupted_store = Store::open(&corrupted_path).unwrap();
        let corrupted_sql = rusqlite::Connection::open(&corrupted_path).unwrap();
        // Missing historical rows deliberately violate references in this
        // disposable copy; positive fixtures always use the normal writers.
        corrupted_sql
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        corrupted_sql
            .execute_batch(corruption)
            .unwrap_or_else(|error| panic!("{corruption}: {error}"));
        assert!(
            matches!(
                corrupted_store.accept_qualified_start_command(
                    &session, "owner", id, 1, "start", 90000, 10000
                ),
                Err(LifecycleError::CorruptStoredData)
            ),
            "{corruption}"
        );
    }
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        accepted
    );
}

#[tokio::test]
async fn start_receipt_observes_ready_cleanup_replacement_and_revoked_policy() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    let receipt = store
        .accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    let joined = store
        .accept_qualified_start_command(&session, "owner", id, 1, "join", 1801, 11000)
        .unwrap();
    assert!(joined.joined());
    let raw: String = sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let effective = mllm_config::effective::decode_effective_snapshot(&raw).unwrap();
    let controls = store
        .resource_policy(&effective.host.name)
        .unwrap()
        .unwrap()
        .controls;
    let limits: Vec<_> = controls
        .domains
        .iter()
        .map(|(domain, d)| mllm_domain::resources::MemoryLimit {
            domain: domain.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    store
        .arm_step(
            &session,
            receipt.step_id(),
            AdmissionContext::new(
                &source.observations,
                &limits,
                1900,
                controls.observation_ttl_ms,
                controls.max_parked as usize,
            ),
        )
        .unwrap();
    let fake = FakeEngine::for_qualification();
    let observation = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context: store
                .qualified_initialize_execution(&session, receipt.step_id())
                .unwrap(),
        })
        .await
        .unwrap();
    store
        .record_owned_launch(
            &session,
            receipt.step_id(),
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
    store
        .complete_step(
            &session,
            receipt.step_id(),
            &CompletionEvidence {
                token: observation.token,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                control_receipt: Some(observation.receipt),
                milestones: observation.facts,
            },
            1950,
            controls.observation_ttl_ms,
        )
        .unwrap();
    let ready = counts(&sql);
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert!(store
        .accept_qualified_start_command(&session, "owner", id, 1, "new-ready", 2000, 10000)
        .is_err());
    assert_eq!(counts(&sql), ready);
    let stop = store
        .accept_ordinary_cleanup(&session, "owner", &source.fence, "stop", 2000, 10000)
        .unwrap();
    let stopping = counts(&sql);
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert_eq!(counts(&sql), stopping);
    assert!(matches!(
        store.accept_qualified_start_command(&session, "owner", id, 1, "stop", 2000, 10000),
        Err(LifecycleError::IdempotencyConflict)
    ));
    let (_, context) = store
        .arm_ordinary_cleanup_with_context(&session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        mllm_controller::qualification::collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    store
        .complete_cleanup(
            &session,
            &stop.step_id,
            &gone,
            2150,
            controls.observation_ttl_ms,
        )
        .unwrap();
    let stopped = counts(&sql);
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert_eq!(counts(&sql), stopped);
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
    host["runtime_profiles"]["local"]["qualification_id"] =
        json!(effective.profile.qualification_id);
    let replaced = store
        .replace_stopped_managed_configuration(
            &session,
            "owner",
            "replace",
            id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2200,
        )
        .unwrap();
    // Ordinary Starts retain their existing resource-sharing policy gate;
    // candidate-run policy removal does not revoke an already-qualified recipe.
    let snapshot = store
        .resource_policy(&effective.host.name)
        .unwrap()
        .unwrap();
    let mut revoked = snapshot.controls;
    revoked.device_sharing = mllm_config::effective::Sharing::Exclusive;
    for sharing in revoked.device_sharing_overrides.values_mut() {
        *sharing = mllm_config::effective::Sharing::Exclusive;
    }
    store
        .update_resource_policy(
            &session,
            "owner",
            &effective.host.name,
            snapshot.revision,
            "revoke-sharing",
            &revoked,
            &source.observations,
            2250,
        )
        .unwrap();
    let current = store.begin_coordinator_session().unwrap();
    let before = counts(&sql);
    assert_eq!(
        store
            .accept_qualified_start_command(&current, "owner", id, 1, "join", 90000, 11000)
            .unwrap(),
        joined
    );
    assert_eq!(
        store
            .accept_qualified_start_command(&current, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert!(store
        .accept_qualified_start_command(
            &current,
            "owner",
            id,
            replaced.revision,
            "new",
            2300,
            10000
        )
        .is_err());
    assert_eq!(counts(&sql), before);
    assert!(store
        .arm_step(
            &current,
            receipt.step_id(),
            AdmissionContext::new(
                &source.observations,
                &limits,
                2300,
                controls.observation_ttl_ms,
                controls.max_parked as usize
            )
        )
        .is_err());
}

#[tokio::test]
async fn start_receipt_concurrent_same_key_has_one_acceptance() {
    let (store, session, sql, dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let before = counts(&sql);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let store = Store::open(&dir.path().join("start.sqlite3")).unwrap();
            let session = session.clone();
            let id = source.fence.deployment_id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .accept_qualified_start_command(&session, "owner", &id, 1, "race", 1800, 10000)
                    .unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(receipts[0], receipts[1]);
    assert!(!receipts[0].joined());
    assert_eq!(
        counts(&sql)
            .iter()
            .zip(&before)
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>(),
        [1, 1, 1, 1, 1, 1, 1, 0, 0, 0]
    );
    assert!(store
        .qualified_initialize_execution(&session, receipts[0].step_id())
        .is_err());
}

#[tokio::test]
async fn start_receipt_acceptance_replay_join_and_scopes_have_no_execution_effect() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    let before = counts(&sql);
    let epoch = store.resource_snapshot().unwrap().epoch;
    let accepted = store
        .accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    assert!(!accepted.joined());
    assert_eq!(
        (
            accepted.revision(),
            accepted.generation(),
            accepted.accepted_at_ms(),
            accepted.deadline_ms()
        ),
        (1, 1, 1800, 10000)
    );
    let after = counts(&sql);
    assert_eq!(
        after
            .iter()
            .zip(&before)
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>(),
        [1, 1, 1, 1, 1, 1, 1, 0, 0, 0]
    );
    assert_eq!(store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        accepted
    );
    assert_eq!(counts(&sql), after);
    let long = "a".repeat(257);
    for (principal, target, revision, key, now, deadline) in [
        ("", id.as_str(), 1, "start", 1800, 10000),
        (long.as_str(), id.as_str(), 1, "start", 1800, 10000),
        ("owner", "invalid", 1, "start", 1800, 10000),
        ("owner", id.as_str(), 0, "start", 1800, 10000),
        ("owner", id.as_str(), 1, "", 1800, 10000),
        ("owner", id.as_str(), 1, long.as_str(), 1800, 10000),
        ("owner", id.as_str(), 1, "start", -1, 10000),
        ("owner", id.as_str(), 1, "start", 1800, 0),
        ("owner", id.as_str(), 1, "new-expired", 1800, 1800),
    ] {
        assert!(matches!(
            store.accept_qualified_start_command(
                &session, principal, target, revision, key, now, deadline
            ),
            Err(LifecycleError::Invalid)
        ));
    }
    assert_eq!(counts(&sql), after);
    let joined = store
        .accept_qualified_start_command(&session, "owner", id, 1, "join", 1801, 20000)
        .unwrap();
    assert!(joined.joined());
    assert_eq!(joined.operation_id(), accepted.operation_id());
    assert_eq!(
        (joined.deadline_ms(), joined.accepted_at_ms()),
        (10000, 1800)
    );
    assert_eq!(
        counts(&sql)
            .iter()
            .zip(&after)
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>(),
        [0, 0, 0, 0, 0, 0, 1, 0, 0, 0]
    );
    assert_eq!(store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "join", 90000, 20000)
            .unwrap(),
        joined
    );
    let principal = store
        .accept_qualified_start_command(&session, "another", id, 1, "start", 1802, 11000)
        .unwrap();
    assert!(principal.joined());
    assert_eq!(principal.operation_id(), accepted.operation_id());
    let other = store
        .accept_qualified_start_command(
            &session,
            "owner",
            &source.other.deployment_id,
            1,
            "start",
            1800,
            10000,
        )
        .unwrap();
    assert_ne!(other.operation_id(), accepted.operation_id());
    let fixed = counts(&sql);
    for (revision, key, deadline) in [
        (2, "start", 10000),
        (1, "start", 10001),
        (2, "stale", 10000),
    ] {
        let error = store
            .accept_qualified_start_command(&session, "owner", id, revision, key, 1802, deadline)
            .unwrap_err();
        if key == "stale" {
            assert!(matches!(error, LifecycleError::RevisionConflict));
        } else {
            assert!(matches!(error, LifecycleError::IdempotencyConflict));
        }
    }
    assert!(matches!(
        store.accept_ordinary_cleanup(&session, "owner", &source.fence, "start", 1802, 10000),
        Err(LifecycleError::Conflict)
    ));
    assert_eq!(counts(&sql), fixed);
    let current = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.accept_qualified_start_command(&session, "owner", id, 1, "start", 1802, 10000),
        Err(LifecycleError::Stale)
    ));
    assert_eq!(
        store
            .accept_qualified_start_command(&current, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        accepted
    );
}
