use super::*;
use mllm_config::effective::{
    DevicePolicy, DomainMemory, DomainPolicy, HostPolicy, PortRange, QueuePolicy, Sharing,
};
use mllm_config::resource_controls::ResourceControls;
use mllm_domain::resources::MemoryObservation;
use std::collections::BTreeMap;

fn host() -> HostPolicy {
    HostPolicy {
        name: "host-a".into(),
        hardware_fingerprint: "hw".into(),
        environment_fingerprint: "env".into(),
        device_inventory_digest: None,
        model_store: "/srv/models".into(),
        domains: BTreeMap::from([(
            "system".into(),
            DomainPolicy {
                managed_limit: 80,
                free_reserve: 20,
                host_kv_limit: Some(40),
                parked_limit: Some(50),
                memory: DomainMemory::Distinct,
            },
        )]),
        devices: BTreeMap::from([(
            "gpu0".into(),
            DevicePolicy {
                physical_gpu_uuid: None,
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

// SPEC §6.2 / ADR 0010 decision 5: a domain's memory topology is a declared
// hardware fact. Nothing else in ResourceControls carries that character — every
// other field is an operator-tunable limit an update may freely change — so only
// `memory` is compared here.
#[test]
fn update_rejects_a_domain_topology_change_as_a_revision_conflict() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();

    let mut flipped = ResourceControls::from_host(&host());
    flipped.domains.get_mut("system").unwrap().memory = DomainMemory::Unified;
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "flip",
            &flipped,
            &observations(),
            11_000,
        ),
        Err(ResourcePolicyError::RevisionConflict)
    ));
    // The rejected update must not have advanced the ledger epoch or revision:
    // uncertainty must retain accounting rather than silently applying part of a
    // rejected write.
    assert_eq!(store.resource_snapshot().unwrap().epoch, 1);
}

#[test]
fn update_leaving_a_domain_topology_unchanged_still_succeeds() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();

    let mut unchanged = ResourceControls::from_host(&host());
    unchanged.max_parked = 1;
    let updated = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "leave-alone",
            &unchanged,
            &observations(),
            11_000,
        )
        .unwrap();
    assert_eq!(updated.revision, 2);
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
    store.conn.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('owner-2','owner-2','model',NULL,'stopped',1,0,1,1)", []).unwrap();
    let second = serde_json::json!({"version":1,"phase":"parked","allocations":[["system",10,5]],"devices":[]}).to_string();
    store
        .conn
        .execute(
            "INSERT INTO resource_owners(owner_id,footprint_json) VALUES('owner-2',?1)",
            [second],
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
            managed_bytes: 30,
            host_kv_bytes: 15,
            parked_bytes: 40
        }
    );
    assert_eq!(result.overcommit.parked_owners, 2);
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
fn generic_reader_rejects_oversized_operation_fields() {
    for column in ["kind", "error_code", "accepted_at", "updated_at"] {
        let store = crate::Store::open_in_memory().unwrap();
        let session = store.begin_coordinator_session().unwrap();
        store
            .import_resource_policy(&session, &host(), &observations(), 11_000)
            .unwrap();
        let result = store
            .update_resource_policy(
                &session,
                "alice",
                "host-a",
                1,
                "key",
                &ResourceControls::from_host(&host()),
                &observations(),
                11_000,
            )
            .unwrap();
        // Corrupt an otherwise real accepted operation. Limits count bytes,
        // not SQLite text characters, and must run before String allocation.
        store
            .conn
            .execute(
                &format!("UPDATE operations SET {column}=?1 WHERE id=?2"),
                params!["é".repeat(8_193), result.operation_id],
            )
            .unwrap();
        assert!(
            matches!(
                store.get_management_operation(&result.operation_id),
                Err(ResourcePolicyError::CorruptStoredPolicy)
            ),
            "oversized {column} must fail closed"
        );
    }
}

#[test]
fn generic_reader_rejects_oversized_receipt_scope_before_returning_target() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let result = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "key",
            &ResourceControls::from_host(&host()),
            &observations(),
            11_000,
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE command_receipts SET command_scope=?1 WHERE operation_id=?2",
            params!["é".repeat(8_193), result.operation_id],
        )
        .unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
}

#[test]
fn generic_reader_receipt_boundary_and_ambiguity_leave_no_writes_or_open_transaction() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let result = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "key",
            &ResourceControls::from_host(&host()),
            &observations(),
            11_000,
        )
        .unwrap();
    let mut receipt: String = store
        .conn
        .query_row(
            "SELECT response_json FROM command_receipts WHERE operation_id=?1",
            [&result.operation_id],
            |r| r.get(0),
        )
        .unwrap();
    receipt.extend(std::iter::repeat_n(' ', MAX_JSON_BYTES - receipt.len()));
    store
        .conn
        .execute("UPDATE command_receipts SET response_json=?1", [&receipt])
        .unwrap();
    let before = store.snapshot().unwrap();
    assert!(store
        .get_management_operation(&result.operation_id)
        .unwrap()
        .is_some());
    assert_eq!(store.snapshot().unwrap(), before);

    receipt.push(' ');
    store
        .conn
        .execute("UPDATE command_receipts SET response_json=?1", [&receipt])
        .unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
    receipt.pop();
    store
        .conn
        .execute("UPDATE command_receipts SET response_json=?1", [&receipt])
        .unwrap();
    // Ambiguous provenance must reject, not choose whichever receipt SQLite
    // visits first. This is corruption injection, never acceptance authority.
    store.conn.execute("INSERT INTO command_receipts SELECT 'other',command_scope,idempotency_key,request_hash,operation_id,response_json FROM command_receipts", []).unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
    assert!(store.get_management_operation("missing").unwrap().is_none());
    assert!(store.conn.is_autocommit());
    assert_eq!(store.snapshot().unwrap(), before);
}

#[test]
fn failed_receipt_write_rolls_back_policy_epoch_operation_and_event() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let before_events: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM management_events", [], |r| r.get(0))
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
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM command_receipts", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM management_events", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        before_events
    );
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

#[test]
fn bootstrap_rejects_host_rename_and_stale_session() {
    let store = crate::Store::open_in_memory().unwrap();
    let stale = store.begin_coordinator_session().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.import_resource_policy(&stale, &host(), &observations(), 11_000),
        Err(ResourcePolicyError::StaleSession)
    ));
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let mut renamed = host();
    renamed.name = "host-b".into();
    assert!(matches!(
        store.import_resource_policy(&session, &renamed, &observations(), 11_000),
        Err(ResourcePolicyError::RevisionConflict)
    ));
    let rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM host_resource_policies", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 1);
    let mut changed_context = host();
    changed_context.endpoint_port_range.end = 20_101;
    assert!(matches!(
        store.import_resource_policy(&session, &changed_context, &observations(), 11_000),
        Err(ResourcePolicyError::RevisionConflict)
    ));
}

#[test]
fn corrupt_policy_and_receipt_metadata_fail_closed() {
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
    store
        .conn
        .execute("UPDATE command_receipts SET command_scope='PUT /wrong'", [])
        .unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
    store.conn.execute("UPDATE command_receipts SET command_scope='PUT /management/v1/hosts/\"host-a\"/resource-policy',response_json=json_set(response_json,'$.epoch',0)", []).unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
    store.conn.execute("UPDATE command_receipts SET response_json=json_set(response_json,'$.epoch',2,'$.overcommit.domains.system.managed_bytes',-1)", []).unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
    store.conn.execute("UPDATE command_receipts SET response_json=json_set(response_json,'$.overcommit.domains.system.managed_bytes',0,'$.overcommit.domains.system.host_kv_bytes',0,'$.overcommit.domains.system.parked_bytes',0,'$.overcommit.domains.system.extra',1)", []).unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
    store.conn.execute("UPDATE host_resource_policies SET policy_json=json_set(policy_json,'$.controls.queue.extra',1)", []).unwrap();
    assert!(matches!(
        store.resource_policy("host-a"),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
}

#[test]
fn receipt_epoch_above_sqlite_range_is_corrupt() {
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
    let too_large = (i64::MAX as u64) + 1;
    let json: String = store
        .conn
        .query_row("SELECT response_json FROM command_receipts", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    value["epoch"] = serde_json::Value::Number(too_large.into());
    store
        .conn
        .execute(
            "UPDATE command_receipts SET response_json=?1",
            [value.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.get_management_operation(&result.operation_id),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
}

#[test]
fn stale_session_rejects_update_and_persisted_noop_import() {
    let store = crate::Store::open_in_memory().unwrap();
    let stale = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&stale, &host(), &observations(), 11_000)
        .unwrap();
    let current = store.begin_coordinator_session().unwrap();
    let controls = ResourceControls::from_host(&host());
    assert!(matches!(
        store.import_resource_policy(&stale, &host(), &observations(), 11_000),
        Err(ResourcePolicyError::StaleSession)
    ));
    assert!(matches!(
        store.update_resource_policy(
            &stale,
            "alice",
            "host-a",
            1,
            "key",
            &controls,
            &observations(),
            11_000
        ),
        Err(ResourcePolicyError::StaleSession)
    ));
    assert!(
        !store
            .import_resource_policy(&current, &host(), &observations(), 11_000)
            .unwrap()
            .changed
    );
}

#[test]
fn update_rejects_observation_stale_under_current_ttl_even_if_replacement_is_larger() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let mut controls = ResourceControls::from_host(&host());
    controls.observation_ttl_ms = 10_000;
    let stale = vec![MemoryObservation {
        sampled_at_ms: 8_999,
        ..observations()[0].clone()
    }];
    assert!(matches!(
        store.update_resource_policy(
            &session, "alice", "host-a", 1, "key", &controls, &stale, 11_000
        ),
        Err(ResourcePolicyError::Invalid)
    ));
    assert_eq!(
        store.resource_policy("host-a").unwrap().unwrap().revision,
        1
    );
}

#[test]
fn long_config_valid_domain_roundtrips_through_retry_and_operation_read() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let domain = "d".repeat(300);
    let mut long_host = host();
    let policy = long_host.domains.remove("system").unwrap();
    long_host.domains.insert(domain.clone(), policy);
    long_host.devices.get_mut("gpu0").unwrap().domain = domain.clone();
    let observed = vec![MemoryObservation {
        domain: domain.clone(),
        ..observations()[0].clone()
    }];
    store
        .import_resource_policy(&session, &long_host, &observed, 11_000)
        .unwrap();
    let controls = ResourceControls::from_host(&long_host);
    let first = store
        .update_resource_policy(
            &session, "alice", "host-a", 1, "key", &controls, &observed, 11_000,
        )
        .unwrap();
    assert_eq!(
        store
            .update_resource_policy(
                &session, "alice", "host-a", 1, "key", &controls, &observed, 11_000
            )
            .unwrap(),
        first
    );
    assert!(matches!(
        store
            .get_management_operation(&first.operation_id)
            .unwrap()
            .unwrap()
            .target,
        ManagementOperationTarget::HostResourcePolicy { revision: 2, .. }
    ));
}

#[test]
fn host_operation_target_shape_must_match_kind_and_receipt_scope() {
    let store = crate::Store::open_in_memory().unwrap();
    store.conn.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('owner','owner','model',NULL,'stopped',1,0,1,1)", []).unwrap();
    store.conn.execute("INSERT INTO operations(id,deployment_id,kind,state,error_code,idempotency_key) VALUES('bad','owner','host_resource_policy_update','succeeded',NULL,NULL)", []).unwrap();
    assert!(matches!(
        store.get_management_operation("bad"),
        Err(ResourcePolicyError::CorruptStoredPolicy)
    ));
}

#[test]
fn current_ttl_applies_before_new_ttl_and_historical_retry_survives_later_update() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    let mut first_controls = ResourceControls::from_host(&host());
    first_controls.observation_ttl_ms = 1;
    let first = store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            1,
            "first",
            &first_controls,
            &observations(),
            11_000,
        )
        .unwrap();
    let mut second_controls = first_controls.clone();
    second_controls.max_parked = 1;
    let stale_observation = vec![MemoryObservation {
        sampled_at_ms: 10_999,
        ..observations()[0].clone()
    }];
    store
        .update_resource_policy(
            &session,
            "alice",
            "host-a",
            2,
            "second",
            &second_controls,
            &stale_observation,
            11_000,
        )
        .unwrap();
    assert_eq!(
        store
            .update_resource_policy(
                &session,
                "alice",
                "host-a",
                1,
                "first",
                &first_controls,
                &observations(),
                11_000
            )
            .unwrap(),
        first
    );
}

#[test]
fn revision_epoch_legacy_and_event_failures_roll_back() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &host(), &observations(), 11_000)
        .unwrap();
    store.conn.execute("UPDATE host_resource_policies SET revision=?1,policy_json=json_set(policy_json,'$.revision',?1)", [i64::MAX]).unwrap();
    let controls = ResourceControls::from_host(&host());
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "a",
            "host-a",
            i64::MAX,
            "overflow",
            &controls,
            &observations(),
            11_000
        ),
        Err(ResourcePolicyError::Invalid)
    ));
    store.conn.execute("UPDATE host_resource_policies SET revision=1,policy_json=json_set(policy_json,'$.revision',1)", []).unwrap();
    store
        .conn
        .execute("UPDATE resource_ledger_meta SET epoch=?1", [i64::MAX])
        .unwrap();
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "a",
            "host-a",
            1,
            "epoch",
            &controls,
            &observations(),
            11_000
        ),
        Err(ResourcePolicyError::Invalid)
    ));
    store
        .conn
        .execute("UPDATE resource_ledger_meta SET epoch=1", [])
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO owners(id,kind,deployment_id) VALUES('legacy','model',NULL)",
            [],
        )
        .unwrap();
    store.conn.execute("INSERT INTO reservations(owner_id,domain_id,bytes,phase,exclusive_devices) VALUES('legacy','system',1,'ready','[]')", []).unwrap();
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "a",
            "host-a",
            1,
            "legacy",
            &controls,
            &observations(),
            11_000
        ),
        Err(ResourcePolicyError::NeedsReconciliation)
    ));
    store.conn.execute("DELETE FROM reservations", []).unwrap();
    store.conn.execute_batch("CREATE TRIGGER fail_policy_event BEFORE INSERT ON management_events WHEN NEW.kind='host_resource_policy_updated' BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    let before_events: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM management_events", [], |r| r.get(0))
        .unwrap();
    assert!(matches!(
        store.update_resource_policy(
            &session,
            "a",
            "host-a",
            1,
            "event",
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
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM operations", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM command_receipts", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM management_events", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        before_events
    );
}
