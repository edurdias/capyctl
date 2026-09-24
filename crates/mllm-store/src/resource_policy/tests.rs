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
            stream_idle_ms: mllm_config::effective::DEFAULT_STREAM_IDLE_MS,
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
            "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('owner',?1,'owner')",
            [footprint],
        )
        .unwrap();
    store.conn.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('owner-2','owner-2','model',NULL,'stopped',1,0,1,1)", []).unwrap();
    let second = serde_json::json!({"version":1,"phase":"parked","allocations":[["system",10,5]],"devices":[]}).to_string();
    store
        .conn
        .execute(
            "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('owner-2',?1,'owner-2')",
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

// T24: two hosts may publish the same local domain/device labels without overlap.
#[test]
fn remote_policies_namespace_local_resources_and_do_not_break_embedded_lookup() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let embedded = store.import_resource_policy(&session,&host(),&observations(),11_000).unwrap();
    let embedded_id = store.embedded_host_id().unwrap().unwrap();
    assert_ne!(embedded_id, host().name);
    assert_eq!(store.host_resource_key(&embedded_id,"domain","system").unwrap(),Some("system".into()));
    let first = store.import_remote_resource_policy(&session,"enrolled-one",&host(),&observations(),11_000).unwrap();
    let second = store.import_remote_resource_policy(&session,"enrolled-two",&host(),&observations(),11_000).unwrap();
    assert_ne!(first.context.domain_ids,second.context.domain_ids);
    assert_ne!(first.context.device_domains,second.context.device_domains);
    assert_eq!(store.resource_policy(&embedded_id).unwrap().unwrap().context,embedded.context);
    assert_eq!(store.resource_policy("enrolled-one").unwrap().unwrap().context,first.context);
    assert_eq!(store.import_resource_policy(&session,&host(),&observations(),11_000).unwrap().context,embedded.context);
}

// T24: migration adds ownership metadata without changing accounting or replay bytes.
#[test]
fn namespace_upgrade_preserves_owned_reservations_grants_and_epoch() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store.import_resource_policy(&session,&host(),&observations(),11_000).unwrap();
    // Recreate the populated pre-namespace schema shape. Ownership bytes are
    // deliberately opaque here: the migration must not reinterpret their digest.
    store.conn.execute_batch("DROP TABLE host_publication_migrations; DROP TABLE engine_config_migrations; DROP TABLE remote_binding_ingress; DROP TABLE managed_configuration_sources; DROP TABLE approved_host_publications; DROP TABLE host_certificate_renewals; DROP TABLE host_enrollment_transactions; DROP TABLE host_certificates; DROP TABLE enrolled_hosts; DROP TABLE host_invitations; DROP TABLE host_resource_keys; DROP TABLE host_resource_namespaces; DELETE FROM schema_migrations WHERE version>=16;
        INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version,revision) VALUES('kept','kept','model','ready',0,0,8,1,3);
        INSERT INTO operations(id,deployment_id,kind,state) VALUES('op-kept','kept','initialize','running');
        INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding-kept','kept',3,'incarnation-kept','managed','original binding','original identities','live');
        INSERT INTO endpoint_leases(host_id,host,port,binding_id) VALUES('','127.0.0.1',30000,'binding-kept');
        INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('kept','{\"immutable\":true}','kept');
        INSERT INTO resource_grants VALUES('grant-kept','kept','op-kept','original request digest',23);
        UPDATE resource_ledger_meta SET epoch=23;").unwrap();
    crate::migrations::apply(&store.conn).unwrap();
    let host_id = store.embedded_host_id().unwrap().unwrap();
    crate::migrations::apply(&store.conn).unwrap();
    assert_eq!(store.embedded_host_id().unwrap(),Some(host_id.clone()));
    assert_eq!(store.host_resource_key(&host_id,"device","gpu0").unwrap(),Some("gpu0".into()));
    let state:(String,String,i64,i64) = store.conn.query_row("SELECT footprint_json,(SELECT request_json FROM resource_grants WHERE id='grant-kept'),(SELECT epoch FROM resource_ledger_meta),(SELECT current_generation FROM deployments WHERE id='kept') FROM resource_owners WHERE owner_id='kept'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(state,("{\"immutable\":true}".into(),"original request digest".into(),23,8));
    let binding:(String,String,String,i64) = store.conn.query_row("SELECT state,binding_json,identities_json,(SELECT count(*) FROM endpoint_leases WHERE binding_id='binding-kept') FROM runtime_bindings WHERE id='binding-kept'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(binding,("live".into(),"original binding".into(),"original identities".into(),1));
}

// T24: multiple legacy policies have no reliable ownership provenance.
#[test]
fn ambiguous_legacy_namespace_requires_reconciliation() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store.import_resource_policy(&session,&host(),&observations(),11_000).unwrap();
    store.conn.execute_batch("DROP TABLE host_publication_migrations; DROP TABLE engine_config_migrations; DROP TABLE remote_binding_ingress; DROP TABLE managed_configuration_sources; DROP TABLE approved_host_publications; DROP TABLE host_certificate_renewals; DROP TABLE host_enrollment_transactions; DROP TABLE host_certificates; DROP TABLE enrolled_hosts; DROP TABLE host_invitations; DROP TABLE host_resource_keys; DROP TABLE host_resource_namespaces; DELETE FROM schema_migrations WHERE version>=16;
        INSERT INTO host_resource_policies SELECT 'other-host',revision,policy_json FROM host_resource_policies;").unwrap();
    crate::migrations::apply(&store.conn).unwrap();
    assert!(matches!(store.resource_policy("host-a"),Err(ResourcePolicyError::NeedsReconciliation)));
    assert!(matches!(store.import_resource_policy(&session,&host(),&observations(),11_000),Err(ResourcePolicyError::NeedsReconciliation)));
    assert!(store.embedded_host_id().unwrap().is_none());
    assert!(matches!(store.import_remote_resource_policy(&session,"new-host",&host(),&observations(),11_000),Err(ResourcePolicyError::NeedsReconciliation)));
}

// T24: legacy local labels cannot alias a new host's generated accounting key.
#[test]
fn scoped_key_collision_rolls_back_namespace_and_policy() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let collision = crate::resource_namespace::ledger_key("remote","domain","system");
    let mut legacy = host();
    let domain = legacy.domains.remove("system").unwrap();
    legacy.domains.insert(collision.clone(),domain);
    legacy.devices.get_mut("gpu0").unwrap().domain = collision.clone();
    let mut sampled = observations(); sampled[0].domain = collision;
    let before = store.import_resource_policy(&session,&legacy,&sampled,11_000).unwrap();
    assert!(store.import_remote_resource_policy(&session,"remote",&host(),&observations(),11_000).is_err());
    assert!(store.resource_policy("remote").unwrap().is_none());
    assert!(store.host_resource_key("remote","domain","system").unwrap().is_none());
    assert_eq!(store.resource_snapshot().unwrap().epoch,before.epoch);
}

// T26 T27 (Phase B follow-up): a host's policy update is judged against that
// host's owners. Another host's parked charge on its own domain is neither
// corrupt stored policy nor overcommit here, and does not count against this
// host's `max_parked`; the host-scoped snapshot sets it aside the same way.
#[test]
fn a_policy_update_on_one_host_ignores_another_hosts_owners() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let one = store
        .import_remote_resource_policy(&session, "enrolled-one", &host(), &observations(), 11_000)
        .unwrap();
    store
        .import_remote_resource_policy(&session, "enrolled-two", &host(), &observations(), 11_000)
        .unwrap();
    let theirs = store
        .host_resource_key("enrolled-two", "domain", "system")
        .unwrap()
        .unwrap();
    let ours = store
        .host_resource_key("enrolled-one", "domain", "system")
        .unwrap()
        .unwrap();
    store.conn.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('elsewhere','elsewhere','model',NULL,'stopped',1,0,1,1)", []).unwrap();
    let parked = serde_json::json!({"version":1,"phase":"parked","allocations":[[theirs,60,30]],"devices":[]}).to_string();
    store
        .conn
        .execute(
            "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('elsewhere',?1,'elsewhere')",
            [parked],
        )
        .unwrap();
    let mut controls = one.controls.clone();
    controls.max_parked = 0;
    controls.domains.get_mut(&ours).unwrap().managed_limit = 40;
    let scoped_observations: Vec<_> = observations()
        .into_iter()
        .map(|mut o| {
            o.domain = ours.clone();
            o
        })
        .collect();
    let result = store
        .update_resource_policy(
            &session,
            "alice",
            "enrolled-one",
            one.revision,
            "scoped",
            &controls,
            &scoped_observations,
            11_000,
        )
        .unwrap();
    assert_eq!(result.overcommit.parked_owners, 0);
    assert_eq!(
        result.overcommit.domains[&ours],
        DomainOvercommit { managed_bytes: 0, host_kv_bytes: 0, parked_bytes: 0 }
    );
    // Set aside, never released.
    assert!(store.resource_snapshot().unwrap().owners.contains_key("elsewhere"));
    let scoped = store.host_scoped_resource_snapshot(&[ours.as_str()]).unwrap();
    assert!(scoped.owners.is_empty());
    let theirs_view = store.host_scoped_resource_snapshot(&[theirs.as_str()]).unwrap();
    assert!(theirs_view.owners.contains_key("elsewhere"));
    // An unregistered domain keeps the whole ledger (fails closed).
    let unknown = store.host_scoped_resource_snapshot(&["unregistered"]).unwrap();
    assert!(unknown.owners.contains_key("elsewhere"));
}

// T24 T26: found live 2026-09-23 (matrix M32). A host restarted with a changed
// resource policy in its document (normal to tight: max_parked 2 to 1) kept its
// first imported limits at the controller, silently, so `max_parked` was never
// enforced. A changed remote policy is now applied as a revision on the next
// publication.
#[test]
fn a_republished_remote_policy_with_changed_limits_is_applied() {
    let store = crate::Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let first = store
        .import_remote_resource_policy(&session, "enrolled-one", &host(), &observations(), 11_000)
        .unwrap();
    assert_eq!(first.controls.max_parked, 2);
    let mut tight = host();
    tight.max_parked = 1;
    let again = store
        .import_remote_resource_policy(&session, "enrolled-one", &tight, &observations(), 11_000)
        .unwrap();
    assert_eq!(again.controls.max_parked, 1);
    assert_eq!(again.revision, first.revision + 1);
    assert!(again.changed);
    let stored = store.resource_policy("enrolled-one").unwrap().unwrap();
    assert_eq!(stored.controls.max_parked, 1);
    // Publishing the same document again changes nothing.
    let same = store
        .import_remote_resource_policy(&session, "enrolled-one", &tight, &observations(), 11_000)
        .unwrap();
    assert_eq!(same.revision, again.revision);
    assert!(!same.changed);
    // Back to the first limits and to tight again: each change is a revision.
    let back = store
        .import_remote_resource_policy(&session, "enrolled-one", &host(), &observations(), 11_000)
        .unwrap();
    assert_eq!((back.controls.max_parked, back.revision), (2, again.revision + 1));
    let tight_again = store
        .import_remote_resource_policy(&session, "enrolled-one", &tight, &observations(), 11_000)
        .unwrap();
    assert_eq!((tight_again.controls.max_parked, tight_again.revision), (1, back.revision + 1));
}
