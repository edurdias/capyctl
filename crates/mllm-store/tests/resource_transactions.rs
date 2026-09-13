use mllm_domain::{DeploymentId, LifecycleState, OperationId};
use mllm_domain::resources::*;
use mllm_scheduler::residency::{AdmissionContext, ResourceError};
use mllm_store::AcceptDeployment;
use mllm_store::resource_ledger::{GrantReceipt, GrantRequest, ResourceStoreError};
use mllm_store::Store;

fn deployment(store: &Store, name: &str) -> String {
    let id = DeploymentId::new();
    store.accept_deployment(AcceptDeployment {
        id, name: name.into(), kind: "model".into(), route_model_id: None,
        desired_state: LifecycleState::Stopped, schema_version: 1,
        idempotency_key: name.into(), initial_operation_id: OperationId(format!("op-{name}")),
    }).unwrap();
    id.to_string()
}

fn request(deployment: &str, name: &str, bytes: i64) -> GrantRequest {
    GrantRequest { id: format!("grant-{name}"), deployment_id: deployment.into(),
        operation_id: format!("op-{name}"), revision: 1, generation: 1, expected_epoch: 0,
        next: PhaseFootprint { phase: ResourcePhase::Cold,
            allocations: vec![Allocation { domain: "system".into(), bytes, host_kv_bytes: 0 }],
            devices: vec![] } }
}

fn observations() -> [MemoryObservation; 1] {
    [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 128, sampled_at_ms: 100 }]
}
fn limits() -> [MemoryLimit; 1] {
    [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }]
}

#[test]
fn new_store_has_empty_epoch_zero() {
    let store = Store::open_in_memory().unwrap();
    let snapshot = store.resource_snapshot().unwrap();
    assert_eq!(snapshot.epoch, 0);
    assert!(snapshot.owners.is_empty());
}

#[test]
fn grants_are_atomic_and_retries_are_not_dispatch_authority() {
    let store = Store::open_in_memory().unwrap();
    let a = deployment(&store, "a");
    let b = deployment(&store, "b");
    let obs = observations();
    let bounds = limits();
    let context = AdmissionContext::new(&obs, &bounds, 101, 60, 4);
    let first = request(&a, "a", 60);
    assert_eq!(store.reserve_increase(&first, context).unwrap(), GrantReceipt::New { epoch: 1 });
    assert_eq!(store.reserve_increase(&first, context).unwrap(), GrantReceipt::Recorded { epoch: 1 });
    let mut changed = first.clone();
    changed.next.allocations[0].bytes = 61;
    assert!(matches!(store.reserve_increase(&changed, context), Err(ResourceStoreError::Conflict)));
    let mut second = request(&b, "b", 60);
    assert!(matches!(store.reserve_increase(&second, context), Err(ResourceStoreError::Conflict)));
    second.expected_epoch = 1;
    assert!(matches!(store.reserve_increase(&second, context),
        Err(ResourceStoreError::Admission(ResourceError::Insufficient))));
    let snapshot = store.resource_snapshot().unwrap();
    assert_eq!(snapshot.epoch, 1);
    assert_eq!(snapshot.owners.len(), 1);
    assert_eq!(snapshot.owners[&a], first.next);
}
