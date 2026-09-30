//! SPEC §6.3 (W6): `delete deployment` follows verified cleanup on every instance of a
//! deployment (ADR 0013 §5). CPU-only store checks; no engine is involved.
use capyctl_store::{lifecycle::LifecycleError, Store};
use rusqlite::Connection;

fn fixture() -> (tempfile::TempDir, Store, Connection, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("delete.sqlite3");
    let store = Store::open(&path).unwrap();
    let writer = Connection::open(path).unwrap();
    writer.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    let id = ulid::Ulid::new().to_string();
    writer
        .execute_batch(&format!(
            r#"INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES('{id}','two','model','stopped','stopped',0,0,0,3,1,1);
            INSERT INTO effective_revisions VALUES('{id}',1,'{{"routes":["two"]}}','f');
            INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json) VALUES('{id}',1,2,'{{"hosts":null,"selector":{{}},"strategy":"spread","max_per_host":null}}');
            INSERT OR IGNORE INTO deployment_instances(deployment_id,instance_index) VALUES('{id}',0);
            INSERT INTO deployment_instances(deployment_id,instance_index,host_id,generation) VALUES('{id}',1,'host-b',3);
            INSERT INTO deployment_routes(route,deployment_id) VALUES('two','{id}');"#
        ))
        .unwrap();
    (dir, store, writer, id)
}

fn count(writer: &Connection, table: &str, id: &str) -> i64 {
    writer
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE deployment_id=?1"),
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

// T32: one instance of two still holds a runtime on a host (for example a
// disconnected one whose cleanup evidence has not arrived): the delete is
// refused and nothing is removed or released. After the instance's cleanup is
// recorded, the delete removes every instance row and the route.
#[test]
fn delete_waits_for_cleanup_on_every_instance() {
    let (_dir, store, writer, id) = fixture();
    let session = store.begin_coordinator_session().unwrap();
    writer
        .execute_batch(&format!(
            r#"INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state,instance_index) VALUES('b','{id}',1,'inc','managed','{{}}','[]','live',1);
            UPDATE deployment_instances SET observed_state='ready' WHERE deployment_id='{id}' AND instance_index=1;"#
        ))
        .unwrap();
    let delete = |key: &str, deadline: i64| {
        store.accept_delete_command(&session, "owner", &id, 1, key, 1000, deadline)
    };
    assert!(matches!(
        delete("early", 2000),
        Err(LifecycleError::RuntimeRetained)
    ));
    assert_eq!(count(&writer, "deployment_instances", &id), 2);
    assert_eq!(count(&writer, "deployment_routes", &id), 1);
    assert!(store.find_deployment_by_route("two").unwrap().is_some());

    // The runtime is gone but an operation is still open: still refused.
    writer
        .execute_batch(&format!(
            r#"UPDATE runtime_bindings SET state='released' WHERE id='b';
            UPDATE deployment_instances SET observed_state='stopped' WHERE deployment_id='{id}' AND instance_index=1;
            INSERT INTO operations(id,deployment_id,kind,state) VALUES('{op}','{id}','stop','running');"#,
            op = ulid::Ulid::new()
        ))
        .unwrap();
    assert!(matches!(
        delete("early", 2000),
        Err(LifecycleError::RuntimeRetained)
    ));
    writer
        .execute_batch(&format!(
            "UPDATE operations SET state='succeeded' WHERE deployment_id='{id}'"
        ))
        .unwrap();

    let receipt = delete("delete", 2000).unwrap();
    assert_eq!(
        (receipt.name.as_str(), receipt.routes.as_slice()),
        ("two", ["two".to_string()].as_slice())
    );
    assert_eq!(count(&writer, "deployment_instances", &id), 0);
    assert_eq!(count(&writer, "deployment_routes", &id), 0);
    assert!(store.find_deployment_by_route("two").unwrap().is_none());
    assert!(store.is_deleted(&id).unwrap());
    assert!(store.snapshot().unwrap().deployments.is_empty());
    // The released binding stays as history.
    assert_eq!(count(&writer, "runtime_bindings", &id), 1);

    // T09: an exact retry is the same receipt; the same key with another
    // request is a conflict; a new key finds nothing.
    assert_eq!(delete("delete", 2000).unwrap(), receipt);
    assert!(matches!(
        delete("delete", 3000),
        Err(LifecycleError::IdempotencyConflict)
    ));
    assert!(matches!(
        delete("again", 2000),
        Err(LifecycleError::NotFound)
    ));
}

// T32: a deleted deployment keeps no gate-closure record behind: closure
// reasons are per instance incarnation, and nothing of it remains to reopen.
#[test]
fn delete_leaves_no_dispatch_closure_behind() {
    let (_dir, store, writer, id) = fixture();
    let session = store.begin_coordinator_session().unwrap();
    writer
        .execute_batch(&format!(
            "INSERT INTO dispatch_closures(deployment_id,instance_index,generation,reason) VALUES('{id}',1,3,'engine_exit'),('{id}',0,1,'host_session');"
        ))
        .unwrap();
    store
        .accept_delete_command(&session, "owner", &id, 1, "delete", 1000, 2000)
        .unwrap();
    assert_eq!(count(&writer, "dispatch_closures", &id), 0);
}
