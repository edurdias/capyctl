use super::*;
use crate::Store;
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    (value["deployment"].clone(), value["host"].clone())
}

#[test]
fn stopped_acceptance_replays_original_receipt_and_never_opens_dispatch() {
    let (config, host) = fixture();
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let effective = mllm_config::effective::resolve_effective(&config, &host).unwrap();
    let observations = vec![mllm_domain::resources::MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 60 << 30,
        sampled_at_ms: 1,
    }];
    store
        .import_resource_policy(&session, &effective.host, &observations, 1)
        .unwrap();
    let body = json!({"config":config}).to_string();
    let receipt = store
        .create_stopped_managed_configuration(&session, "principal", "key", &body, &host, 10)
        .unwrap();
    assert_eq!(receipt.revision, 1);
    assert_eq!(receipt.generation, 1);
    assert_eq!(
        receipt,
        store
            .create_stopped_managed_configuration(&session, "principal", "key", &body, &host, 20)
            .unwrap()
    );
    let state: (String, String, i64, i64) = store.conn.query_row("SELECT desired_state,observed_state,admission_enabled,dispatch_enabled FROM deployments WHERE id=?1", [&receipt.deployment_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(state, ("stopped".into(), "stopped".into(), 0, 0));
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM runtime_bindings", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

fn setup() -> (Store, CoordinatorSession, Value, Value) {
    let (config, host) = fixture();
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let effective = resolve_effective(&config, &host).unwrap();
    store
        .import_resource_policy(
            &session,
            &effective.host,
            &[mllm_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64 << 30,
                available_bytes: 60 << 30,
                sampled_at_ms: 1,
            }],
            1,
        )
        .unwrap();
    (store, session, config, host)
}

#[test]
fn replacement_preserves_history_and_replay_survives_revision_advance() {
    let (store, session, mut config, host) = setup();
    let first = store
        .create_stopped_managed_configuration(
            &session,
            "p",
            "key",
            &json!({"config":config}).to_string(),
            &host,
            10,
        )
        .unwrap();
    config["routes"] = json!(["new-route"]);
    let body = json!({"config":config,"expected_revision":1}).to_string();
    let second = store
        .replace_stopped_managed_configuration(
            &session,
            "p",
            "replace",
            &first.deployment_id,
            &body,
            &host,
            20,
        )
        .unwrap();
    assert_eq!((second.revision, second.generation), (2, 2));
    assert_eq!(
        second,
        store
            .replace_stopped_managed_configuration(
                &session,
                "p",
                "replace",
                &first.deployment_id,
                &body,
                &host,
                30
            )
            .unwrap()
    );
    assert!(matches!(
        store.replace_stopped_managed_configuration(
            &session,
            "p",
            "other",
            &first.deployment_id,
            &body,
            &host,
            30
        ),
        Err(ManagedConfigurationError::RevisionConflict)
    ));
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM effective_revisions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT route FROM deployment_routes", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "new-route"
    );
}

#[test]
fn semantic_replay_sorts_route_aliases_and_normalizes_units() {
    let (store, session, mut config, host) = setup();
    config["routes"] = json!(["z", "a"]);
    let first = store
        .create_stopped_managed_configuration(
            &session,
            "p",
            "key",
            &json!({"config":config}).to_string(),
            &host,
            1,
        )
        .unwrap();
    config["routes"] = json!(["a", "z"]);
    config["request_deadline"] = json!("300000ms");
    assert_eq!(
        first,
        store
            .create_stopped_managed_configuration(
                &session,
                "p",
                "key",
                &json!({"config":config}).to_string(),
                &host,
                2
            )
            .unwrap()
    );
}

#[test]
fn corrupt_receipt_operation_target_is_not_replayed() {
    let (store, session, config, host) = setup();
    let body = json!({"config":config}).to_string();
    let first = store
        .create_stopped_managed_configuration(&session, "p", "key", &body, &host, 1)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE operations SET kind='other' WHERE id=?1",
            [first.operation_id],
        )
        .unwrap();
    assert!(matches!(
        store.create_stopped_managed_configuration(&session, "p", "key", &body, &host, 2),
        Err(ManagedConfigurationError::CorruptStoredData)
    ));
}

#[test]
fn strict_input_rejects_activation_duplicates_unknown_fields_and_bounds() {
    let (store, session, config, host) = setup();
    for body in [
        json!({"config":config,"activate":true}).to_string(),
        json!({"config":config,"extra":null}).to_string(),
        format!("{{\"config\":{config},\"config\":{config}}}"),
        format!(
            "{{\"config\":{}}}",
            config
                .to_string()
                .replacen("\"name\":\"toy\"", "\"name\":\"toy\",\"name\":\"toy\"", 1)
        ),
        " ".repeat((1 << 20) + 1),
    ] {
        assert!(matches!(
            store.create_stopped_managed_configuration(&session, "p", "k", &body, &host, 1),
            Err(ManagedConfigurationError::Invalid)
        ));
    }
    for principal in ["".to_string(), "p\n".into(), "p".repeat(257)] {
        assert!(matches!(
            store.create_stopped_managed_configuration(
                &session,
                &principal,
                "k",
                &json!({"config":config}).to_string(),
                &host,
                1
            ),
            Err(ManagedConfigurationError::Invalid)
        ));
    }
    assert_eq!(store.deployment_count().unwrap(), 0);
}

#[test]
fn changed_semantics_conflict_but_other_principal_does_not_replay() {
    let (store, session, mut config, host) = setup();
    let body = json!({"config":config}).to_string();
    store
        .create_stopped_managed_configuration(&session, "p", "k", &body, &host, 1)
        .unwrap();
    config["name"] = json!("changed");
    assert!(matches!(
        store.create_stopped_managed_configuration(
            &session,
            "p",
            "k",
            &json!({"config":config}).to_string(),
            &host,
            1
        ),
        Err(ManagedConfigurationError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.create_stopped_managed_configuration(&session, "other", "k", &body, &host, 1),
        Err(ManagedConfigurationError::RouteConflict)
    ));
}

#[test]
fn current_policy_and_session_required_without_resetting_policy() {
    let (store, session, config, mut host) = setup();
    let body = json!({"config":config}).to_string();
    host["resource_policy"]["max_parked"] = json!(15);
    assert!(matches!(
        store.create_stopped_managed_configuration(&session, "p", "k", &body, &host, 1),
        Err(ManagedConfigurationError::PolicyConflict)
    ));
    assert_eq!(
        store
            .resource_policy("lab")
            .unwrap()
            .unwrap()
            .controls
            .max_parked,
        16
    );
    store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.create_stopped_managed_configuration(&session, "p", "k", &body, &host, 1),
        Err(ManagedConfigurationError::StaleSession)
    ));
    assert_eq!(store.deployment_count().unwrap(), 0);
}

#[test]
fn retained_accounting_or_runtime_denies_replacement_without_revision_change() {
    for insert in [
        "INSERT INTO resource_owners(owner_id,footprint_json) VALUES(?1,'{}')",
        "INSERT INTO owners(id,kind,deployment_id) VALUES('owner','managed',?1)",
        "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('lease',?1,1,1,'old','uncertain')",
        "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding',?1,1,'incarnation','managed','{}','[]','uncertain')",
        "UPDATE deployments SET observed_state='unknown' WHERE id=?1",
    ] {
        let (store,session,config,host)=setup();
        let first=store.create_stopped_managed_configuration(&session,"p","k",&json!({"config":config}).to_string(),&host,1).unwrap();
        store.conn.execute(insert,[&first.deployment_id]).unwrap();
        assert!(matches!(store.replace_stopped_managed_configuration(&session,"p","r",&first.deployment_id,&json!({"config":config,"expected_revision":1}).to_string(),&host,2),Err(ManagedConfigurationError::RuntimeRetained)),"{insert}");
        assert_eq!(store.conn.query_row("SELECT revision FROM deployments",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
}

#[test]
fn legacy_route_collision_and_event_failure_roll_back_every_write() {
    let (store, session, config, host) = setup();
    store.conn.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('legacy','legacy','model','toy','stopped',0,0,1,1)",[]).unwrap();
    let body = json!({"config":config}).to_string();
    assert!(matches!(
        store.create_stopped_managed_configuration(&session, "p", "k", &body, &host, 1),
        Err(ManagedConfigurationError::RouteConflict)
    ));
    store
        .conn
        .execute(
            "UPDATE deployments SET route_model_id=NULL WHERE id='legacy'",
            [],
        )
        .unwrap();
    store.conn.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'test'); END;").unwrap();
    assert!(store
        .create_stopped_managed_configuration(&session, "p", "k", &body, &host, 1)
        .is_err());
    for table in [
        "operations",
        "effective_revisions",
        "deployment_routes",
        "command_receipts",
    ] {
        assert_eq!(
            store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(store.deployment_count().unwrap(), 1);
}

#[test]
fn two_connections_race_for_one_route_without_partial_loser() {
    let (initial, _, config, host) = setup();
    drop(initial);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    first
        .conn
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    second
        .conn
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let session = first.begin_coordinator_session().unwrap();
    let effective = resolve_effective(&config, &host).unwrap();
    first
        .import_resource_policy(
            &session,
            &effective.host,
            &[mllm_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64 << 30,
                available_bytes: 60 << 30,
                sampled_at_ms: 1,
            }],
            1,
        )
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let barrier = barrier.clone();
            let session = session.clone();
            let mut config = config.clone();
            let host = host.clone();
            config["name"] = json!(format!("deployment-{index}"));
            std::thread::spawn(move || {
                barrier.wait();
                store.create_stopped_managed_configuration(
                    &session,
                    "p",
                    &format!("key-{index}"),
                    &json!({"config":config}).to_string(),
                    &host,
                    1,
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(ManagedConfigurationError::RouteConflict)))
            .count(),
        1
    );
    let store = Store::open(&path).unwrap();
    assert_eq!(store.deployment_count().unwrap(), 1);
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM command_receipts", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn ambiguous_legacy_routes_block_new_configuration_globally() {
    let (store, session, config, host) = setup();
    for id in ["legacy-a", "legacy-b"] {
        store.conn.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES(?1,?1,'model','unrelated','stopped',0,0,1,1)",[id]).unwrap();
    }
    assert!(matches!(
        store.create_stopped_managed_configuration(
            &session,
            "p",
            "k",
            &json!({"config":config}).to_string(),
            &host,
            1
        ),
        Err(ManagedConfigurationError::RouteConflict)
    ));
}

#[test]
fn released_binding_with_retained_endpoint_still_blocks_update() {
    let (store, session, config, host) = setup();
    let first = store
        .create_stopped_managed_configuration(
            &session,
            "p",
            "k",
            &json!({"config":config}).to_string(),
            &host,
            1,
        )
        .unwrap();
    store.conn.execute("INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding',?1,1,'incarnation','managed','{}','[]','released')",[&first.deployment_id]).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO endpoint_leases(host,port,binding_id) VALUES('lab',8100,'binding')",
            [],
        )
        .unwrap();
    assert!(matches!(
        store.replace_stopped_managed_configuration(
            &session,
            "p",
            "r",
            &first.deployment_id,
            &json!({"config":config,"expected_revision":1}).to_string(),
            &host,
            2
        ),
        Err(ManagedConfigurationError::RuntimeRetained)
    ));
}

#[test]
fn replay_checks_frozen_revision_content_not_just_its_existence() {
    let (store, session, config, host) = setup();
    let body = json!({"config":config}).to_string();
    store
        .create_stopped_managed_configuration(&session, "p", "k", &body, &host, 1)
        .unwrap();
    store
        .conn
        .execute("UPDATE effective_revisions SET effective_json='{}'", [])
        .unwrap();
    assert!(matches!(
        store.create_stopped_managed_configuration(&session, "p", "k", &body, &host, 2),
        Err(ManagedConfigurationError::CorruptStoredData)
    ));
}
