use mllm_store::{snapshot::SnapshotError, Store};
use rusqlite::{params, Connection};

fn fixture() -> (tempfile::TempDir, Store, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.sqlite3");
    let store = Store::open(&path).unwrap();
    let writer = Connection::open(path).unwrap();
    writer.execute_batch(r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('d','public-name','managed','ready',1,0,1,1);
        INSERT INTO deployment_routes VALUES('public-route','d');
        INSERT INTO operations(id,deployment_id,kind,state,error_code,idempotency_key) VALUES('o','d','activate','running','SECRET-ERROR','SECRET-KEY');
        INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('b','d',1,'inc','managed','SECRET-BINDING','[]','uncertain');
        INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) VALUES('o','d',1,1,'session','activate','running',9007199254740993,'SECRET-PLAN');
        INSERT INTO lifecycle_steps VALUES('s','o',0,'d','b','session','armed','SECRET-STEP',NULL);
        INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('d','{"version":1,"phase":"cold","allocations":[["ram",9007199254740993,0]],"devices":[]}','d');
        INSERT INTO effective_revisions VALUES('d',1,'SECRET-CONFIG-/private/checkpoint','digest');
        INSERT INTO management_events(recorded_at_ms,kind,payload_json) VALUES(9223372036854775807,'fixture','{}');"#).unwrap();
    (dir, store, writer)
}

#[test]
fn sanitized_snapshot_preserves_wide_numbers_and_does_not_mutate() {
    let (_dir, store, writer) = fixture();
    let before: i64 = writer
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["api_version"], "1");
    assert_eq!(json["deployments"][0]["revision"], "1");
    assert_eq!(json["deployments"][0]["dispatch_enabled"], false);
    assert_eq!(json["runs"][0]["deadline_ms"], "9007199254740993");
    assert_eq!(
        json["reservations"][0]["allocations"][0]["bytes"],
        "9007199254740993"
    );
    assert_eq!(json["steps"][0]["state"], "armed");
    assert_eq!(json["bindings"][0]["state"], "uncertain");
    assert_eq!(snapshot.cursor.sequence, 1);
    assert!(!serde_json::to_string(&snapshot).unwrap().contains("SECRET"));
    assert!(!format!("{snapshot:?}").contains("SECRET"));
    assert_eq!(store.snapshot().unwrap(), snapshot);
    let after: i64 = writer
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn snapshot_to_subscription_replays_intervening_commit_and_pruned_high_water() {
    let (_dir, store, writer) = fixture();
    let snapshot = store.snapshot().unwrap();
    writer.execute_batch("BEGIN IMMEDIATE; UPDATE deployments SET revision=2; INSERT INTO management_events(recorded_at_ms,kind,payload_json) VALUES(9223372036854775807,'revision_changed','{}'); COMMIT;").unwrap();
    let page = store
        .events_after(Some(&snapshot.cursor.to_string()), 100)
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].cursor.sequence, 2);
    writer
        .execute_batch("DELETE FROM management_events; UPDATE event_meta SET retained_after=2;")
        .unwrap();
    assert_eq!(store.snapshot().unwrap().cursor.sequence, 2);
}

#[test]
fn concurrent_commits_never_split_revision_and_cursor() {
    let (_dir, store, writer) = fixture();
    let thread = std::thread::spawn(move || {
        for revision in 2..150 {
            writer.execute_batch("BEGIN IMMEDIATE").unwrap();
            writer
                .execute("UPDATE deployments SET revision=?1", [revision])
                .unwrap();
            writer.execute("INSERT INTO management_events(recorded_at_ms,kind,payload_json) VALUES(9223372036854775807,'revision_changed','{}')", []).unwrap();
            writer.execute_batch("COMMIT").unwrap();
        }
    });
    for _ in 0..150 {
        let snapshot = store.snapshot().unwrap();
        assert_eq!(
            snapshot.deployments[0].revision,
            snapshot.cursor.sequence.to_string()
        );
    }
    thread.join().unwrap();
    assert_eq!(store.snapshot().unwrap().cursor.sequence, 149);
}

#[test]
fn bounds_fail_instead_of_omitting_rows_or_allocating_unbounded_fields() {
    let (_dir, store, writer) = fixture();
    writer
        .execute("UPDATE deployments SET name=?1", ["x".repeat(16385)])
        .unwrap();
    assert!(matches!(store.snapshot(), Err(SnapshotError::TooLarge)));
    writer
        .execute("UPDATE deployments SET name='ok'", [])
        .unwrap();
    for n in 0..4097 {
        writer
            .execute(
                "INSERT INTO deployment_routes VALUES(?1,'d')",
                params![format!("route-{n}")],
            )
            .unwrap();
    }
    assert!(matches!(store.snapshot(), Err(SnapshotError::TooLarge)));
}

#[test]
fn retained_metadata_and_host_operations_are_not_silently_dropped() {
    let (_dir, store, writer) = fixture();
    writer.execute_batch(r#"
        UPDATE runtime_bindings SET identities_json='[{"role":"api","pid":123,"boot_id":"boot","start_ticks":18446744073709551615}]';
        INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('released','d',1,'old','managed','SECRET','[]','released');
        INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation) VALUES('d','o',1,1);
        INSERT INTO resource_grants VALUES('grant','d','o','SECRET-GRANT',1);
        UPDATE lifecycle_steps SET grant_id='grant';
        INSERT INTO lifecycle_evidence VALUES('s','SECRET-EVIDENCE',1);
        INSERT INTO owners VALUES('legacy','external','d');
        INSERT INTO reservations VALUES('legacy','ram',40,'ready','["gpu0"]');
        INSERT INTO domains VALUES('ram','system',80,'2026-09-15T00:00:00Z');
        INSERT INTO host_resource_policies VALUES('host',2,'SECRET-POLICY');
        INSERT INTO operations(id,deployment_id,kind,state) VALUES('host-op',NULL,'host_resource_policy_update','succeeded');
    "#).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.bindings.len(), 1);
    assert_eq!(
        snapshot.bindings[0].recorded_identities[0].start_ticks,
        "18446744073709551615"
    );
    assert_eq!(snapshot.operations[0].deployment_id, None);
    assert_eq!(snapshot.claims[0].operation_id, "o");
    assert_eq!(snapshot.grants[0].committed_epoch, "1");
    assert_eq!(snapshot.steps[0].evidence_epoch.as_deref(), Some("1"));
    assert_eq!(snapshot.legacy_reservations[0].bytes, "40");
    assert_eq!(snapshot.legacy_reservations[0].exclusive_devices, ["gpu0"]);
    assert_eq!(
        snapshot.observations[0].observed_bytes.as_deref(),
        Some("80")
    );
    assert_eq!(snapshot.host_policy_revisions[0].revision, "2");
    assert!(!serde_json::to_string(&snapshot).unwrap().contains("SECRET"));
}

#[test]
fn rollback_is_invisible_and_malformed_metadata_fails_closed() {
    let (_dir, store, writer) = fixture();
    let before = store.snapshot().unwrap();
    writer.execute_batch("BEGIN IMMEDIATE; UPDATE deployments SET revision=2; INSERT INTO management_events(recorded_at_ms,kind,payload_json) VALUES(9223372036854775807,'change','{}');").unwrap();
    assert_eq!(store.snapshot().unwrap(), before);
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(store.snapshot().unwrap(), before);
    writer
        .execute(
            "UPDATE runtime_bindings SET identities_json=?1",
            [r#"[{"role":"api","pid":1,"boot_id":"boot","start_ticks":1,"secret":"PRIVATE"}]"#],
        )
        .unwrap();
    assert!(matches!(store.snapshot(), Err(SnapshotError::CorruptData)));
    writer
        .execute("UPDATE runtime_bindings SET identities_json='[]'", [])
        .unwrap();
    writer
        .execute("UPDATE deployments SET admission_enabled=2", [])
        .unwrap();
    assert!(matches!(store.snapshot(), Err(SnapshotError::CorruptData)));
}

#[test]
fn aggregate_input_and_escaped_output_are_bounded_independently() {
    for escaped in [false, true] {
        let (_dir, store, writer) = fixture();
        let route = if escaped {
            "\u{0001}".repeat(15000)
        } else {
            "x".repeat(15000)
        };
        let count = if escaped { 110 } else { 290 };
        for n in 0..count {
            writer
                .execute(
                    "INSERT INTO deployment_routes VALUES(?1,'d')",
                    [format!("{n}{route}")],
                )
                .unwrap();
        }
        assert!(matches!(store.snapshot(), Err(SnapshotError::TooLarge)));
    }
}

/// Phase B follow-up (SPEC §§6.1, 6.4): status reports what is happening, not
/// only the stored evidence state. A queued start is `queued`, a sent one
/// `starting`, a stop in progress `stopping`, and a Ready engine whose dispatch is
/// closed for a readiness re-proof `reconciling`; an unresolved outcome stays
/// `uncertain`. W14's development controls and the v19 operator action are
/// untouched.
// T05 T08 T29 T33
#[test]
fn status_derives_queued_starting_stopping_and_reconciling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.sqlite3");
    let store = Store::open(&path).unwrap();
    let writer = Connection::open(path).unwrap();
    let deployment = |id: &str, observed: &str, dispatch: i64| {
        writer.execute(
            "INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version) VALUES(?1,?1,'model','ready',?2,1,?3,0,1,1)",
            params![id, observed, dispatch],
        ).unwrap();
        writer.execute(
            "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES(?1,?2,1,?1,'managed','{}','[]','reserved')",
            params![format!("b-{id}"), id],
        ).unwrap();
    };
    let run = |id: &str, action: &str, state: &str, step: &str| {
        writer.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'initialize','running')", params![format!("o-{id}-{action}"), id]).unwrap();
        writer.execute("INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) VALUES(?1,?2,1,1,'session',?3,?4,1,'{}')", params![format!("o-{id}-{action}"), id, action, state]).unwrap();
        writer
            .execute(
                "INSERT INTO lifecycle_steps VALUES(?1,?2,0,?3,?4,'session',?5,'{}',NULL)",
                params![
                    format!("s-{id}-{action}"),
                    format!("o-{id}-{action}"),
                    id,
                    format!("b-{id}"),
                    step
                ],
            )
            .unwrap();
    };
    deployment("queued", "stopped", 0);
    run("queued", "activate", "queued", "planned");
    deployment("starting", "stopped", 0);
    run("starting", "activate", "running", "armed");
    deployment("stopping", "ready", 0);
    run("stopping", "stop", "running", "armed");
    deployment("reconciling", "ready", 0);
    run("reconciling", "activate", "succeeded", "completed");
    deployment("serving", "ready", 1);
    run("serving", "activate", "succeeded", "completed");
    deployment("unknown", "stopped", 0);
    run("unknown", "activate", "uncertain", "uncertain");
    // Not coordinator-managed: no lifecycle run, the stored state stands.
    writer.execute("INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version) VALUES('f1','f1','model','ready','ready',1,0,0,1,1)", []).unwrap();

    let snapshot = store.snapshot().unwrap();
    let observed = |id: &str| {
        snapshot
            .deployments
            .iter()
            .find(|d| d.id == id)
            .unwrap()
            .observed_state
            .clone()
    };
    assert_eq!(observed("queued"), "queued");
    assert_eq!(observed("starting"), "starting");
    assert_eq!(observed("stopping"), "stopping");
    assert_eq!(observed("reconciling"), "reconciling");
    assert_eq!(observed("serving"), "ready");
    assert_eq!(observed("unknown"), "uncertain");
    assert_eq!(observed("f1"), "ready");
    // Additive fields keep their shape.
    let json = serde_json::to_value(&snapshot).unwrap();
    for deployment in json["deployments"].as_array().unwrap() {
        assert!(deployment.get("development_controls").is_some());
        assert!(deployment.get("operator_action").is_none());
    }
}
