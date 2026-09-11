use std::fs;
use std::os::unix::fs::PermissionsExt;

use mllm_domain::{DeploymentId, LifecycleState, OperationId};
use mllm_store::{AcceptDeployment, Store, StoreError};

fn req(name: &str, key: &str) -> AcceptDeployment {
    AcceptDeployment {
        id: DeploymentId::new(),
        name: name.to_string(),
        kind: "model".to_string(),
        route_model_id: Some("org/model".to_string()),
        desired_state: LifecycleState::Stopped,
        schema_version: 1,
        idempotency_key: key.to_string(),
        initial_operation_id: OperationId(format!("op-{}", ulid::Ulid::new())),
    }
}

#[test]
fn t08_id_returned_after_persistence_survives_new_client() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    let id = {
        let s = Store::open(&path).unwrap(); // "process 1"
        let accepted = s.accept_deployment(req("d1", "k1")).unwrap();
        accepted.deployment_id
    }; // store dropped
    let s2 = Store::open(&path).unwrap(); // "new client"
    let row = s2.get_deployment(&id.to_string()).unwrap().unwrap();
    assert_eq!(row.name, "d1");
    assert!(s2.latest_operation(&id.to_string()).unwrap().is_some());
}

#[test]
fn t09_retry_with_same_key_returns_same_deployment() {
    let s = Store::open_in_memory().unwrap();
    let a = s.accept_deployment(req("d1", "k1")).unwrap();
    let b = s.accept_deployment(req("d1", "k1")).unwrap(); // retry
    assert_eq!(a.deployment_id, b.deployment_id);
    assert_eq!(s.deployment_count().unwrap(), 1);
}

#[test]
fn same_key_different_content_is_conflict() {
    let s = Store::open_in_memory().unwrap();
    s.accept_deployment(req("d1", "k1")).unwrap();
    let mut other = req("d1", "k1");
    other.kind = "host".into(); // different payload, same key
    assert!(matches!(
        s.accept_deployment(other),
        Err(StoreError::IdempotencyConflict)
    ));
}

#[test]
fn store_file_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    Store::open(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(
        fs::metadata(dir.path()).unwrap().permissions().mode() & 0o077,
        0
    );
}
