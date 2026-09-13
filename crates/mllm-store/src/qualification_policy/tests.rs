use super::{QualificationImportError, QualificationPolicyState};
use crate::Store;
use mllm_config::effective::{HostPolicy, PortRange, QualificationPolicy, QueuePolicy, Sharing};
use rusqlite::params;
use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

fn policy(revision: i64) -> QualificationPolicy {
    QualificationPolicy {
        revision,
        allow_qualification_runs: true,
        allow_experimental_controls: false,
        allowed_manifest_digests: vec!["11".repeat(32), "aa".repeat(32)],
        max_run_duration_ms: 60_000,
        max_cleanup_duration_ms: 30_000,
        max_cases: 4,
        max_requests: 8,
        max_request_body_bytes: 4096,
        max_input_tokens_per_request: 1024,
        max_output_tokens_per_request: 512,
    }
}

fn host(revision: Option<i64>) -> HostPolicy {
    HostPolicy {
        name: "host-a".into(),
        hardware_fingerprint: "hw-a".into(),
        environment_fingerprint: "env-a".into(),
        domains: BTreeMap::new(),
        devices: BTreeMap::new(),
        max_parked: 1,
        observation_ttl_ms: 1000,
        device_sharing: Sharing::Exclusive,
        endpoint_port_range: PortRange {
            start: 8000,
            end: 8010,
        },
        planner_max_states: 10,
        queue: QueuePolicy {
            max_pending_per_deployment: 1,
            max_pending_total: 1,
            max_buffered_bytes_total: 1024,
            request_deadline_ms: 1000,
            admission_window_ms: 1000,
        },
        qualification_policy: revision.map(policy),
    }
}

fn current(store: &Store) -> (i64, String) {
    store
        .conn
        .query_row(
            "SELECT revision,policy_json FROM host_qualification_policies WHERE host_id='host-a'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

#[test]
fn absent_policy_without_row_is_an_unconfigured_noop() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let result = store
        .import_qualification_policy(&session, &host(None))
        .unwrap();
    assert_eq!(
        (result.state, result.revision, result.changed),
        (QualificationPolicyState::Unconfigured, None, false)
    );
    assert!(store
        .conn
        .query_row("SELECT 1 FROM host_qualification_policies", [], |_| Ok(()))
        .is_err());
    assert_eq!(store.events_after(None, 10).unwrap().events.len(), 1);
}

#[test]
fn import_noop_update_remove_and_readd_follow_revision_protocol() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let imported = store
        .import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    assert_eq!(
        (imported.state, imported.revision, imported.changed),
        (QualificationPolicyState::Configured, Some(1), true)
    );
    assert!(
        !store
            .import_qualification_policy(&session, &host(Some(1)))
            .unwrap()
            .changed
    );
    assert!(
        store
            .import_qualification_policy(&session, &host(Some(2)))
            .unwrap()
            .changed
    );
    let removed = store
        .import_qualification_policy(&session, &host(None))
        .unwrap();
    assert_eq!(
        (removed.state, removed.revision, removed.changed),
        (QualificationPolicyState::Removed, Some(2), true)
    );
    assert!(
        !store
            .import_qualification_policy(&session, &host(None))
            .unwrap()
            .changed
    );
    assert!(matches!(
        store.import_qualification_policy(&session, &host(Some(2))),
        Err(QualificationImportError::Conflict)
    ));
    let readded = store
        .import_qualification_policy(&session, &host(Some(3)))
        .unwrap();
    assert_eq!(
        (readded.state, readded.revision, readded.changed),
        (QualificationPolicyState::Configured, Some(3), true)
    );
    let kinds: Vec<_> = store
        .events_after(None, 10)
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "coordinator_session_started",
            "host_qualification_policy_changed",
            "host_qualification_policy_changed",
            "host_qualification_policy_changed",
            "host_qualification_policy_changed"
        ]
    );
}

#[test]
fn same_revision_changed_identity_contents_or_fingerprints_conflict() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    let mut changed = host(Some(1));
    changed
        .qualification_policy
        .as_mut()
        .unwrap()
        .allow_experimental_controls = true;
    assert!(matches!(
        store.import_qualification_policy(&session, &changed),
        Err(QualificationImportError::Conflict)
    ));
    changed = host(Some(1));
    changed.hardware_fingerprint = "hw-b".into();
    assert!(matches!(
        store.import_qualification_policy(&session, &changed),
        Err(QualificationImportError::Conflict)
    ));
    changed = host(Some(1));
    changed.environment_fingerprint = "env-b".into();
    assert!(matches!(
        store.import_qualification_policy(&session, &changed),
        Err(QualificationImportError::Conflict)
    ));
}

#[test]
fn stale_gapped_and_overflowing_revisions_conflict_but_max_can_be_removed() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    for revision in [3, 4] {
        assert!(matches!(
            store.import_qualification_policy(&session, &host(Some(revision))),
            Err(QualificationImportError::Conflict)
        ));
    }
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let max = r#"{"version":1,"host_id":"host-a","revision":9223372036854775807,"state":{"kind":"configured","hardware_fingerprint":"hw-a","environment_fingerprint":"env-a","policy":{"allow_qualification_runs":true,"allow_experimental_controls":false,"allowed_manifest_digests":[],"max_run_duration_ms":60000,"max_cleanup_duration_ms":30000,"max_cases":4,"max_requests":8,"max_request_body_bytes":4096,"max_input_tokens_per_request":1024,"max_output_tokens_per_request":512}}}"#;
    store
        .conn
        .execute(
            "INSERT INTO host_qualification_policies VALUES('host-a',?1,?2)",
            params![i64::MAX, max],
        )
        .unwrap();
    assert_eq!(
        store
            .import_qualification_policy(&session, &host(None))
            .unwrap()
            .state,
        QualificationPolicyState::Removed
    );
    assert!(matches!(
        store.import_qualification_policy(&session, &host(Some(i64::MAX))),
        Err(QualificationImportError::Conflict)
    ));
}

#[test]
fn first_import_must_be_revision_one_and_host_identity_cannot_drift() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.import_qualification_policy(&session, &host(Some(2))),
        Err(QualificationImportError::Conflict)
    ));
    store
        .import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    let mut drifted = host(Some(1));
    drifted.name = "host-b".into();
    assert!(matches!(
        store.import_qualification_policy(&session, &drifted),
        Err(QualificationImportError::Conflict)
    ));
}

#[test]
fn invalid_constructed_input_and_unsorted_digests_are_rejected() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let mut invalid = host(Some(1));
    invalid.qualification_policy.as_mut().unwrap().max_cases = 0;
    assert!(matches!(
        store.import_qualification_policy(&session, &invalid),
        Err(QualificationImportError::Invalid)
    ));
    let mut unsorted = host(Some(1));
    unsorted
        .qualification_policy
        .as_mut()
        .unwrap()
        .allowed_manifest_digests
        .reverse();
    assert!(matches!(
        store.import_qualification_policy(&session, &unsorted),
        Err(QualificationImportError::Invalid)
    ));
}

#[test]
fn configured_status_preserves_both_permissions_independently() {
    for (qualification, experimental) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let store = Store::open_in_memory().unwrap();
        let session = store.begin_coordinator_session().unwrap();
        let mut input = host(Some(1));
        let policy = input.qualification_policy.as_mut().unwrap();
        policy.allow_qualification_runs = qualification;
        policy.allow_experimental_controls = experimental;
        let result = store.import_qualification_policy(&session, &input).unwrap();
        assert_eq!(result.state, QualificationPolicyState::Configured);
        let stored: serde_json::Value = serde_json::from_str(&current(&store).1).unwrap();
        assert_eq!(
            stored["state"]["policy"]["allow_qualification_runs"],
            qualification
        );
        assert_eq!(
            stored["state"]["policy"]["allow_experimental_controls"],
            experimental
        );
        assert!(
            !store
                .import_qualification_policy(&session, &input)
                .unwrap()
                .changed
        );
    }
}

#[test]
fn stale_session_is_rejected_even_for_noop() {
    let store = Store::open_in_memory().unwrap();
    let stale = store.begin_coordinator_session().unwrap();
    let _current = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.import_qualification_policy(&stale, &host(None)),
        Err(QualificationImportError::StaleSession)
    ));
}

#[test]
fn stored_json_is_strict_bounded_and_bound_to_columns() {
    let cases = [
        (
            1,
            r#"{"version":2,"host_id":"host-a","revision":1,"state":{"kind":"removed"}}"#
                .to_owned(),
        ),
        (
            1,
            r#"{"version":1,"host_id":"host-a","revision":1,"state":{"kind":"removed"},"extra":1}"#
                .to_owned(),
        ),
        (
            1,
            r#"{"version":1,"host_id":"other","revision":1,"state":{"kind":"removed"}}"#.to_owned(),
        ),
        (
            2,
            r#"{"version":1,"host_id":"host-a","revision":1,"state":{"kind":"removed"}}"#
                .to_owned(),
        ),
        (1, "x".repeat((1 << 20) + 1)),
    ];
    for (revision, json) in cases {
        let store = Store::open_in_memory().unwrap();
        let session = store.begin_coordinator_session().unwrap();
        store.conn.execute("INSERT INTO host_qualification_policies(host_id,revision,policy_json) VALUES('host-a',?1,?2)", params![revision,json]).unwrap();
        assert!(matches!(
            store.import_qualification_policy(&session, &host(None)),
            Err(QualificationImportError::CorruptStoredPolicy)
        ));
        assert_eq!(current(&store), (revision, json));
    }
}

#[test]
fn configured_stored_body_is_revalidated() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let bad = r#"{"version":1,"host_id":"host-a","revision":1,"state":{"kind":"configured","hardware_fingerprint":"hw-a","environment_fingerprint":"env-a","policy":{"allow_qualification_runs":true,"allow_experimental_controls":false,"allowed_manifest_digests":[],"max_run_duration_ms":60000,"max_cleanup_duration_ms":30000,"max_cases":0,"max_requests":8,"max_request_body_bytes":4096,"max_input_tokens_per_request":1024,"max_output_tokens_per_request":512}}}"#;
    store
        .conn
        .execute(
            "INSERT INTO host_qualification_policies VALUES('host-a',1,?1)",
            [bad],
        )
        .unwrap();
    assert!(matches!(
        store.import_qualification_policy(&session, &host(Some(2))),
        Err(QualificationImportError::CorruptStoredPolicy)
    ));
}

#[test]
fn change_events_are_ordered_versioned_and_redacted() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    store
        .import_qualification_policy(&session, &host(Some(2)))
        .unwrap();
    store
        .import_qualification_policy(&session, &host(None))
        .unwrap();
    store
        .import_qualification_policy(&session, &host(Some(3)))
        .unwrap();
    let events = store.events_after(None, 10).unwrap().events;
    assert_eq!(
        events[1].payload_json,
        r#"{"version":"1","change_kind":"imported","previous_revision":null,"current_revision":1,"session_epoch":1}"#
    );
    assert_eq!(
        events[2].payload_json,
        r#"{"version":"1","change_kind":"updated","previous_revision":1,"current_revision":2,"session_epoch":1}"#
    );
    assert_eq!(
        events[3].payload_json,
        r#"{"version":"1","change_kind":"removed","previous_revision":2,"current_revision":2,"session_epoch":1}"#
    );
    assert_eq!(
        events[4].payload_json,
        r#"{"version":"1","change_kind":"readded","previous_revision":2,"current_revision":3,"session_epoch":1}"#
    );
    for event in &events[1..] {
        assert_eq!((&event.deployment_id, &event.operation_id), (&None, &None));
        assert!(!event.payload_json.contains("host-a"));
        assert!(!event.payload_json.contains("fingerprint"));
        assert!(!event.payload_json.contains("1111"));
    }
}

#[test]
fn event_failure_rolls_back_policy_mutation() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store.conn.execute_batch("CREATE TRIGGER reject_policy_event BEFORE INSERT ON management_events WHEN NEW.kind='host_qualification_policy_changed' BEGIN SELECT RAISE(ABORT,'reject'); END;").unwrap();
    assert!(store
        .import_qualification_policy(&session, &host(Some(1)))
        .is_err());
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM host_qualification_policies",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn removal_preserves_run_authority_accounting_and_cleanup_state() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    store.conn.execute_batch(r#"
        INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version)
          VALUES('d','d','model','stopped',0,0,1,1);
        INSERT INTO operations(id,deployment_id,kind,state) VALUES('op','d','qualification','running');
        INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state)
          VALUES('b','d',1,'inc','managed','{}','{}','live');
        INSERT INTO qualification_runs(id,host_id,deployment_id,revision,binding_id,incarnation,operation_id,principal_id,recipe_digest,authorization_json,state,deadline_ms,requests_used,cleanup_state)
          VALUES('run','host-a','d',1,'b','inc','op','principal','recipe','{"frozen":true}','running',9999,7,'retained');
        INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition)
          VALUES('lease','d',1,1,'session','uncertain');
        INSERT INTO owners(id,kind,deployment_id) VALUES('owner','model','d');
        INSERT INTO reservations(owner_id,domain_id,bytes,phase) VALUES('owner','system',64,'qualification');
        INSERT INTO resource_grants(id,deployment_id,operation_id,request_json,committed_epoch)
          VALUES('grant','d','op','{"bounded":true}',1);
    "#).unwrap();
    store
        .import_qualification_policy(&session, &host(None))
        .unwrap();
    let run: (String, i64, String) = store.conn.query_row(
        "SELECT authorization_json,requests_used,cleanup_state FROM qualification_runs WHERE id='run'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(run, (r#"{"frozen":true}"#.into(), 7, "retained".into()));
    for table in ["request_leases", "reservations", "resource_grants"] {
        let count: i64 = store
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1, "{table}");
    }
}

#[test]
fn concurrent_same_base_updates_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let seed = Store::open(&path).unwrap();
    let session = seed.begin_coordinator_session().unwrap();
    seed.import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    drop(seed);
    let stores = [Store::open(&path).unwrap(), Store::open(&path).unwrap()];
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = stores
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let barrier = Arc::clone(&barrier);
            let session = session.clone();
            std::thread::spawn(move || {
                let mut replacement = host(Some(2));
                replacement
                    .qualification_policy
                    .as_mut()
                    .unwrap()
                    .allow_experimental_controls = index == 0;
                barrier.wait();
                store.import_qualification_policy(&session, &replacement)
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(QualificationImportError::Conflict)))
            .count(),
        1
    );
    let verify = Store::open(&path).unwrap();
    assert_eq!(current(&verify).0, 2);
}

#[test]
fn concurrent_identical_successor_imports_are_mutation_then_unchanged_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let seed = Store::open(&path).unwrap();
    let session = seed.begin_coordinator_session().unwrap();
    seed.import_qualification_policy(&session, &host(Some(1)))
        .unwrap();
    drop(seed);
    let stores = [Store::open(&path).unwrap(), Store::open(&path).unwrap()];
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = stores
        .into_iter()
        .map(|store| {
            let barrier = Arc::clone(&barrier);
            let session = session.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.import_qualification_policy(&session, &host(Some(2)))
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.changed).count(), 1);
    assert_eq!(results.iter().filter(|result| !result.changed).count(), 1);
    assert!(results.iter().all(|result| {
        result.state == QualificationPolicyState::Configured && result.revision == Some(2)
    }));
    let verify = Store::open(&path).unwrap();
    let policy_events = verify
        .events_after(None, 10)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == "host_qualification_policy_changed")
        .count();
    assert_eq!(policy_events, 2);
}
