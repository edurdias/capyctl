use super::*;
use mllm_store::ordinary_lifecycle::worker::QualifiedInitializeStatus;

fn terminal(sql: &rusqlite::Connection, step: &str) -> (String, String, String, String, String) {
    sql.query_row("SELECT s.state,r.state,o.state,o.error_code,b.state FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id JOIN operations o ON o.id=s.operation_id JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.id=?1", [step], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap()
}

#[tokio::test]
async fn expired_unarmed_is_atomic_at_deadline_and_replays_history_after_replacement() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    let receipt = store
        .accept_qualified_start_command(&session, "owner", id, 1, "expiry", 1800, 1901)
        .unwrap();
    let plan: String = sql
        .query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [receipt.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    let before = counts(&sql);
    let ledger = store.resource_snapshot().unwrap();
    assert!(store
        .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 1900)
        .is_err());
    assert_eq!(counts(&sql), before);
    sql.execute_batch("CREATE TRIGGER expiry_failure BEFORE INSERT ON management_events WHEN NEW.kind='qualified_initialize_expired_unarmed' BEGIN SELECT RAISE(ABORT,'expiry rollback'); END;").unwrap();
    assert!(matches!(
        store.expire_unarmed_qualified_initialize(&session, receipt.step_id(), 1901),
        Err(LifecycleError::Sql(_))
    ));
    assert_eq!(counts(&sql), before);
    assert_eq!(
        store.runtime_binding(id).unwrap().unwrap().state,
        "reserved"
    );
    assert_eq!(
        store
            .qualified_initialize_status(&session, receipt.step_id(), 1900)
            .unwrap(),
        QualifiedInitializeStatus::Planned
    );
    sql.execute_batch("DROP TRIGGER expiry_failure").unwrap();
    assert!(store
        .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 1901)
        .unwrap());
    assert_eq!(
        terminal(&sql, receipt.step_id()),
        (
            "cancelled".into(),
            "failed".into(),
            "failed".into(),
            "deadline_expired_unarmed".into(),
            "released".into()
        )
    );
    assert_eq!(store.resource_snapshot().unwrap(), ledger);
    assert!(store.runtime_binding(id).unwrap().is_none());
    let state: (String,String,bool,bool) = sql.query_row("SELECT desired_state,observed_state,admission_enabled,dispatch_enabled FROM deployments WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(state, ("stopped".into(), "stopped".into(), false, false));
    assert_eq!(
        store
            .qualified_initialize_status(&session, receipt.step_id(), 1901)
            .unwrap(),
        QualifiedInitializeStatus::ExpiredUnarmed
    );
    let ended = counts(&sql);
    assert!(!store
        .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 1902)
        .unwrap());
    assert_eq!(counts(&sql), ended);
    assert_eq!(
        sql.query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [receipt.step_id()],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        plan
    );
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "expiry", 90000, 1901)
            .unwrap(),
        receipt
    );

    let golden: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let effective: Value = serde_json::from_str(&plan).unwrap();
    let effective: Value =
        serde_json::from_str(effective["effective_json"].as_str().unwrap()).unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    config["name"] = json!("ordinary");
    config["routes"] = json!(["ordinary-expired-replaced"]);
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["qualification_id"] =
        effective["profile"]["qualification_id"].clone();
    store
        .replace_stopped_managed_configuration(
            &session,
            "owner",
            "replace-expired",
            id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2000,
        )
        .unwrap();
    let replaced = counts(&sql);
    assert_eq!(
        store
            .qualified_initialize_status(&session, receipt.step_id(), 2001)
            .unwrap(),
        QualifiedInitializeStatus::Superseded
    );
    assert_eq!(
        store
            .accept_qualified_start_command(&session, "owner", id, 1, "expiry", 90000, 1901)
            .unwrap(),
        receipt
    );
    assert!(store
        .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 2001)
        .is_err());
    assert_eq!(counts(&sql), replaced);
}

#[tokio::test]
async fn expired_unarmed_rejects_contradictions_and_stale_ownership_without_release() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let receipt = store
        .accept_qualified_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "expiry",
            1800,
            1901,
        )
        .unwrap();
    for corruption in [
        "UPDATE deployments SET revision=revision+1 WHERE id=(SELECT deployment_id FROM operations WHERE id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry'))",
        "UPDATE deployments SET current_generation=current_generation+1 WHERE id=(SELECT deployment_id FROM operations WHERE id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry'))",
        "UPDATE lifecycle_claims SET generation=generation+1",
        "UPDATE lifecycle_claims SET operation_id=(SELECT operation_id FROM lifecycle_runs WHERE operation_id!=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry') LIMIT 1)",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution',json('{}')) WHERE id=(SELECT json_extract(response_json,'$.receipt.step_id') FROM command_receipts WHERE idempotency_key='expiry')",
        "UPDATE runtime_bindings SET identities_json='[{}]' WHERE state='reserved'",
        "UPDATE runtime_bindings SET incarnation='contradiction' WHERE state='reserved'",
        "UPDATE endpoint_leases SET port=port+1",
        "UPDATE deployments SET dispatch_enabled=1 WHERE desired_state='ready'",
        "UPDATE deployments SET observed_state='ready' WHERE desired_state='ready'",
        "INSERT INTO request_leases SELECT 'retained-lease',deployment_id,revision,generation,session_id,'uncertain' FROM lifecycle_runs WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO resource_grants SELECT 'retained-grant',deployment_id,operation_id,'{}',999999 FROM lifecycle_runs WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO resource_owners SELECT deployment_id,'{}' FROM lifecycle_runs WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO owned_launch_associations SELECT id,binding_id,'contradiction','{}' FROM lifecycle_steps WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "UPDATE lifecycle_steps SET grant_id='retained-grant' WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
    ] {
        let copy = tempfile::tempdir().unwrap();
        let path = copy.path().join("corrupt.sqlite3");
        sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let candidate = Store::open(&path).unwrap();
        let corrupt = rusqlite::Connection::open(&path).unwrap();
        corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        corrupt.execute_batch(corruption).unwrap_or_else(|e|panic!("{corruption}: {e}"));
        let before = counts(&corrupt);
        assert!(candidate.expire_unarmed_qualified_initialize(&session,receipt.step_id(),1901).is_err(),"{corruption}");
        assert_eq!(counts(&corrupt),before,"{corruption}");
        assert_eq!(corrupt.query_row("SELECT state FROM runtime_bindings WHERE id=?1",[receipt.binding_id()],|r|r.get::<_,String>(0)).unwrap(),"reserved");
    }
    let current = store.begin_coordinator_session().unwrap();
    let before = counts(&sql);
    assert!(store
        .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 1901)
        .is_err());
    assert!(store
        .expire_unarmed_qualified_initialize(&current, receipt.step_id(), 1901)
        .is_err());
    assert_eq!(counts(&sql), before);
}

pub(super) fn limits(
    store: &Store,
    sql: &rusqlite::Connection,
    id: &str,
) -> (Vec<mllm_domain::resources::MemoryLimit>, i64, usize) {
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
    (
        controls
            .domains
            .iter()
            .map(|(domain, d)| mllm_domain::resources::MemoryLimit {
                domain: domain.clone(),
                managed_bytes: d.managed_limit,
                free_reserve_bytes: d.free_reserve,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect(),
        controls.observation_ttl_ms,
        controls.max_parked as usize,
    )
}

#[tokio::test]
async fn expired_unarmed_and_arm_serialize_on_independent_connections() {
    let (store, session, sql, dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let receipt = store
        .accept_qualified_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "expiry",
            1800,
            1901,
        )
        .unwrap();
    let (limits, ttl, max_parked) = limits(&store, &sql, &source.fence.deployment_id);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let arm_store = Store::open(&dir.path().join("start.sqlite3")).unwrap();
    let arm_session = session.clone();
    let arm_step = receipt.step_id().to_owned();
    let arm_barrier = barrier.clone();
    let observations = source.observations.clone();
    let arm = std::thread::spawn(move || {
        arm_barrier.wait();
        arm_store.arm_qualified_initialize_with_context(
            &arm_session,
            &arm_step,
            AdmissionContext::new(&observations, &limits, 1900, ttl, max_parked),
        )
    });
    let expire_store = Store::open(&dir.path().join("start.sqlite3")).unwrap();
    let expire_session = session.clone();
    let expire_step = receipt.step_id().to_owned();
    let expiry = std::thread::spawn(move || {
        barrier.wait();
        expire_store.expire_unarmed_qualified_initialize(&expire_session, &expire_step, 1901)
    });
    let armed = arm.join().unwrap();
    let expired = expiry.join().unwrap();
    assert_ne!(armed.is_ok(), expired.is_ok());
    if armed.is_ok() {
        let retained = store.resource_snapshot().unwrap();
        assert!(retained.owners.contains_key(&source.fence.deployment_id));
        assert_eq!(
            store
                .runtime_binding(&source.fence.deployment_id)
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );
        assert!(store
            .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 2000)
            .is_err());
        assert_eq!(store.resource_snapshot().unwrap(), retained);
    } else {
        assert!(expired.unwrap());
        assert!(store.resource_snapshot().unwrap().owners.is_empty());
        assert!(store
            .runtime_binding(&source.fence.deployment_id)
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn expired_unarmed_never_releases_armed_without_association_and_proves_retry_state() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let receipt = store
        .accept_qualified_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "expiry",
            1800,
            1901,
        )
        .unwrap();
    let (limits, ttl, max_parked) = limits(&store, &sql, &source.fence.deployment_id);
    store
        .arm_qualified_initialize_with_context(
            &session,
            receipt.step_id(),
            AdmissionContext::new(&source.observations, &limits, 1900, ttl, max_parked),
        )
        .unwrap();
    let before = counts(&sql);
    let retained = store.resource_snapshot().unwrap();
    assert!(store
        .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 1901)
        .is_err());
    assert_eq!(counts(&sql), before);
    assert_eq!(store.resource_snapshot().unwrap(), retained);
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM owned_launch_associations WHERE step_id=?1",
            [receipt.step_id()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        store
            .runtime_binding(&source.fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );

    let receipt = store
        .accept_qualified_start_command(
            &session,
            "owner",
            &source.other.deployment_id,
            1,
            "other-expiry",
            1800,
            1901,
        )
        .unwrap();
    store
        .expire_unarmed_qualified_initialize(&session, receipt.step_id(), 1901)
        .unwrap();
    // A failed row alone must never be treated as a successful exact retry.
    for corruption in [
        "UPDATE operations SET error_code='arbitrary_failure' WHERE id=?1",
        "UPDATE lifecycle_steps SET state='planned' WHERE operation_id=?1",
        "UPDATE lifecycle_runs SET state='queued' WHERE operation_id=?1",
        "UPDATE runtime_bindings SET state='reserved' WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE operation_id=?1)",
        "INSERT INTO lifecycle_claims SELECT deployment_id,operation_id,revision,generation FROM lifecycle_runs WHERE operation_id=?1",
    ] {
        let copy = tempfile::tempdir().unwrap();
        let path = copy.path().join("terminal.sqlite3");
        sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let candidate = Store::open(&path).unwrap();
        let corrupt = rusqlite::Connection::open(path).unwrap();
        corrupt.execute(corruption,[receipt.operation_id()]).unwrap();
        let before = counts(&corrupt);
        assert!(candidate.expire_unarmed_qualified_initialize(&session,receipt.step_id(),1902).is_err(),"{corruption}");
        assert_eq!(counts(&corrupt),before);
    }
}
