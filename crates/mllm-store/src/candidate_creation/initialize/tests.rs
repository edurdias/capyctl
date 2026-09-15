use super::*;
use crate::candidate_creation::tests::{command, fixture, setup};
use serde_json::{Value, json};
const BODY: &str = r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#;
#[path = "../../lifecycle/completion/tests.rs"]
mod completion_tests;
#[test]
fn initialize_preserves_reviewed_device_order_while_matching_canonical_ledger() {
    let (mut manifest, mut host, mut policy) = fixture("fake");
    manifest["effective_recipe"]["host_devices"]["gpu1"] =
        manifest["effective_recipe"]["host_devices"]["gpu0"].clone();
    host["resource_policy"]["devices"]["gpu1"] = host["resource_policy"]["devices"]["gpu0"].clone();
    policy
        .devices
        .insert("gpu1".into(), policy.devices["gpu0"].clone());
    let devices = json!([{"id":"gpu1","sharing":"shared"},{"id":"gpu0","sharing":"shared"}]);
    manifest["effective_recipe"]["devices"] = devices.clone();
    for phase in ["cold", "ready", "parking", "wake"] {
        manifest["effective_recipe"]["resources"][phase]["devices"] = devices.clone();
    }
    let reviewed =
        super::super::validate_candidate_reviewed_snapshot_text(&manifest.to_string()).unwrap();
    policy
        .qualification_policy
        .as_mut()
        .unwrap()
        .allowed_manifest_digests = vec![reviewed.manifest_digest().into()];
    let store = crate::Store::open_in_memory().unwrap();
    let s = setup(&store, &policy);
    let c = store
        .create_candidate_run(&s, "owner", "create", &command(&manifest), &host, 1000)
        .unwrap();
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    assert!(matches!(
        arm(&store, &s, r.step_id()),
        Ok(ArmResult::New { .. })
    ));
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    assert_eq!(context.completion_target.unwrap().devices.len(), 2);
    assert_eq!(
        store
            .candidate_run_snapshot("owner", c.run_id())
            .unwrap()
            .unwrap()
            .reviewed_manifest()
            .effective_recipe()
            .devices()[0]
            .id,
        "gpu1"
    );
}
fn arm(
    store: &crate::Store,
    session: &CoordinatorSession,
    step: &str,
) -> std::result::Result<ArmResult, LifecycleError> {
    use mllm_domain::resources::{MemoryLimit, MemoryObservation};
    let policy = store.resource_policy("lab").unwrap().unwrap();
    let limits: Vec<_> = policy
        .controls
        .domains
        .iter()
        .map(|(id, p)| MemoryLimit {
            domain: id.clone(),
            managed_bytes: p.managed_limit,
            free_reserve_bytes: p.free_reserve,
            host_kv_bytes: p.host_kv_limit,
            parked_bytes: p.parked_limit,
        })
        .collect();
    let observations: Vec<_> = limits
        .iter()
        .map(|l| MemoryObservation {
            domain: l.domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1200,
        })
        .collect();
    store.arm_step(
        session,
        step,
        mllm_scheduler::residency::AdmissionContext::new(
            &observations,
            &limits,
            1200,
            policy.controls.observation_ttl_ms,
            policy.controls.max_parked as usize,
        ),
    )
}
#[test]
fn initialize_arm_records_one_conservative_grant_and_exact_context() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    let epoch = store.resource_snapshot().unwrap().epoch;
    assert_eq!(
        arm(&store, &s, r.step_id()).unwrap(),
        ArmResult::New {
            step_id: r.step_id().into()
        }
    );
    let execution = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    assert_eq!(
        execution.token,
        mllm_domain::completion::TransitionToken {
            deployment_id: c.deployment_id().into(),
            revision: 1,
            generation: 1,
            operation_id: r.operation_id().into(),
            step_id: r.step_id().into(),
            qualification_id: format!("candidate:{}", c.run_id())
        }
    );
    assert_eq!(execution.binding_id, c.binding_id());
    assert_eq!(execution.incarnation, c.incarnation());
    assert_eq!(
        (execution.issued_at_ms, execution.deadline_ms),
        (1200, 400000)
    );
    assert_eq!(
        execution.identities,
        mllm_domain::completion::ExecutionIdentities::OwnedLaunch
    );
    assert_eq!(
        execution.launch_settings,
        Some(mllm_domain::launch::ProfileLaunchSettings::Fake(
            mllm_domain::launch::FakeLaunchSettings
        ))
    );
    let ready = execution.completion_target.unwrap();
    assert_eq!(ready.phase, mllm_domain::resources::ResourcePhase::Ready);
    assert_eq!(ready.allocations[0].bytes, 8_589_934_592);
    assert!(execution.grant_id.is_some());
    let ledger = store.resource_snapshot().unwrap();
    assert_eq!(ledger.epoch, epoch + 1);
    assert_eq!(
        ledger.owners[c.deployment_id()].allocations[0].bytes,
        10_737_418_240
    );
    let before = durable(&store);
    assert_eq!(
        arm(&store, &s, r.step_id()).unwrap(),
        ArmResult::AlreadyRecorded
    );
    assert_eq!(durable(&store), before);
    assert_eq!(
        store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 999999)
            .unwrap(),
        r
    );
    assert!(matches!(
        store.candidate_initialize_plan(&s, "owner", c.run_id(), r.step_id()),
        Err(Error::LifecycleConflict)
    ));
}
#[test]
fn initialize_arm_rolls_back_after_grant_and_denies_native() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    store.conn.execute_batch("CREATE TRIGGER fail BEFORE UPDATE ON lifecycle_steps BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = durable(&store);
    assert!(arm(&store, &s, r.step_id()).is_err());
    assert_eq!(durable(&store), before);
    for engine in ["vllm", "sglang"] {
        let (store, s, c) = created(engine);
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        let before = durable(&store);
        assert!(matches!(
            arm(&store, &s, r.step_id()),
            Err(LifecycleError::Unsupported)
        ));
        assert_eq!(durable(&store), before);
    }
}
#[test]
fn native_descriptor_revalidates_clock_boundaries_and_current_policy() {
    let (store, s, c) = created("sglang-pinned");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let before = durable(&store);
    for now in [1200, 399999] {
        assert!(store.candidate_native_launch(&s, r.step_id(), now).is_ok());
    }
    for now in [i64::MIN, 1100, 1199, 400000, i64::MAX] {
        assert!(store.candidate_native_launch(&s, r.step_id(), now).is_err());
        assert_eq!(durable(&store), before);
    }
    store.conn.execute("UPDATE host_qualification_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)", []).unwrap();
    let changed = durable(&store);
    assert!(
        store
            .candidate_native_launch(&s, r.step_id(), 1200)
            .is_err()
    );
    assert_eq!(durable(&store), changed);
    assert_eq!(
        arm(&store, &s, r.step_id()).unwrap(),
        ArmResult::AlreadyRecorded
    );
    assert_eq!(count(&store, "resource_grants"), 1);
}

#[test]
fn native_arm_freezes_once_and_keeps_ordinary_dispatch_closed() {
    let (store, s, c) = created("sglang-pinned");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    assert!(
        store
            .candidate_native_launch(&s, r.step_id(), 1200)
            .is_err()
    );
    assert_eq!(
        arm(&store, &s, r.step_id()).unwrap(),
        ArmResult::New {
            step_id: r.step_id().into()
        }
    );
    let frozen = store
        .candidate_native_launch(&s, r.step_id(), 1200)
        .unwrap();
    assert_eq!(frozen.metadata().binding_id, c.binding_id());
    assert_eq!(frozen.metadata().incarnation, c.incarnation());
    assert_eq!(frozen.metadata().device.host_id, "lab");
    assert_eq!(frozen.metadata().device.hardware_fingerprint, "hw-01");
    assert_eq!(frozen.metadata().device.device_id, "gpu0");
    let reviewed = store
        .candidate_run_snapshot("owner", c.run_id())
        .unwrap()
        .unwrap();
    assert_eq!(
        frozen.metadata().device.memory_domain,
        reviewed
            .reviewed_manifest()
            .effective_recipe()
            .host_devices()["gpu0"]
            .domain
    );
    let leased_port: u16 = store
        .conn
        .query_row(
            "SELECT port FROM endpoint_leases WHERE binding_id=?1",
            [c.binding_id()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        frozen.metadata().endpoint,
        format!("http://127.0.0.1:{leased_port}")
    );
    let public = format!("{:?}", frozen.metadata());
    for private in [
        "/srv/models",
        "/bin/true",
        "secret://",
        "engine-key",
        "admin-key",
    ] {
        assert!(!public.contains(private));
    }
    assert_eq!(
        frozen.checkpoint_root(),
        "/srv/models/Qwen3-4B-Instruct-2507"
    );
    let before = durable(&store);
    assert_eq!(
        arm(&store, &s, r.step_id()).unwrap(),
        ArmResult::AlreadyRecorded
    );
    assert_eq!(durable(&store), before);
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    assert!(matches!(
        context.launch_settings,
        Some(mllm_domain::launch::ProfileLaunchSettings::Sglang(_))
    ));
    assert_eq!(count(&store, "resource_grants"), 1);
    assert_eq!(count(&store, "deployment_routes"), 0);
    let gates: (bool, bool) = store
        .conn
        .query_row(
            "SELECT admission_enabled,dispatch_enabled FROM deployments",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(gates, (false, false));
    assert_eq!(count(&store, "qualifications"), 0);
    let json: String = store
        .conn
        .query_row("SELECT step_json FROM lifecycle_steps", [], |r| r.get(0))
        .unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["execution"]["launch_settings"]["engine"], "sglang");
    assert_eq!(value["execution"]["launch_settings"]["version"], 1);
}

#[test]
fn native_arm_failures_preserve_planned_intent_and_endpoint() {
    for mutation in [
        "UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('false'))",
        "UPDATE lifecycle_claims SET generation=2",
        "DELETE FROM lifecycle_claims",
        "UPDATE runtime_bindings SET state='uncertain'",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.descriptor.manifest_digest','mutated')",
        "CREATE TRIGGER fail BEFORE INSERT ON resource_grants BEGIN SELECT RAISE(ABORT,'injected'); END;",
        "CREATE TRIGGER fail BEFORE UPDATE ON lifecycle_steps BEGIN SELECT RAISE(ABORT,'injected'); END;",
    ] {
        let (store, s, c) = created("sglang-pinned");
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        store.conn.execute_batch(mutation).unwrap();
        let before = durable(&store);
        assert!(arm(&store, &s, r.step_id()).is_err(), "{mutation}");
        assert_eq!(durable(&store), before, "{mutation}");
        assert_eq!(count(&store, "resource_grants"), 0);
        assert_eq!(count(&store, "endpoint_leases"), 1);
        assert!(
            store
                .candidate_initialize_execution(&s, r.step_id())
                .is_err()
        );
        assert!(
            store
                .candidate_native_launch(&s, r.step_id(), 1200)
                .is_err()
        );
    }
    let (store, s, c) = created("sglang-pinned");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    store.begin_coordinator_session().unwrap();
    let before = durable(&store);
    assert!(matches!(
        arm(&store, &s, r.step_id()),
        Err(LifecycleError::Stale)
    ));
    assert_eq!(durable(&store), before);
}

#[test]
fn native_descriptor_mutation_invalidates_execution_and_replay() {
    for (field, value) in [
        ("checkpoint_root", json!("/another/root")),
        ("checkpoint_revision", json!("main")),
        ("executable", json!("/bin/other")),
        ("binding_id", json!("other")),
        ("incarnation", json!("other")),
        ("inference_credential_ref", json!("other")),
        ("admin_credential_ref", json!("other")),
        ("rendered_settings_digest", json!("0".repeat(64))),
        ("version", json!(2)),
        ("extra", json!(true)),
    ] {
        let (store, s, c) = created("sglang-pinned");
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        arm(&store, &s, r.step_id()).unwrap();
        store
            .conn
            .execute(
                "UPDATE lifecycle_steps SET step_json=json_set(step_json,?1,json(?2))",
                params![
                    format!("$.execution.launch_settings.{field}"),
                    value.to_string()
                ],
            )
            .unwrap();
        let before = durable(&store);
        assert!(
            matches!(
                store.candidate_initialize_execution(&s, r.step_id()),
                Err(LifecycleError::CorruptStoredData)
            ),
            "{field}"
        );
        assert!(arm(&store, &s, r.step_id()).is_err(), "{field}");
        assert!(
            store
                .candidate_native_launch(&s, r.step_id(), 1200)
                .is_err(),
            "{field}"
        );
        assert_eq!(durable(&store), before);
    }
}

#[test]
fn native_descriptor_duplicate_fields_cannot_reconstruct_a_launch() {
    let (store, s, c) = created("sglang-pinned");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let original: String = store
        .conn
        .query_row("SELECT step_json FROM lifecycle_steps", [], |r| r.get(0))
        .unwrap();
    let malformed = original.replace(
        "\"checkpoint_root\":",
        "\"checkpoint_root\":\"/unexpected\",\"checkpoint_root\":",
    );
    assert_ne!(malformed, original);
    store
        .conn
        .execute("UPDATE lifecycle_steps SET step_json=?1", [malformed])
        .unwrap();
    let before = durable(&store);
    assert!(matches!(
        store.candidate_native_launch(&s, r.step_id(), 1200),
        Err(LifecycleError::CorruptStoredData)
    ));
    assert!(arm(&store, &s, r.step_id()).is_err());
    assert_eq!(durable(&store), before);
}
fn created(
    engine: &str,
) -> (
    crate::Store,
    CoordinatorSession,
    super::super::CandidateCreationReceipt,
) {
    let store = crate::Store::open_in_memory().unwrap();
    let (manifest, host, policy) = if engine == "sglang-pinned" {
        pinned_fixture()
    } else {
        fixture(engine)
    };
    let session = setup(&store, &policy);
    let receipt = store
        .create_candidate_run(
            &session,
            "owner",
            "create",
            &command(&manifest),
            &host,
            1000,
        )
        .unwrap();
    (store, session, receipt)
}
fn pinned_fixture() -> (Value, Value, mllm_config::effective::HostPolicy) {
    let (mut manifest, mut host, mut policy) = fixture("sglang");
    manifest["effective_recipe"]["model"]["path"] = json!("/srv/models/Qwen3-4B-Instruct-2507");
    manifest["effective_recipe"]["model"]["revision"] =
        json!("cdbee75f17c01a7cc42f958dc650907174af0554");
    manifest["effective_recipe"]["resolved_profile"]["build_fingerprint"] =
        json!("fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1");
    host["runtime_profiles"]["local"]["build_fingerprint"] =
        json!("fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1");
    let reviewed =
        super::super::validate_candidate_reviewed_snapshot_text(&manifest.to_string()).unwrap();
    policy
        .qualification_policy
        .as_mut()
        .unwrap()
        .allowed_manifest_digests = vec![reviewed.manifest_digest().into()];
    (manifest, host, policy)
}
fn count(store: &crate::Store, table: &str) -> i64 {
    store
        .conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn durable(store: &crate::Store) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    [
        "operations",
        "lifecycle_runs",
        "lifecycle_claims",
        "lifecycle_steps",
        "qualification_case_actions",
        "owned_launch_associations",
        "candidate_cleanup_actions",
        "lifecycle_evidence",
        "request_leases",
        "generation_history",
        "command_receipts",
        "management_events",
        "qualification_runs",
        "runtime_bindings",
        "endpoint_leases",
        "resource_owners",
        "resource_grants",
        "resource_ledger_meta",
        "deployments",
        "effective_revisions",
        "deployment_routes",
        "event_meta",
    ]
    .iter()
    .map(|table| {
        let mut stmt = store
            .conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let n = stmt.column_count();
        stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    })
    .collect()
}
#[test]
fn initialize_replay_is_historical_and_case_is_permanent() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    let before = durable(&store);
    assert_eq!(
        store
            .accept_candidate_initialize(
                &s,
                "owner",
                c.run_id(),
                "a",
                r#"{"deadline_ms":400000,"action":"initialize","expected_revision":1}"#,
                999999
            )
            .unwrap(),
        r
    );
    assert_eq!(durable(&store), before);
    assert!(matches!(
        store.accept_candidate_initialize(
            &s,
            "owner",
            c.run_id(),
            "a",
            &BODY.replace("400000", "390000"),
            1100
        ),
        Err(Error::IdempotencyConflict)
    ));
    for state in ["failed", "uncertain"] {
        store
            .conn
            .execute(
                "UPDATE lifecycle_runs SET state=?1 WHERE operation_id=?2",
                params![state, r.operation_id()],
            )
            .unwrap();
        store
            .conn
            .execute("UPDATE lifecycle_steps SET state='cancelled'", [])
            .unwrap();
        assert!(matches!(
            store.accept_candidate_initialize(&s, "owner", c.run_id(), "b", BODY, 1100),
            Err(Error::LifecycleConflict)
        ));
        assert_eq!(
            store
                .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 999999)
                .unwrap(),
            r
        );
    }
    assert_eq!(count(&store, "qualification_case_actions"), 1);
}
#[test]
fn initialize_input_owner_and_fences_fail_closed() {
    let (store, s, c) = created("fake");
    for body in [
        "{}",
        r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000,"extra":1}"#,
        r#"{"expected_revision":1,"expected_revision":1,"action":"initialize","deadline_ms":400000}"#,
        r#"{"expected_revision":1,"action":"stop","deadline_ms":400000}"#,
    ] {
        assert!(matches!(
            store.accept_candidate_initialize(&s, "owner", c.run_id(), "a", body, 1100),
            Err(Error::InvalidCommand)
        ));
    }
    assert!(matches!(
        store.accept_candidate_initialize(&s, "other", c.run_id(), "a", BODY, 1100),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        store.accept_candidate_initialize(
            &s,
            "owner",
            c.run_id(),
            "a",
            &BODY.replace("revision\":1", "revision\":2"),
            1100
        ),
        Err(Error::RevisionConflict)
    ));
    for deadline in [1100_i64, 500001, i64::MAX] {
        assert!(matches!(
            store.accept_candidate_initialize(
                &s,
                "owner",
                c.run_id(),
                "a",
                &BODY.replace("400000", &deadline.to_string()),
                1100
            ),
            Err(Error::QualificationDenied)
        ));
    }
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    assert!(
        store
            .candidate_initialize_plan(&s, "other", c.run_id(), r.step_id())
            .unwrap()
            .is_none()
    );
    store
        .conn
        .execute("UPDATE deployments SET current_generation=2", [])
        .unwrap();
    assert!(matches!(
        store.candidate_initialize_plan(&s, "owner", c.run_id(), r.step_id()),
        Err(Error::LifecycleConflict)
    ));
    let new = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.candidate_initialize_plan(&s, "owner", c.run_id(), r.step_id()),
        Err(Error::StaleSession)
    ));
    assert!(matches!(
        store.candidate_initialize_plan(&new, "owner", c.run_id(), r.step_id()),
        Err(Error::StaleSession)
    ));
    assert_eq!(
        store
            .accept_candidate_initialize(&new, "owner", c.run_id(), "a", BODY, 999999)
            .unwrap(),
        r
    );
}
#[test]
fn initialize_native_descriptors_are_informational_and_ordinary_admission_stays_closed() {
    for engine in ["fake", "vllm", "sglang"] {
        let (store, s, c) = created(engine);
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        let plan = store
            .candidate_initialize_plan(&s, "owner", c.run_id(), r.step_id())
            .unwrap()
            .unwrap();
        let settings = plan
            .reviewed_manifest()
            .effective_recipe()
            .profile()
            .launch_settings();
        assert_eq!(
            serde_json::to_value(launch_settings(settings)).unwrap(),
            serde_json::to_value(settings).unwrap()
        );
        let fence = DeploymentFence {
            deployment_id: c.deployment_id().into(),
            revision: 1,
            generation: 1,
        };
        assert!(matches!(
            store.accept_start(&s, &fence, 400000),
            Err(LifecycleError::Disabled)
        ));
        assert!(matches!(
            store.accept_activation(&s, &fence, 400000),
            Err(LifecycleError::Disabled)
        ));
        assert_eq!(
            plan.reviewed_manifest().reviewed_json(),
            store
                .candidate_run_snapshot("owner", c.run_id())
                .unwrap()
                .unwrap()
                .reviewed_manifest()
                .reviewed_json()
        );
        assert_eq!(count(&store, "resource_grants"), 0);
        assert_eq!(count(&store, "qualifications"), 0);
        assert_eq!(count(&store, "deployment_routes"), 0);
    }
}
#[test]
fn initialize_acceptance_rolls_back_late_failures() {
    for trigger in [
        "CREATE TRIGGER fail BEFORE INSERT ON qualification_case_actions BEGIN SELECT RAISE(ABORT,'injected'); END;",
        "CREATE TRIGGER fail BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'injected'); END;",
    ] {
        let (store, s, c) = created("fake");
        store.conn.execute_batch(trigger).unwrap();
        let before = durable(&store);
        assert!(matches!(
            store.accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100),
            Err(Error::Sql(_))
        ));
        assert_eq!(durable(&store), before);
    }
}
#[test]
fn initialize_step_and_receipt_corruption_is_rejected() {
    for field in [
        "version",
        "kind",
        "principal_id",
        "run_id",
        "creation_operation_id",
        "operation_id",
        "step_id",
        "deployment_id",
        "revision",
        "generation",
        "binding_id",
        "incarnation",
        "qualification_id",
        "case_id",
        "case_kind",
        "cycle",
        "descriptor",
        "accepted_at_ms",
        "deadline_ms",
        "identities",
        "unknown",
    ] {
        let (store, s, c) = created("fake");
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        let text: String = store
            .conn
            .query_row("SELECT step_json FROM lifecycle_steps", [], |r| r.get(0))
            .unwrap();
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value[field] = json!("corrupt");
        store
            .conn
            .execute(
                "UPDATE lifecycle_steps SET step_json=?1",
                [value.to_string()],
            )
            .unwrap();
        assert!(
            matches!(
                store.accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100),
                Err(Error::CorruptStoredData)
            ),
            "{field}"
        );
        assert!(
            matches!(
                store.candidate_initialize_plan(&s, "owner", c.run_id(), r.step_id()),
                Err(Error::CorruptStoredData)
            ),
            "{field}"
        );
    }
}
#[test]
fn initialize_policy_revocation_blocks_fresh_but_not_replay() {
    let (store, s, c) = created("fake");
    let before = durable(&store);
    store.conn.execute("UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('false'))",[]).unwrap();
    assert!(matches!(
        store.accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100),
        Err(Error::QualificationDenied)
    ));
    assert_eq!(durable(&store), before);
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    store.conn.execute("UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('false'))",[]).unwrap();
    assert_eq!(
        store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 999999)
            .unwrap(),
        r
    );
}

#[test]
fn initialize_owned_missing_association_and_receipt_scope_are_corruption() {
    for mutation in [
        "DELETE FROM qualification_case_actions",
        "UPDATE command_receipts SET command_scope='wrong' WHERE operation_id IN (SELECT operation_id FROM lifecycle_steps)",
    ] {
        let (store, s, c) = created("fake");
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        store.conn.execute_batch(mutation).unwrap();
        assert!(
            matches!(
                store.candidate_initialize_plan(&s, "owner", c.run_id(), r.step_id()),
                Err(Error::CorruptStoredData)
            ),
            "{mutation}"
        );
    }
}
#[test]
fn initialize_strict_envelopes_reject_duplicates_and_oversized_data() {
    for armed in [false, true] {
        for mutation in 0..3 {
            let (store, s, c) = created("fake");
            let r = store
                .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
                .unwrap();
            if armed {
                arm(&store, &s, r.step_id()).unwrap();
            }
            let text: String = store
                .conn
                .query_row("SELECT step_json FROM lifecycle_steps", [], |r| r.get(0))
                .unwrap();
            let corrupt = match mutation {
                0 => text.replacen('{', "{\"version\":1,", 1),
                1 => " ".repeat(MAX_BYTES + 1),
                _ => text.replacen('{', "{\"unknown\":null,", 1),
            };
            store
                .conn
                .execute("UPDATE lifecycle_steps SET step_json=?1", [corrupt])
                .unwrap();
            assert!(matches!(
                store.accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100),
                Err(Error::CorruptStoredData)
            ));
        }
    }
}
#[test]
fn initialize_execution_rejects_wrong_running_operation() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    store
        .conn
        .execute(
            "UPDATE operations SET state='pending' WHERE id=?1",
            [r.operation_id()],
        )
        .unwrap();
    assert!(
        store
            .candidate_initialize_execution(&s, r.step_id())
            .is_err()
    );
}

#[test]
fn initialize_recorded_uncertainty_is_never_a_new_attempt() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    store.conn.execute_batch("UPDATE lifecycle_steps SET state='uncertain'; UPDATE lifecycle_runs SET state='uncertain'; UPDATE qualification_runs SET state='uncertain'; UPDATE operations SET state='failed' WHERE kind='candidate_initialize'; UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('false'));").unwrap();
    let before = durable(&store);
    assert_eq!(
        arm(&store, &s, r.step_id()).unwrap(),
        ArmResult::AlreadyRecorded
    );
    assert!(
        store
            .candidate_initialize_execution(&s, r.step_id())
            .is_err()
    );
    assert_eq!(durable(&store), before);
}
#[test]
fn initialize_grant_context_and_identity_corruption_fails_closed() {
    for mutation in [
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution.issued_at_ms',0)",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution.grant_id','wrong')",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution.completion_target.allocations[0][1]',1)",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution.launch_settings.extra',true)",
        "UPDATE resource_grants SET request_json='[]'",
        "UPDATE runtime_bindings SET identities_json='{}'",
        "DELETE FROM qualification_case_actions",
    ] {
        let (store, s, c) = created("fake");
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        arm(&store, &s, r.step_id()).unwrap();
        store.conn.execute_batch(mutation).unwrap();
        let before = durable(&store);
        assert!(
            matches!(
                store.candidate_initialize_execution(&s, r.step_id()),
                Err(LifecycleError::CorruptStoredData)
            ),
            "{mutation}"
        );
        assert!(arm(&store, &s, r.step_id()).is_err(), "{mutation}");
        assert_eq!(durable(&store), before);
    }
}
#[test]
fn initialize_existing_state_and_competing_run_block_arm() {
    for mutation in [
        "UPDATE runtime_bindings SET state='uncertain'",
        "UPDATE deployments SET admission_enabled=1",
        "UPDATE deployments SET dispatch_enabled=1",
        "UPDATE deployments SET suspended=1",
        "DELETE FROM lifecycle_claims",
        "UPDATE lifecycle_claims SET generation=2",
        "UPDATE lifecycle_steps SET ordinal=1",
        "UPDATE lifecycle_runs SET plan_json=json_set(plan_json,'$.steps[0].action','stop')",
        "INSERT INTO operations(id,deployment_id,kind,state) SELECT 'competing',deployment_id,'park','pending' FROM lifecycle_steps; INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) SELECT 'competing',deployment_id,revision,generation,session_id,'park','queued',deadline_ms,plan_json FROM lifecycle_runs WHERE operation_id!='competing'",
    ] {
        let (store, s, c) = created("fake");
        let r = store
            .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
            .unwrap();
        store.conn.execute_batch(mutation).unwrap();
        let before = durable(&store);
        assert!(arm(&store, &s, r.step_id()).is_err(), "{mutation}");
        assert_eq!(durable(&store), before);
    }
}
#[test]
fn initialize_accepted_events_expose_only_generated_metadata() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let rows:Vec<String>=store.conn.prepare("SELECT payload_json FROM management_events WHERE kind IN ('candidate_initialize_accepted','candidate_initialize_armed')").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<std::result::Result<_,_>>().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let value: Value = serde_json::from_str(&row).unwrap();
        let mut keys: Vec<_> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "deployment_id",
                "generation",
                "operation_id",
                "revision",
                "run_id",
                "session_epoch",
                "step_id",
                "version"
            ]
        );
        assert!(!row.contains("cold_initialize"));
        assert!(!row.contains("owner"));
    }
}
#[test]
fn initialize_current_policy_all_frozen_maxima_gate_acceptance_and_arm() {
    let mutations = [
        ("$.state.policy.max_run_duration_ms", json!(599999)),
        ("$.state.policy.max_cleanup_duration_ms", json!(59999)),
        ("$.state.policy.max_cases", json!(9)),
        ("$.state.policy.max_requests", json!(15)),
        ("$.state.policy.max_request_body_bytes", json!(1048575)),
        ("$.state.policy.max_input_tokens_per_request", json!(131071)),
        ("$.state.policy.max_output_tokens_per_request", json!(16383)),
        ("$.state.policy.allow_experimental_controls", json!(false)),
        ("$.state.policy.allowed_manifest_digests", json!([])),
        ("$.state.hardware_fingerprint", json!("changed")),
        ("$.state.environment_fingerprint", json!("changed")),
    ];
    for (path, value) in mutations {
        for accepted in [false, true] {
            let (store, s, c) = created("fake");
            let receipt = accepted.then(|| {
                store
                    .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
                    .unwrap()
            });
            store.conn.execute("UPDATE host_qualification_policies SET policy_json=json_set(policy_json,?1,json(?2))",params![path,value.to_string()]).unwrap();
            let before = durable(&store);
            if let Some(r) = receipt {
                assert!(
                    matches!(
                        arm(&store, &s, r.step_id()),
                        Err(LifecycleError::Rejected(_))
                    ),
                    "{path}"
                );
            } else {
                assert!(
                    matches!(
                        store.accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100),
                        Err(Error::QualificationDenied)
                    ),
                    "{path}"
                );
            }
            assert_eq!(durable(&store), before);
        }
    }
}
#[test]
fn initialize_arm_uses_current_ledger_epoch_and_denied_admission_keeps_intent() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    // A committed policy import is an unrelated legitimate ledger change.
    let mut policy = store.resource_policy("lab").unwrap().unwrap();
    policy
        .controls
        .domains
        .get_mut("unified")
        .unwrap()
        .free_reserve += 1;
    let observations = [mllm_domain::resources::MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 1_i64 << 50,
        available_bytes: 1_i64 << 50,
        sampled_at_ms: 1150,
    }];
    store
        .update_resource_policy(
            &s,
            "owner",
            "lab",
            policy.revision,
            "policy-update",
            &policy.controls,
            &observations,
            1150,
        )
        .unwrap();
    let epoch = store.resource_snapshot().unwrap().epoch;
    assert!(matches!(
        arm(&store, &s, r.step_id()),
        Ok(ArmResult::New { .. })
    ));
    assert_eq!(store.resource_snapshot().unwrap().epoch, epoch + 1);
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    let before = durable(&store);
    let denied = store.arm_step(&s, r.step_id(), AdmissionContext::new(&[], &[], 1200, 1, 0));
    assert!(matches!(denied, Err(LifecycleError::Rejected(_))));
    assert_eq!(durable(&store), before);
}
#[test]
fn initialize_restart_retains_peak_and_never_replays_arm() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let store = crate::Store::open(&path).unwrap();
    let (manifest, host, policy) = fixture("fake");
    let s = setup(&store, &policy);
    let c = store
        .create_candidate_run(&s, "owner", "create", &command(&manifest), &host, 1000)
        .unwrap();
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "a", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let ledger = store.resource_snapshot().unwrap();
    drop(store);
    let store = crate::Store::open(&path).unwrap();
    let new = store.begin_coordinator_session().unwrap();
    assert_eq!(store.resource_snapshot().unwrap(), ledger);
    let states:(String,String,String)=store.conn.query_row("SELECT s.state,l.state,b.state FROM lifecycle_steps s JOIN lifecycle_runs l ON l.operation_id=s.operation_id JOIN runtime_bindings b ON b.id=s.binding_id",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        states,
        ("uncertain".into(), "uncertain".into(), "uncertain".into())
    );
    assert!(matches!(
        store.candidate_initialize_execution(&s, r.step_id()),
        Err(LifecycleError::Stale)
    ));
    assert!(matches!(
        store.candidate_initialize_execution(&new, r.step_id()),
        Err(LifecycleError::Stale)
    ));
    assert!(matches!(
        arm(&store, &new, r.step_id()),
        Err(LifecycleError::Stale)
    ));
    assert_eq!(
        store
            .accept_candidate_initialize(&new, "owner", c.run_id(), "a", BODY, 999999)
            .unwrap(),
        r
    );
    assert_eq!(count(&store, "resource_grants"), 1);
    assert_eq!(count(&store, "qualification_case_actions"), 1);
    assert_eq!(count(&store, "endpoint_leases"), 1);
}
#[test]
fn initialize_two_connections_serialize_acceptance_and_arm() {
    for same_key in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let store = crate::Store::open(&path).unwrap();
        let (manifest, host, policy) = fixture("fake");
        let s = setup(&store, &policy);
        let c = store
            .create_candidate_run(&s, "owner", "create", &command(&manifest), &host, 1000)
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|i| {
                let path = path.clone();
                let s = s.clone();
                let run = c.run_id().to_owned();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    let store = crate::Store::open(&path).unwrap();
                    b.wait();
                    store.accept_candidate_initialize(
                        &s,
                        "owner",
                        &run,
                        if same_key || i == 0 { "a" } else { "b" },
                        BODY,
                        1100,
                    )
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            if same_key { 2 } else { 1 }
        );
        if same_key {
            assert_eq!(results[0].as_ref().unwrap(), results[1].as_ref().unwrap());
        } else {
            assert!(
                results
                    .iter()
                    .any(|r| matches!(r, Err(Error::LifecycleConflict)))
            );
        }
        let step = results
            .iter()
            .find_map(|r| r.as_ref().ok())
            .unwrap()
            .step_id()
            .to_owned();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let s = s.clone();
                let step = step.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    let store = crate::Store::open(&path).unwrap();
                    b.wait();
                    arm(&store, &s, &step).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, ArmResult::New { .. }))
                .count(),
            1
        );
        assert_eq!(count(&store, "qualification_case_actions"), 1);
        assert_eq!(count(&store, "resource_grants"), 1);
    }
}

#[test]
fn initialize_acceptance_reserves_one_case_without_effects() {
    let store = crate::Store::open_in_memory().unwrap();
    let (manifest, host, policy) = fixture("fake");
    let session = setup(&store, &policy);
    let created = store
        .create_candidate_run(
            &session,
            "owner",
            "create-key",
            &command(&manifest),
            &host,
            1000,
        )
        .unwrap();
    let epoch_before = store.resource_snapshot().unwrap().epoch;
    let receipt = store
        .accept_candidate_initialize(
            &session,
            "owner",
            created.run_id(),
            "initialize-key",
            r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#,
            1100,
        )
        .unwrap();
    assert_ne!(receipt.operation_id(), created.operation_id());
    assert_eq!(
        receipt.case_id(),
        manifest["cases"][0]["id"].as_str().unwrap()
    );
    let plan = store
        .candidate_initialize_plan(&session, "owner", created.run_id(), receipt.step_id())
        .unwrap()
        .unwrap();
    assert_eq!(plan.binding_id(), created.binding_id());
    assert_eq!(plan.incarnation(), created.incarnation());
    assert_eq!(store.resource_snapshot().unwrap().epoch, epoch_before);
    let state: String = store
        .conn
        .query_row(
            "SELECT state FROM runtime_bindings WHERE id=?1",
            [created.binding_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "reserved");
    assert_eq!(
        store
            .candidate_run_snapshot("owner", created.run_id())
            .unwrap()
            .unwrap()
            .receipt(),
        &created
    );
}
