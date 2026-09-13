use super::*;
use mllm_config::effective::{
    DevicePolicy, DomainPolicy, HostPolicy, PortRange, QueuePolicy, Sharing,
};
use mllm_config::resource_controls::ResourceControls;
use mllm_domain::resources::MemoryObservation;
use std::collections::BTreeMap;

fn host() -> HostPolicy {
    HostPolicy {
        name: "host-a".into(),
        hardware_fingerprint: "hw".into(),
        environment_fingerprint: "env".into(),
        domains: BTreeMap::from([(
            "system".into(),
            DomainPolicy {
                managed_limit: 80,
                free_reserve: 20,
                host_kv_limit: Some(40),
                parked_limit: Some(50),
            },
        )]),
        devices: BTreeMap::from([(
            "gpu0".into(),
            DevicePolicy {
                domain: "system".into(),
                sharing: Sharing::Shared,
            },
        )]),
        max_parked: 2,
        observation_ttl_ms: 2_000,
        device_sharing: Sharing::Shared,
        endpoint_port_range: PortRange {
            start: 20_000,
            end: 20_100,
        },
        planner_max_states: 4_096,
        queue: QueuePolicy {
            max_pending_per_deployment: 64,
            max_pending_total: 256,
            max_buffered_bytes_total: 64 << 20,
            request_deadline_ms: 600_000,
            admission_window_ms: 2_000,
        },
        qualification_policy: None,
    }
}

fn observations() -> Vec<MemoryObservation> {
    vec![MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 100,
        available_bytes: 90,
        sampled_at_ms: 10_000,
    }]
}

#[test]
fn bootstrap_is_durable_and_reopen_prefers_persisted_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let store = crate::Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let imported = store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    assert_eq!(
        (imported.revision, imported.epoch, imported.changed),
        (1, 1, true)
    );
    drop(store);

    let store = crate::Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let mut local = host();
    local.domains.get_mut("system").unwrap().managed_limit = 99;
    let reopened = store
        .import_resource_policy(&session, &local, &observations(), 11_000)
        .unwrap();
    assert!(!reopened.changed);
    assert_eq!(reopened.controls.domains["system"].managed_limit, 80);
    assert_eq!(store.resource_snapshot().unwrap().epoch, 1);
}

#[test]
fn bootstrap_rejects_untrusted_observation_shapes_and_capacity() {
    for bad in [
        vec![],
        vec![observations()[0].clone(), observations()[0].clone()],
        vec![MemoryObservation {
            domain: "other".into(),
            ..observations()[0].clone()
        }],
        vec![MemoryObservation {
            available_bytes: 101,
            ..observations()[0].clone()
        }],
        vec![MemoryObservation {
            sampled_at_ms: 8_999,
            ..observations()[0].clone()
        }],
        vec![MemoryObservation {
            capacity_bytes: 99,
            available_bytes: 90,
            ..observations()[0].clone()
        }],
    ] {
        let store = crate::Store::open_in_memory().unwrap();
        let session = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.import_resource_policy(&session, &host(), &bad, 11_000),
            Err(ResourcePolicyError::Invalid)
        ));
    }
}

#[test]
fn update_replays_exact_result_conflicts_on_changed_body_and_scopes_principal() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let mut controls = ResourceControls::from_host(&host());
    controls.domains.get_mut("system").unwrap().managed_limit = 70;
    let first = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "key",
            &controls,
            &observations(),
            11_000,
        )
        .unwrap();
    let replay = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "key",
            &controls,
            &observations(),
            11_000,
        )
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(first.revision, 2);
    assert_eq!(store.resource_snapshot().unwrap().epoch, 2);
    controls.max_parked = 1;
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "key",
            &controls,
            &observations(),
            11_000
        ),
        Err(ResourcePolicyError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "bob",
            "host-a",
            1,
            "key",
            &controls,
            &observations(),
            11_000
        ),
        Err(ResourcePolicyError::RevisionConflict)
    ));
}

#[test]
fn lowering_limits_retains_all_owners_and_reports_each_overcommit_category() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    store.conn.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('owner','owner','model',NULL,'stopped',1,0,1,1)", []).unwrap();
    let footprint = serde_json::json!({"version":1,"phase":"parked","allocations":[["system",60,30]],"devices":[]}).to_string();
    store
        .conn
        .execute(
            "INSERT INTO resource_owners(owner_id,footprint_json) VALUES('owner',?1)",
            [footprint],
        )
        .unwrap();
    let mut controls = ResourceControls::from_host(&host());
    let domain = controls.domains.get_mut("system").unwrap();
    domain.managed_limit = 40;
    domain.host_kv_limit = Some(20);
    domain.parked_limit = Some(30);
    controls.max_parked = 0;
    let result = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "over",
            &controls,
            &observations(),
            11_000,
        )
        .unwrap();
    assert_eq!(
        result.overcommit.domains["system"],
        DomainOvercommit {
            managed_bytes: 20,
            host_kv_bytes: 10,
            parked_bytes: 30
        }
    );
    assert_eq!(result.overcommit.parked_owners, 1);
    assert!(store
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key("owner"));
}

#[test]
fn generic_reader_handles_host_targets_and_legacy_reader_excludes_them() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let controls = ResourceControls::from_host(&host());
    let result = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "key",
            &controls,
            &observations(),
            11_000,
        )
        .unwrap();
    let operation = store
        .get_management_operation(&result.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        operation.target,
        ManagementOperationTarget::HostResourcePolicy {
            host_id: "host-a".into(),
            revision: 2
        }
    );
    assert_eq!(operation.state, crate::OpState::Succeeded);
    assert!(store.get_operation(&result.operation_id).unwrap().is_none());
}

#[test]
fn failed_receipt_write_rolls_back_policy_epoch_operation_and_event() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    store.conn.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON command_receipts BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    let controls = ResourceControls::from_host(&host());
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "key",
            &controls,
            &observations(),
            11_000
        ),
        Err(ResourcePolicyError::Sql(_))
    ));
    assert_eq!(
        store.resource_policy("host-a").unwrap().unwrap().revision,
        1
    );
    assert_eq!(store.resource_snapshot().unwrap().epoch, 1);
    let operations: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM operations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(operations, 0);
}

#[test]
fn independent_connections_serialize_updates_from_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.sqlite3");
    let store = crate::Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    drop(store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut threads = Vec::new();
    for key in ["left", "right"] {
        let path = path.clone();
        let session = session.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let store = crate::Store::open(&path).unwrap();
            let controls = ResourceControls::from_host(&host());
            barrier.wait();
            store.update_resource_policy(
                &session,
                key,
                "host-a",
                1,
                key,
                &controls,
                &observations(),
                11_000,
            )
        }));
    }
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ResourcePolicyError::RevisionConflict)))
            .count(),
        1
    );
    let store = crate::Store::open(&path).unwrap();
    assert_eq!(
        store.resource_policy("host-a").unwrap().unwrap().revision,
        2
    );
    assert_eq!(store.resource_snapshot().unwrap().epoch, 2);
}
