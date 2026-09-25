//! Website spec, Landing page 1: the hero output "must match the real CLI
//! format". `mllm list deployments` renders the snapshot's `deployments`
//! array; every key the site's fixture uses must exist in a real one.

use mllm_store::Store;
use rusqlite::Connection;
use serde_json::Value;

fn keys_subset(fixture: &Value, real: &Value, path: &str) {
    match (fixture, real) {
        (Value::Object(f), Value::Object(r)) => {
            for (k, v) in f {
                let rv = r
                    .get(k)
                    .unwrap_or_else(|| panic!("{path}.{k} is not in the real snapshot"));
                keys_subset(v, rv, &format!("{path}.{k}"));
            }
        }
        (Value::Array(f), Value::Array(r)) if !f.is_empty() => {
            let r0 = r
                .first()
                .unwrap_or_else(|| panic!("{path}[] is empty in the real snapshot"));
            for v in f {
                keys_subset(v, r0, &format!("{path}[]"));
            }
        }
        _ => {}
    }
}

#[test]
fn hero_fixture_uses_only_real_snapshot_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hero.sqlite3");
    let store = Store::open(&path).unwrap();
    Connection::open(&path).unwrap().execute_batch(
        "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('d','coding-small','model','ready',1,0,1,1);
         INSERT INTO deployment_instances(deployment_id,instance_index,host_id,generation) VALUES('d',1,'gpu-box',1);",
    ).unwrap();
    let real = serde_json::to_value(store.snapshot().unwrap()).unwrap()["deployments"].clone();
    let fixture: Value =
        serde_json::from_str(include_str!("../../../site/src/data/hero-deployments.json")).unwrap();
    assert!(fixture.as_array().is_some_and(|a| a.len() == 3));
    keys_subset(&fixture, &real, "deployments");
}
