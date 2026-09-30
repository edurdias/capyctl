//! ADR 0013 §6: deployment status carries every instance and the `degraded`
//! condition. CPU-only store projection; no engine is involved.
use capyctl_store::Store;
use rusqlite::Connection;

fn fixture() -> (tempfile::TempDir, Store, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("instances.sqlite3");
    let store = Store::open(&path).unwrap();
    let writer = Connection::open(path).unwrap();
    writer.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    (dir, store, writer)
}

const VLLM_EXPOSED: &str = r#"{"host":{"name":"host-a"},"residency":"deep","profile":{"engine":"vllm","security":{"deep_park":"enabled"}},"engine_config":{"enable_sleep_mode":true}}"#;
const VLLM_OPTED_OUT: &str = r#"{"host":{"name":"host-b"},"residency":"deep","profile":{"engine":"vllm","security":{"deep_park":"disabled"}},"engine_config":{"enable_sleep_mode":false}}"#;

/// One READY instance of two is `ready` with the `degraded` condition; the
/// other instance is reported unplaced and stopped. Each instance carries the
/// development-control mark of the revision as resolved on its own host.
// T10 T21 T29
#[test]
fn status_lists_instances_and_marks_a_partially_ready_deployment_degraded() {
    let (_dir, store, writer) = fixture();
    writer
        .execute_batch(&format!(
            r#"INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES('d','d','model','ready','ready',1,1,0,4,1,1);
            INSERT INTO effective_revisions VALUES('d',1,'{VLLM_EXPOSED}','f');
            INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json) VALUES('d',1,2,'{{"hosts":null,"selector":{{}},"strategy":"spread","max_per_host":null}}');
            INSERT INTO host_effective_revisions(deployment_id,revision,host_id,outcome,effective_json,fingerprint) VALUES('d',1,'host-a','resolved','{VLLM_EXPOSED}','f'),('d',1,'host-b','resolved','{VLLM_OPTED_OUT}','g');
            INSERT INTO deployment_instances(deployment_id,instance_index) VALUES('d',1);
            UPDATE deployment_instances SET host_id='host-a',generation=4 WHERE deployment_id='d' AND instance_index=0;
            INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('b','d',1,'inc','managed','{{}}','[]','live');
            INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('d','{{"version":1,"phase":"ready","allocations":[["ram",1,0]],"devices":[]}}','d');"#
        ))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let deployment = &snapshot.deployments[0];
    assert_eq!(deployment.observed_state, "ready");
    assert_eq!(
        (deployment.desired_instances, deployment.ready_instances),
        (2, 1)
    );
    assert_eq!(deployment.conditions, vec!["degraded"]);
    let [zero, one] = deployment.instances.as_slice() else {
        panic!("two instances expected: {:?}", deployment.instances)
    };
    assert_eq!(
        (
            zero.index,
            zero.host_id.as_deref(),
            zero.generation.as_deref(),
            zero.observed_state.as_str()
        ),
        (0, Some("host-a"), Some("4"), "ready")
    );
    assert_eq!(zero.reservation_owner.as_deref(), Some("d"));
    assert!(zero.development_controls.is_exposed());
    assert_eq!(
        (
            one.index,
            one.host_id.as_deref(),
            one.observed_state.as_str(),
            one.lifecycle.as_str()
        ),
        (1, None, "stopped", "active")
    );
    // Unplaced: marked from the deployment's revision, never reported safe.
    assert!(one.development_controls.is_exposed());
    // Placed on the opted-out host, the instance carries that host's mark.
    writer
        .execute_batch("UPDATE deployment_instances SET host_id='host-b' WHERE deployment_id='d' AND instance_index=1;")
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(!snapshot.deployments[0].instances[1]
        .development_controls
        .is_exposed());
    // The operator stopped the second instance: nothing else is wanted.
    writer
        .execute_batch("UPDATE deployment_instances SET operator_stopped=1 WHERE deployment_id='d' AND instance_index=1;")
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(snapshot.deployments[0].conditions.is_empty());
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(
        json["deployments"][0]["instances"][1]["operator_stopped"],
        true
    );
}

/// A runtime recorded on an instance the single-instance lifecycle does not
/// realize is reported `uncertain`, never `stopped`.
// T32
#[test]
fn an_uninterpretable_instance_runtime_is_uncertain() {
    let (_dir, store, writer) = fixture();
    writer
        .execute_batch(
            r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('d','d','model','stopped',0,0,1,1);
            INSERT INTO deployment_instances(deployment_id,instance_index) VALUES('d',1);
            INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index) VALUES('deployment:d/instance:1','{"version":1,"phase":"ready","allocations":[["ram",1,0]],"devices":[]}','d',1);"#,
        )
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let instance = &snapshot.deployments[0].instances[1];
    assert_eq!(instance.observed_state, "uncertain");
    assert_eq!(
        instance.reservation_owner.as_deref(),
        Some("deployment:d/instance:1")
    );
    assert_eq!(snapshot.deployments[0].desired_instances, 2);
}

/// SPEC §6.4: status exposes the latest operation and its error, for the
/// deployment and for each instance: its id, kind, state, closed error code,
/// the recorded reason and a fixed operator hint. The reason is bounded and
/// redacted: one line, no engine log tail, no option values or secrets.
// T08 T29
#[test]
fn status_shows_the_latest_operation_with_its_reason_and_hint() {
    let (_dir, store, writer) = fixture();
    writer
        .execute_batch(&format!(
            r#"INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES('d','d','model','ready','stopped',0,0,0,5,1,1);
            INSERT INTO effective_revisions VALUES('d',1,'{VLLM_EXPOSED}','f');
            INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json) VALUES('d',1,2,'{{"hosts":null,"selector":{{}},"strategy":"spread","max_per_host":null}}');
            INSERT INTO deployment_instances(deployment_id,instance_index,generation) VALUES('d',1,5);
            UPDATE deployment_instances SET generation=4 WHERE deployment_id='d' AND instance_index=0;
            INSERT INTO operations(id,deployment_id,kind,state,error_code,accepted_at) VALUES
              ('o0','d','initialize','failed','launch_failed','2026-01-01T00:00:01.000Z'),
              ('o1','d','initialize','running','SECRET-CODE','2026-01-01T00:00:02.000Z');
            INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES
              ('o0','d',1,4,'s','activate','failed',1,'{{}}',0),
              ('o1','d',1,5,'s','activate','running',1,'{{}}',1);
            INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES
              ('j0',NULL,'o0','attempt_failed','deployment d: attempt failed: insufficient resources'),
              ('j1',NULL,'o0','launch_failed','deployment d: launch failed: coordinator service failed: engine launch failed: the engine exited before readiness with exit code 2; it rejected argument --moe-backend; log tail:
ValueError: invalid choice bogus-value SECRET'),
              ('j2',NULL,'o1','start_deferred','deployment d: reclaiming 1 parked instance(s) first');"#
        ))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let json = serde_json::to_value(&snapshot).unwrap();
    let text = json.to_string();
    assert!(!text.contains("SECRET"), "{text}");
    assert!(!text.contains("bogus-value"), "{text}");
    let deployment = &json["deployments"][0];
    // The deployment's latest operation is the most recently accepted one.
    assert_eq!(deployment["latest_operation"]["id"], "o1");
    assert_eq!(deployment["latest_operation"]["kind"], "initialize");
    assert_eq!(deployment["latest_operation"]["state"], "running");
    // A code that is not a closed category is not shown.
    assert!(deployment["latest_operation"]["error_code"].is_null());
    assert_eq!(
        deployment["latest_operation"]["reason"],
        "reclaiming 1 parked instance(s) first"
    );
    // Each instance shows its own latest operation.
    let zero = &deployment["instances"][0]["latest_operation"];
    assert_eq!(zero["id"], "o0");
    assert_eq!(zero["state"], "failed");
    assert_eq!(zero["error_code"], "launch_failed");
    assert_eq!(
        zero["reason"],
        "launch failed: engine launch failed: the engine exited before readiness with exit code 2; it rejected argument --moe-backend"
    );
    assert!(zero["hint"].as_str().unwrap().contains("engine_config"));
    assert_eq!(deployment["instances"][1]["latest_operation"]["id"], "o1");
    // A deployment with no operation shows none.
    writer
        .execute_batch(
            "DELETE FROM journal_entries; DELETE FROM lifecycle_runs; DELETE FROM operations;",
        )
        .unwrap();
    let json = serde_json::to_value(store.snapshot().unwrap()).unwrap();
    assert!(json["deployments"][0].get("latest_operation").is_none());
    assert!(json["deployments"][0]["instances"][0]
        .get("latest_operation")
        .is_none());
}

/// ADR 0013 §7: compaction moves an instance that holds no runtime into a free
/// lower index. The runs recorded at that index belong to the incarnations
/// that used it before; the instance now there never ran them. Status derives
/// an instance's state and latest operation from its own incarnation's runs
/// only, so a moved instance never reads as `failed` on someone else's launch.
// T08 T09 T29
#[test]
fn a_compacted_instance_does_not_inherit_the_history_of_its_index() {
    let (_dir, store, writer) = fixture();
    writer
        .execute_batch(&format!(
            r#"INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES('d','d','model','ready','stopped',0,0,0,7,1,1);
            INSERT INTO effective_revisions VALUES('d',1,'{VLLM_EXPOSED}','f');
            INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json) VALUES('d',1,2,'{{"hosts":null,"selector":{{}},"strategy":"spread","max_per_host":null}}');
            UPDATE deployment_instances SET generation=2 WHERE deployment_id='d' AND instance_index=0;
            INSERT INTO deployment_instances(deployment_id,instance_index,generation,desired_state,observed_state,admission_enabled) VALUES('d',1,7,'ready','stopped',0);
            INSERT INTO operations(id,deployment_id,kind,state,error_code,accepted_at) VALUES
              ('gone','d','initialize','failed','launch_failed','2026-01-01T00:00:01.000Z'),
              ('exit','d','ordinary_cleanup','succeeded',NULL,'2026-01-01T00:00:02.000Z');
            INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES
              ('gone','d',1,3,'s','activate','failed',1,'{{}}',1),
              ('exit','d',1,4,'s','stop','succeeded',1,'{{}}',1);
            INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES('system:engine_exit','scope','k','h','exit','{{}}');"#
        ))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let moved = &snapshot.deployments[0].instances[1];
    assert_eq!(moved.observed_state, "stopped");
    assert!(
        moved.latest_operation.is_none(),
        "{:?}",
        moved.latest_operation
    );
    // The same runs recorded against the instance's own incarnation do count.
    writer
        .execute_batch("UPDATE lifecycle_runs SET generation=7 WHERE operation_id='gone'; DELETE FROM lifecycle_runs WHERE operation_id='exit';")
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let own = &snapshot.deployments[0].instances[1];
    assert_eq!(own.observed_state, "failed");
    assert_eq!(own.latest_operation.as_ref().unwrap().id, "gone");
}
