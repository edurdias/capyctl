use super::*;
use crate::Store;
use mllm_config::effective::{resolve_effective, HostPolicy};
use mllm_domain::resources::MemoryObservation;
use serde_json::{json, Value};
use std::net::TcpListener;

pub(super) fn fixture(engine: &str) -> (Value, Value, HostPolicy) {
    let (candidate, ordinary) = match engine {
        "fake" => (
            include_str!("../../../mllm-config/tests/fixtures/candidate-fake.json"),
            include_str!("../../../mllm-config/tests/fixtures/effective-fake-golden.json"),
        ),
        "vllm" => (
            include_str!("../../../mllm-config/tests/fixtures/candidate-vllm.json"),
            include_str!("../../../mllm-config/tests/fixtures/effective-vllm-golden.json"),
        ),
        "sglang" => (
            include_str!("../../../mllm-config/tests/fixtures/candidate-sglang.json"),
            include_str!("../../../mllm-config/tests/fixtures/effective-sglang-golden.json"),
        ),
        _ => unreachable!(),
    };
    let ordinary: Value = serde_json::from_str(ordinary).unwrap();
    let mut host = ordinary["input"]["host"].clone();
    let mut manifest: Value = serde_json::from_str(candidate).unwrap();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    host["resource_policy"]["endpoint_port_range"] = json!({"start":port,"end":port});
    manifest["limits"]["max_run_duration_ms"] = json!(600_000);
    manifest["limits"]["max_cleanup_duration_ms"] = json!(60_000);
    let digest = validate_candidate_reviewed_snapshot_text(&manifest.to_string())
        .unwrap()
        .manifest_digest()
        .to_owned();
    host["qualification_policy"] = json!({"revision":1,"allow_qualification_runs":true,
        "allow_experimental_controls":true,"allowed_manifest_digests":[digest],"max_run_duration":"600s",
        "max_cleanup_duration":"60s","max_cases":128,"max_requests":4096,"max_request_body_bytes":"1MiB",
        "max_input_tokens_per_request":131072,"max_output_tokens_per_request":16384});
    let policy = resolve_effective(&ordinary["input"]["deployment"], &host)
        .unwrap()
        .host;
    (manifest, host, policy)
}

pub(super) fn setup(store: &Store, policy: &HostPolicy) -> CoordinatorSession {
    let session = store.begin_coordinator_session().unwrap();
    let observations: Vec<_> = policy
        .domains
        .keys()
        .map(|domain| MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1000,
        })
        .collect();
    store
        .import_resource_policy(&session, policy, &observations, 1000)
        .unwrap();
    store.import_qualification_policy(&session, policy).unwrap();
    session
}

pub(super) fn command(manifest: &Value) -> String {
    json!({"host_id":manifest["host"]["id"],"expected_host_revision":1,
        "recipe_digest":validate_candidate_reviewed_snapshot_text(&manifest.to_string()).unwrap().manifest_digest(),
        "manifest":manifest,"deadline_ms":500_000,"allow_owned_abort_cleanup":true}).to_string()
}

fn counts(store: &Store) -> Vec<i64> {
    [
        "deployments",
        "effective_revisions",
        "runtime_bindings",
        "endpoint_leases",
        "operations",
        "qualification_runs",
        "command_receipts",
        "management_events",
        "deployment_routes",
        "lifecycle_runs",
        "lifecycle_steps",
        "generation_history",
        "owners",
        "reservations",
        "resource_owners",
        "resource_grants",
        "request_leases",
        "qualifications",
        "qualification_evidence_refs",
        "lifecycle_evidence",
    ]
    .iter()
    .map(|table| {
        store
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    })
    .collect()
}

#[test]
fn candidate_creation_accepts_all_engines_without_execution_effects() {
    for engine in ["fake", "vllm", "sglang"] {
        let (manifest, host, policy) = fixture(engine);
        let store = Store::open_in_memory().unwrap();
        let session = setup(&store, &policy);
        let before = counts(&store);
        let receipt = store
            .create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000)
            .unwrap();
        let after = counts(&store);
        assert_eq!(
            after
                .iter()
                .zip(before)
                .map(|(a, b)| a - b)
                .collect::<Vec<_>>(),
            [vec![1; 8], vec![0; 12]].concat()
        );
        let snapshot = store
            .candidate_run_snapshot("owner", receipt.run_id())
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.receipt(), &receipt);
        assert_eq!(snapshot.state(), CandidateRunState::Accepted);
        assert_eq!(snapshot.requests_used(), 0);
        assert_eq!(snapshot.cleanup_state(), CandidateCleanupState::Retained);
        assert_eq!(
            snapshot.reviewed_manifest().reviewed_json(),
            validate_candidate_reviewed_snapshot_text(&manifest.to_string())
                .unwrap()
                .reviewed_json()
        );
        assert_eq!(
            snapshot.runtime_credential_ref(),
            Some("secret://engine-key")
        );
        if engine == "sglang" {
            assert!(snapshot.admin_credential_ref().is_some());
        }
        let flags: (String,String,i64,i64,i64) = store.conn.query_row("SELECT desired_state,observed_state,admission_enabled,dispatch_enabled,suspended FROM deployments",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(flags, ("stopped".into(), "stopped".into(), 0, 0, 0));
        assert_eq!(
            store
                .conn
                .query_row("SELECT kind FROM deployments", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "model"
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT ownership FROM runtime_bindings", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "managed"
        );
        assert!(matches!(
            store.runtime_binding(receipt.deployment_id()),
            Err(crate::lifecycle::LifecycleError::Invalid)
        ));
        assert_eq!(
            store
                .conn
                .query_row("SELECT epoch FROM resource_ledger_meta", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(store
            .candidate_run_snapshot("another", receipt.run_id())
            .unwrap()
            .is_none());
        assert!(TcpListener::bind(("127.0.0.1", policy.endpoint_port_range.start)).is_ok());
    }
}

#[test]
fn candidate_creation_replay_precedes_local_policy_and_time_checks() {
    let (manifest, mut host, mut policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let body = command(&manifest);
    let receipt = store
        .create_candidate_run(&session, "owner", "key", &body, &host, 1000)
        .unwrap();
    host["runtime_profiles"] = json!({});
    host["resource_policy"] = json!("invalid");
    policy.qualification_policy = None;
    store
        .import_qualification_policy(&session, &policy)
        .unwrap();
    let before = counts(&store);
    assert_eq!(
        store
            .create_candidate_run(&session, "owner", "key", &body, &host, i64::MAX)
            .unwrap(),
        receipt
    );
    assert_eq!(counts(&store), before);
    let mut changed: Value = serde_json::from_str(&body).unwrap();
    changed["deadline_ms"] = json!(500_001);
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "key", &changed.to_string(), &host, 1000),
        Err(CandidateCreationError::IdempotencyConflict)
    ));
    let newer = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "key", &body, &host, 1000),
        Err(CandidateCreationError::StaleSession)
    ));
    assert_eq!(
        store
            .create_candidate_run(&newer, "owner", "key", &body, &Value::Null, -1)
            .unwrap(),
        receipt
    );
}

#[test]
fn candidate_creation_late_failure_rolls_back_and_releases_listener() {
    let (manifest, host, policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    store.conn.execute_batch("CREATE TRIGGER fail_candidate_event BEFORE INSERT ON management_events WHEN NEW.kind='candidate_run_accepted' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = counts(&store);
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000),
        Err(CandidateCreationError::Sql(_))
    ));
    assert_eq!(counts(&store), before);
    assert!(TcpListener::bind(("127.0.0.1", policy.endpoint_port_range.start)).is_ok());
}

#[test]
fn candidate_creation_rejects_malformed_command_without_writes() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    assert!(store
        .create_candidate_run(&session, "owner", "key", "{}", &Value::Null, 0)
        .is_err());
    assert_eq!(store.deployment_count().unwrap(), 0);
}

#[test]
fn candidate_creation_fake_can_freeze_absent_auth() {
    let (mut manifest, mut host, mut policy) = fixture("fake");
    host["runtime_profiles"]["local"]["security"]
        .as_object_mut()
        .unwrap()
        .remove("credential_ref");
    manifest["effective_recipe"]["resolved_profile"]["runtime_auth"] = json!(false);
    policy
        .qualification_policy
        .as_mut()
        .unwrap()
        .allowed_manifest_digests =
        vec![
            validate_candidate_reviewed_snapshot_text(&manifest.to_string())
                .unwrap()
                .manifest_digest()
                .into(),
        ];
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let receipt = store
        .create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000)
        .unwrap();
    let snapshot = store
        .candidate_run_snapshot("owner", receipt.run_id())
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.runtime_credential_ref(), None);
    assert_eq!(snapshot.admin_credential_ref(), None);
}

#[test]
fn candidate_creation_every_current_policy_bound_denies_without_writes() {
    for case in 0..12 {
        let (manifest, host, mut policy) = fixture("fake");
        let p = policy.qualification_policy.as_mut().unwrap();
        match case {
            0 => p.allow_qualification_runs = false,
            1 => p.allow_experimental_controls = false,
            2 => p.allowed_manifest_digests.clear(),
            3 => p.max_cases = 1,
            4 => p.max_requests = 1,
            5 => p.max_request_body_bytes = 1,
            6 => p.max_input_tokens_per_request = 1,
            7 => p.max_output_tokens_per_request = 1,
            8 => p.max_run_duration_ms = 599_999,
            9 => p.max_cleanup_duration_ms = 59_999,
            10 => policy.hardware_fingerprint = "changed".into(),
            11 => policy.environment_fingerprint = "changed".into(),
            _ => unreachable!(),
        }
        let store = Store::open_in_memory().unwrap();
        let session = setup(&store, &policy);
        let before = counts(&store);
        assert!(
            matches!(
                store.create_candidate_run(
                    &session,
                    "owner",
                    "key",
                    &command(&manifest),
                    &host,
                    1000
                ),
                Err(CandidateCreationError::QualificationDenied)
            ),
            "case {case}"
        );
        assert_eq!(counts(&store), before, "case {case}");
    }
}

#[test]
fn candidate_creation_deadlines_use_checked_positive_remaining_time() {
    let (manifest, host, policy) = fixture("fake");
    for (now, deadline, allowed) in [
        (0, 1, true),
        (0, 600_000, true),
        (0, 600_001, false),
        (500_000, 500_000, false),
        (-1, 1, false),
        (i64::MIN, i64::MAX, false),
        (i64::MAX - 1, i64::MAX, true),
    ] {
        let store = Store::open_in_memory().unwrap();
        let session = setup(&store, &policy);
        let mut body: Value = serde_json::from_str(&command(&manifest)).unwrap();
        body["deadline_ms"] = json!(deadline);
        assert_eq!(
            store
                .create_candidate_run(&session, "owner", "key", &body.to_string(), &host, now)
                .is_ok(),
            allowed,
            "{now} {deadline}"
        );
    }
}

#[test]
fn candidate_creation_strict_command_and_semantic_retries() {
    let (manifest, host, policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let body = command(&manifest);
    let receipt = store
        .create_candidate_run(&session, "owner", "key", &body, &host, 1000)
        .unwrap();
    let value: Value = serde_json::from_str(&body).unwrap();
    let reordered = format!(
        "{{ {} }}",
        value
            .as_object()
            .unwrap()
            .iter()
            .rev()
            .map(|(k, v)| format!("{}: {}", serde_json::to_string(k).unwrap(), v))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert_eq!(
        store
            .create_candidate_run(&session, "owner", "key", &reordered, &Value::Null, 0)
            .unwrap(),
        receipt
    );
    for malformed in [
        format!("{body} {{}}"),
        body.replacen(
            "\"deadline_ms\":500000",
            "\"deadline_ms\":500000,\"deadline_ms\":500000",
            1,
        ),
        body.replacen("\"count\":1", "\"count\":1,\"count\":1", 1),
        body.replacen("\"host_id\":\"lab\"", "\"host_id\":\"wrong\"", 1),
    ] {
        assert_ne!(malformed, body);
        assert!(matches!(
            store.create_candidate_run(&session, "owner", "key", &malformed, &host, 1000),
            Err(CandidateCreationError::InvalidCommand)
        ));
    }
    for field in [
        "host_id",
        "expected_host_revision",
        "recipe_digest",
        "manifest",
        "deadline_ms",
        "allow_owned_abort_cleanup",
    ] {
        for replacement in [None, Some(Value::Null)] {
            let mut bad = value.clone();
            if let Some(v) = replacement {
                bad[field] = v;
            } else {
                bad.as_object_mut().unwrap().remove(field);
            }
            assert!(
                matches!(
                    store.create_candidate_run(
                        &session,
                        "owner",
                        "key",
                        &bad.to_string(),
                        &host,
                        1000
                    ),
                    Err(CandidateCreationError::InvalidCommand)
                ),
                "{field}"
            );
        }
    }
    for change in 0..6 {
        let mut changed = value.clone();
        match change {
            0 => changed["deadline_ms"] = json!(499_999),
            1 => changed["allow_owned_abort_cleanup"] = json!(false),
            2 => changed["expected_host_revision"] = json!(2),
            3 => changed["manifest"]["limits"]["max_requests"] = json!(17),
            4 => {
                for c in changed["manifest"]["cases"].as_array_mut().unwrap() {
                    if c.get("corpus_digest").is_some() {
                        c["corpus_digest"] = json!("a".repeat(64));
                    }
                }
            }
            5 => {
                changed["host_id"] = json!("other");
                changed["manifest"]["host"]["id"] = json!("other");
            }
            _ => unreachable!(),
        }
        changed["recipe_digest"] = json!(validate_candidate_reviewed_snapshot_text(
            &changed["manifest"].to_string()
        )
        .unwrap()
        .manifest_digest());
        assert!(
            matches!(
                store.create_candidate_run(
                    &session,
                    "owner",
                    "key",
                    &changed.to_string(),
                    &host,
                    1000
                ),
                Err(CandidateCreationError::IdempotencyConflict)
            ),
            "{change}"
        );
    }
    assert!(matches!(
        store.create_candidate_run(&session, "other", "key", &body, &host, 1000),
        Err(CandidateCreationError::EndpointUnavailable)
    ));
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "other", &body, &host, 1000),
        Err(CandidateCreationError::EndpointUnavailable)
    ));
}

#[test]
fn candidate_creation_db_controls_override_stale_mutable_local_controls() {
    let (manifest, mut host, policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    host["resource_policy"]["queue"] = json!("invalid stale controls");
    host["resource_policy"]["domains"]["unified"] = json!("invalid stale controls");
    host["resource_policy"]["observation_ttl"] = Value::Null;
    host["resource_policy"]["device_sharing"] = json!("exclusive");
    host["resource_policy"]["devices"]["gpu0"]["sharing"] = json!("exclusive");
    assert!(store
        .create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000)
        .is_ok());
}

#[test]
fn candidate_creation_immutable_local_context_drift_rejects() {
    for case in 0..4 {
        let (manifest, mut host, policy) = fixture("fake");
        let store = Store::open_in_memory().unwrap();
        let session = setup(&store, &policy);
        match case {
            0 => host["name"] = json!("other"),
            1 => {
                host["resource_policy"]["domains"]
                    .as_object_mut()
                    .unwrap()
                    .remove("unified");
            }
            2 => host["resource_policy"]["devices"]["gpu0"]["domain"] = json!("other"),
            3 => host["resource_policy"]["endpoint_port_range"]["end"] = json!(1),
            _ => unreachable!(),
        }
        let before = counts(&store);
        assert!(matches!(
            store.create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000),
            Err(CandidateCreationError::RevisionConflict)
        ));
        assert_eq!(counts(&store), before);
    }
}

#[test]
fn candidate_creation_historical_corruption_rejects_both_read_and_retry() {
    for (table, column, path, value) in [
        (
            "effective_revisions",
            "effective_json",
            "/version",
            json!(2),
        ),
        (
            "effective_revisions",
            "effective_json",
            "/manifest_digest",
            json!("b".repeat(64)),
        ),
        (
            "effective_revisions",
            "effective_json",
            "/recipe_fingerprint",
            json!("b".repeat(64)),
        ),
        (
            "effective_revisions",
            "effective_json",
            "/total_case_request_budget",
            json!(8),
        ),
        (
            "effective_revisions",
            "effective_json",
            "/credential_refs/runtime",
            json!(""),
        ),
        (
            "effective_revisions",
            "effective_json",
            "/credential_refs/admin",
            json!("secret://engine-key"),
        ),
        (
            "effective_revisions",
            "effective_json",
            "/reviewed_manifest/effective_recipe/resolved_profile/revision",
            json!(0),
        ),
        (
            "qualification_runs",
            "authorization_json",
            "/principal_id",
            json!("other"),
        ),
        (
            "qualification_runs",
            "authorization_json",
            "/descriptor/revision",
            json!(2),
        ),
        (
            "qualification_runs",
            "authorization_json",
            "/qualification_runs_permitted",
            json!(false),
        ),
        (
            "qualification_runs",
            "authorization_json",
            "/max_cleanup_duration_ms",
            json!(1),
        ),
        (
            "qualification_runs",
            "authorization_json",
            "/deadline_ms",
            json!(500_001),
        ),
        (
            "command_receipts",
            "response_json",
            "/request_hash",
            json!("b".repeat(64)),
        ),
        (
            "command_receipts",
            "response_json",
            "/resource_policy_revision",
            json!(2),
        ),
        (
            "runtime_bindings",
            "binding_json",
            "/descriptor/recipe_fingerprint",
            json!("b".repeat(64)),
        ),
        (
            "runtime_bindings",
            "binding_json",
            "/auth/admin",
            json!("other"),
        ),
    ] {
        let (manifest, host, policy) = fixture("fake");
        let store = Store::open_in_memory().unwrap();
        let session = setup(&store, &policy);
        let body = command(&manifest);
        let receipt = store
            .create_candidate_run(&session, "owner", "key", &body, &host, 1000)
            .unwrap();
        let text: String = store
            .conn
            .query_row(&format!("SELECT {column} FROM {table}"), [], |r| r.get(0))
            .unwrap();
        let mut value_stored: Value = serde_json::from_str(&text).unwrap();
        *value_stored.pointer_mut(path).unwrap() = value;
        store
            .conn
            .execute(
                &format!("UPDATE {table} SET {column}=?1"),
                [value_stored.to_string()],
            )
            .unwrap();
        assert!(
            matches!(
                store.candidate_run_snapshot("owner", receipt.run_id()),
                Err(CandidateCreationError::CorruptStoredData)
            ),
            "{table}{path}"
        );
        assert!(
            matches!(
                store.create_candidate_run(&session, "owner", "key", &body, &host, 1000),
                Err(CandidateCreationError::CorruptStoredData)
            ),
            "{table}{path}"
        );
    }
}

#[test]
fn candidate_creation_stored_envelopes_require_nullable_fields_and_reject_duplicates() {
    for (table, column) in [
        ("effective_revisions", "effective_json"),
        ("qualification_runs", "authorization_json"),
        ("command_receipts", "response_json"),
        ("runtime_bindings", "binding_json"),
    ] {
        for mutation in 0..5 {
            let (manifest, host, policy) = fixture("fake");
            let store = Store::open_in_memory().unwrap();
            let session = setup(&store, &policy);
            let receipt = store
                .create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000)
                .unwrap();
            let text: String = store
                .conn
                .query_row(&format!("SELECT {column} FROM {table}"), [], |r| r.get(0))
                .unwrap();
            let mutated = match mutation {
                0 => format!("{{\"unknown\":0,{}", &text[1..]),
                1 => text.replacen("\"version\":", "\"version\":1,\"version\":", 1),
                2 => " ".repeat(MAX_BYTES + 1),
                3 if table == "effective_revisions" || table == "runtime_bindings" => {
                    text.replacen(",\"admin\":null", "", 1)
                }
                3 => text.replacen("\"version\":1,", "", 1),
                _ if table == "effective_revisions" => {
                    text.replacen("\"count\":1", "\"count\":1,\"count\":1", 1)
                }
                _ => text
                    .replacen("\"version\":1,", "", 1)
                    .replacen("\"version\":2,", "", 1),
            };
            assert_ne!(mutated, text, "{table} {mutation}");
            store
                .conn
                .execute(&format!("UPDATE {table} SET {column}=?1"), [mutated])
                .unwrap();
            assert!(
                matches!(
                    store.candidate_run_snapshot("owner", receipt.run_id()),
                    Err(CandidateCreationError::CorruptStoredData)
                ),
                "{table} {mutation}"
            );
        }
    }
}

#[test]
fn candidate_creation_hash_has_literal_domain_separated_golden() {
    let (manifest, _, _) = fixture("fake");
    let (_, _, hash) = parse_command(&command(&manifest), "owner", "key").unwrap();
    // Independently calculated with Node crypto over recursively sorted fixture JSON.
    assert_eq!(
        hash,
        "7c1090f4f5c17363818a43c0b6eccc196d97c1baf7e1e4cde406afd67cca8d85"
    );
}

fn observations(policy: &HostPolicy) -> Vec<MemoryObservation> {
    policy
        .domains
        .keys()
        .map(|domain| MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1000,
        })
        .collect()
}

#[test]
fn candidate_creation_current_db_queue_and_separate_revisions_are_enforced() {
    let (manifest, host, mut policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    policy.qualification_policy.as_mut().unwrap().revision = 2;
    store
        .import_qualification_policy(&session, &policy)
        .unwrap();
    let receipt = store
        .create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000)
        .unwrap();
    assert_eq!(receipt.resource_policy_revision(), 1);
    assert_eq!(receipt.qualification_policy_revision(), 2);
    let mut controls = mllm_config::resource_controls::ResourceControls::from_host(&policy);
    controls.queue.request_deadline_ms = 200_000;
    store
        .update_resource_policy(
            &session,
            "owner",
            "lab",
            1,
            "update",
            &controls,
            &observations(&policy),
            1000,
        )
        .unwrap();
    let mut body: Value = serde_json::from_str(&command(&manifest)).unwrap();
    body["expected_host_revision"] = json!(2);
    let before = counts(&store);
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "new", &body.to_string(), &host, 1000),
        Err(CandidateCreationError::InvalidCommand)
    ));
    assert_eq!(counts(&store), before);
    assert_eq!(
        store
            .create_candidate_run(
                &session,
                "owner",
                "key",
                &command(&manifest),
                &Value::Null,
                0
            )
            .unwrap(),
        receipt
    );
}

#[test]
fn candidate_creation_replay_ignores_each_local_drift_and_qualification_revocation() {
    let (manifest, host, mut policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let body = command(&manifest);
    let receipt = store
        .create_candidate_run(&session, "owner", "key", &body, &host, 1000)
        .unwrap();
    for case in 0..5 {
        let mut changed = host.clone();
        match case {
            0 => changed["runtime_profiles"]["local"]["build_fingerprint"] = json!("edited"),
            1 => changed["runtime_profiles"] = json!({}),
            2 => {
                changed["runtime_profiles"]["local"]["security"]["credential_ref"] =
                    json!("rotated")
            }
            3 => changed["resource_policy"] = Value::Null,
            4 => changed["name"] = json!("different"),
            _ => unreachable!(),
        }
        let before = counts(&store);
        assert_eq!(
            store
                .create_candidate_run(&session, "owner", "key", &body, &changed, i64::MAX)
                .unwrap(),
            receipt
        );
        assert_eq!(counts(&store), before);
    }
    let p = policy.qualification_policy.as_mut().unwrap();
    p.revision = 2;
    p.allow_qualification_runs = false;
    p.allow_experimental_controls = false;
    p.allowed_manifest_digests.clear();
    store
        .import_qualification_policy(&session, &policy)
        .unwrap();
    assert_eq!(
        store
            .create_candidate_run(&session, "owner", "key", &body, &Value::Null, 0)
            .unwrap(),
        receipt
    );
    assert_eq!(
        store
            .candidate_run_snapshot("owner", receipt.run_id())
            .unwrap()
            .unwrap()
            .runtime_credential_ref(),
        Some("secret://engine-key")
    );
}

#[test]
fn candidate_creation_policy_row_classification_preserves_sql_failures() {
    for (sql,category) in [
        ("DELETE FROM host_resource_policies",0),
        ("UPDATE host_resource_policies SET host_id='other'",0),
        ("INSERT INTO host_resource_policies SELECT 'other',revision,policy_json FROM host_resource_policies",1),
        ("UPDATE host_resource_policies SET policy_json='{}'",1),
        ("DELETE FROM host_qualification_policies",2),
        ("UPDATE host_qualification_policies SET host_id='other'",1),
        ("INSERT INTO host_qualification_policies SELECT 'other',revision,policy_json FROM host_qualification_policies",1),
        ("UPDATE host_qualification_policies SET policy_json='{}'",1),
        ("DROP TABLE coordinator_session",3),
    ] {
        let (manifest,host,policy) = fixture("fake");
        let store = Store::open_in_memory().unwrap();
        let session = setup(&store,&policy);
        store.conn.execute_batch(sql).unwrap();
        let before = counts(&store);
        let error = store.create_candidate_run(&session,"owner","key",&command(&manifest),&host,1000).unwrap_err();
        match category {
            0 => assert!(matches!(error,CandidateCreationError::RevisionConflict),"{sql}: {error}"),
            1 => assert!(matches!(error,CandidateCreationError::CorruptStoredData),"{sql}: {error}"),
            2 => assert!(matches!(error,CandidateCreationError::QualificationDenied),"{sql}: {error}"),
            _ => assert!(matches!(error,CandidateCreationError::Sql(_)),"{sql}: {error}"),
        }
        assert_eq!(counts(&store),before);
    }
}

#[test]
fn candidate_creation_endpoint_occupied_and_retained_ports_are_skipped() {
    let (manifest, mut host, mut policy) = fixture("fake");
    // Find three adjacent explicitly held local ports without sleep or engine activity.
    let (port, listeners) = (20000_u16..60000)
        .find_map(|start| {
            let listeners = (start..=start + 2)
                .map(|p| TcpListener::bind(("127.0.0.1", p)))
                .collect::<std::io::Result<Vec<_>>>()
                .ok()?;
            Some((start, listeners))
        })
        .unwrap();
    host["resource_policy"]["endpoint_port_range"] = json!({"start":port,"end":port+2});
    policy.endpoint_port_range.start = port;
    policy.endpoint_port_range.end = port + 2;
    let mut listeners = listeners.into_iter();
    let occupied = listeners.next().unwrap();
    drop(listeners);
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let one = store
        .create_candidate_run(&session, "owner", "one", &command(&manifest), &host, 1000)
        .unwrap();
    let first_port: u16 = store
        .conn
        .query_row(
            "SELECT port FROM endpoint_leases WHERE binding_id=?1",
            [one.binding_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(first_port, port + 1);
    assert!(TcpListener::bind(("127.0.0.1", first_port)).is_ok());
    let two = store
        .create_candidate_run(&session, "owner", "two", &command(&manifest), &host, 1000)
        .unwrap();
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT port FROM endpoint_leases WHERE binding_id=?1",
                [two.binding_id()],
                |r| r.get::<_, u16>(0)
            )
            .unwrap(),
        port + 2
    );
    let before = counts(&store);
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "three", &command(&manifest), &host, 1000),
        Err(CandidateCreationError::EndpointUnavailable)
    ));
    assert_eq!(counts(&store), before);
    drop(occupied);
}

#[test]
fn candidate_creation_file_reopen_and_concurrent_receipts_are_durable() {
    for changed_body in [false, true] {
        let (manifest, host, policy) = fixture("fake");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let session = setup(&store, &policy);
        let left = Store::open(&path).unwrap();
        let right = Store::open(&path).unwrap();
        let body = command(&manifest);
        let mut changed: Value = serde_json::from_str(&body).unwrap();
        if changed_body {
            changed["deadline_ms"] = json!(499_999);
        }
        let second = changed.to_string();
        let barrier = std::sync::Barrier::new(2);
        let (a, b) = std::thread::scope(|scope| {
            let barrier = &barrier;
            let host = &host;
            let session = &session;
            let body = &body;
            let second = &second;
            let a = scope.spawn(move || {
                barrier.wait();
                left.create_candidate_run(session, "owner", "key", body, host, 1000)
            });
            let b = scope.spawn(move || {
                barrier.wait();
                right.create_candidate_run(session, "owner", "key", second, host, 1000)
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        let (receipt, winning_body) = if changed_body {
            match (a, b) {
                (Ok(receipt), Err(CandidateCreationError::IdempotencyConflict)) => (receipt, body),
                (Err(CandidateCreationError::IdempotencyConflict), Ok(receipt)) => {
                    (receipt, second)
                }
                other => panic!("unexpected concurrent results: {other:?}"),
            }
        } else {
            assert_eq!(a.as_ref().unwrap(), b.as_ref().unwrap());
            (a.unwrap(), body)
        };
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM qualification_runs", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM management_events WHERE kind='candidate_run_accepted'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let new_session = reopened.begin_coordinator_session().unwrap();
        let before = counts(&reopened);
        assert_eq!(
            reopened
                .create_candidate_run(
                    &new_session,
                    "owner",
                    "key",
                    &winning_body,
                    &Value::Null,
                    i64::MAX
                )
                .unwrap(),
            receipt
        );
        assert!(matches!(
            reopened.create_candidate_run(&session, "owner", "key", &winning_body, &Value::Null, 0),
            Err(CandidateCreationError::StaleSession)
        ));
        assert_eq!(counts(&reopened), before);
    }
}

#[test]
fn candidate_creation_historical_terminal_state_does_not_recreate_acceptance() {
    let (manifest, host, policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let body = command(&manifest);
    let receipt = store
        .create_candidate_run(&session, "owner", "key", &body, &host, 1000)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE qualification_runs SET state='expired',requests_used=7",
            [],
        )
        .unwrap();
    store
        .conn
        .execute("UPDATE deployments SET revision=2,current_generation=3", [])
        .unwrap();
    let snapshot = store
        .candidate_run_snapshot("owner", receipt.run_id())
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.state(), CandidateRunState::Expired);
    assert_eq!(snapshot.requests_used(), 7);
    assert_eq!(snapshot.receipt(), &receipt);
    assert_eq!(
        store
            .create_candidate_run(&session, "owner", "key", &body, &Value::Null, 0)
            .unwrap(),
        receipt
    );
    store.conn.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES('cleanup',?1,'stop','succeeded')",[receipt.deployment_id()]).unwrap();
    store.conn.execute("INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) VALUES('cleanup',?1,1,1,?2,'stop','succeeded',600000,'{}')",params![receipt.deployment_id(),session.id()]).unwrap();
    store.conn.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES('cleanup-step','cleanup',0,?1,?2,?3,'completed','{}')",params![receipt.deployment_id(),receipt.binding_id(),session.id()]).unwrap();
    store
        .conn
        .execute("DELETE FROM endpoint_leases", [])
        .unwrap();
    store
        .conn
        .execute("UPDATE runtime_bindings SET state='released'", [])
        .unwrap();
    store.conn.execute("UPDATE qualification_runs SET cleanup_state='verified_gone',cleanup_step_id='cleanup-step'",[]).unwrap();
    // A bare completed row cannot establish verified disappearance. Real cleanup
    // replay is exercised through the acceptance/arm/association protocol.
    assert!(matches!(
        store.candidate_run_snapshot("owner", receipt.run_id()),
        Err(CandidateCreationError::CorruptStoredData)
    ));
    assert_eq!(
        store
            .create_candidate_run(&session, "owner", "key", &body, &Value::Null, 0)
            .unwrap(),
        receipt
    );
    store
        .conn
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    store
        .conn
        .execute("UPDATE lifecycle_steps SET binding_id='other'", [])
        .unwrap();
    assert!(matches!(
        store.candidate_run_snapshot("owner", receipt.run_id()),
        Err(CandidateCreationError::CorruptStoredData)
    ));
}

#[test]
fn candidate_creation_row_identity_corruption_never_recreates_a_receipt() {
    for sql in [
        "UPDATE effective_revisions SET fingerprint='wrong'",
        "UPDATE qualification_runs SET requests_used=4097",
        "UPDATE qualification_runs SET deadline_ms=1",
        "UPDATE qualification_runs SET incarnation='other'",
        "UPDATE runtime_bindings SET revision=2",
        "UPDATE operations SET state='failed' WHERE kind='candidate_create'",
        "UPDATE command_receipts SET operation_id='dangling'",
        "DELETE FROM effective_revisions",
    ] {
        let (manifest, host, policy) = fixture("fake");
        let store = Store::open_in_memory().unwrap();
        let session = setup(&store, &policy);
        let body = command(&manifest);
        let receipt = store
            .create_candidate_run(&session, "owner", "key", &body, &host, 1000)
            .unwrap();
        store
            .conn
            .pragma_update(None, "foreign_keys", "OFF")
            .unwrap();
        store.conn.execute_batch(sql).unwrap();
        assert!(
            matches!(
                store.candidate_run_snapshot("owner", receipt.run_id()),
                Err(CandidateCreationError::CorruptStoredData)
            ),
            "{sql}"
        );
        assert!(
            matches!(
                store.create_candidate_run(&session, "owner", "key", &body, &host, 1000),
                Err(CandidateCreationError::CorruptStoredData)
            ),
            "{sql}"
        );
    }
}

#[test]
fn candidate_creation_receipt_insert_failure_rolls_back_the_whole_acceptance() {
    let (manifest, host, policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    store.conn.execute_batch("CREATE TRIGGER fail_candidate_receipt BEFORE INSERT ON command_receipts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = counts(&store);
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000),
        Err(CandidateCreationError::Sql(_))
    ));
    assert_eq!(counts(&store), before);
    assert!(TcpListener::bind(("127.0.0.1", policy.endpoint_port_range.start)).is_ok());
}

#[test]
fn candidate_creation_command_bounds_and_errors_do_not_echo_values() {
    let (manifest, host, policy) = fixture("fake");
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let body = command(&manifest);
    let padded = format!("{body}{}", " ".repeat(MAX_BYTES - body.len()));
    let receipt = store
        .create_candidate_run(&session, "owner", "key", &padded, &host, 1000)
        .unwrap();
    assert_eq!(receipt.version(), 1);
    assert!(matches!(
        store.create_candidate_run(&session, "owner", "key", &(padded + " "), &host, 1000),
        Err(CandidateCreationError::InvalidCommand)
    ));
    for (principal, key) in [
        ("".to_owned(), "key".to_owned()),
        ("owner".to_owned(), "".to_owned()),
        ("é".repeat(129), "key".to_owned()),
        ("owner".to_owned(), "é".repeat(129)),
    ] {
        assert!(matches!(
            store.create_candidate_run(&session, &principal, &key, &body, &host, 1000),
            Err(CandidateCreationError::InvalidCommand)
        ));
    }
    let error = store
        .create_candidate_run(
            &session,
            "owner",
            "key",
            r#"{"SECRET_SENTINEL":"secret://never-echo"}"#,
            &host,
            1000,
        )
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
    assert!(!format!("{error:?} {error}").contains("secret://never-echo"));
}

#[test]
fn candidate_creation_optional_domain_limits_and_high_port_survive_db_composition() {
    let (manifest, mut host, mut policy) = fixture("fake");
    // Upper boundary makes inclusive allocation arithmetic observable.
    let socket = TcpListener::bind(("127.0.0.1", 65535)).unwrap();
    policy.endpoint_port_range.start = 65535;
    policy.endpoint_port_range.end = 65535;
    policy.domains.get_mut("unified").unwrap().host_kv_limit = None;
    policy.domains.get_mut("unified").unwrap().parked_limit = None;
    host["resource_policy"]["endpoint_port_range"] = json!({"start":65535,"end":65535});
    let store = Store::open_in_memory().unwrap();
    let session = setup(&store, &policy);
    let resource = store.resource_policy("lab").unwrap().unwrap();
    let composed = compose_host(&host, &resource).unwrap();
    assert!(composed["resource_policy"]["domains"]["unified"]
        .get("host_kv_limit")
        .is_none());
    assert!(composed["resource_policy"]["domains"]["unified"]
        .get("parked_limit")
        .is_none());
    drop(socket);
    assert!(store
        .create_candidate_run(&session, "owner", "key", &command(&manifest), &host, 1000)
        .is_ok());
}
