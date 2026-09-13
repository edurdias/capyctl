use std::sync::Arc;

use mllm_adapters::fake::FakeEngine;
use mllm_controller::{
    RuntimeAction, RuntimeBinding, RuntimeBindings, RuntimeError, RuntimeOwnership,
};
use mllm_domain::resources::{Allocation, PhaseFootprint, RecipeFootprints, ResourcePhase};

fn footprint(phase: ResourcePhase) -> PhaseFootprint {
    PhaseFootprint {
        phase,
        allocations: vec![Allocation {
            domain: "gpu:0".into(),
            bytes: 1,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    }
}

fn recipe() -> RecipeFootprints {
    RecipeFootprints {
        cold: footprint(ResourcePhase::Cold),
        ready: footprint(ResourcePhase::Ready),
        parking: footprint(ResourcePhase::Parking),
        parked: footprint(ResourcePhase::Parked),
        wake: footprint(ResourcePhase::Wake),
    }
}

fn binding(id: &str, deployment: &str, revision: i64, port: u16) -> Arc<RuntimeBinding> {
    let engine = Arc::new(FakeEngine::new());
    Arc::new(RuntimeBinding {
        id: id.into(),
        deployment_id: deployment.into(),
        revision,
        incarnation: format!("inc-{id}"),
        qualification_id: "qualification-1".into(),
        recipe: recipe(),
        ownership: RuntimeOwnership::Managed,
        endpoint: format!("127.0.0.1:{port}"),
        credential_ref: format!("credential-{id}"),
        driver: engine.clone(),
        forward: engine,
    })
}

#[test]
fn deployment_bindings_are_distinct_and_survive_parking() {
    let bindings = RuntimeBindings::default();
    let first = binding("binding-a", "deployment-a", 1, 31001);
    let second = binding("binding-b", "deployment-b", 1, 31002);
    bindings.retain(first.clone()).unwrap();
    bindings.retain(second.clone()).unwrap();
    bindings.park("deployment-a", 1).unwrap();

    let parked = bindings.binding("deployment-a", 1).unwrap();
    assert_eq!(parked.id, "binding-a");
    assert_eq!(parked.endpoint, "127.0.0.1:31001");
    assert_eq!(parked.credential_ref, "credential-binding-a");
    assert_ne!(parked.id, second.id);
    assert_ne!(parked.endpoint, second.endpoint);
    assert_ne!(parked.credential_ref, second.credential_ref);
    assert!(matches!(
        bindings.binding("deployment-a", 2),
        Err(RuntimeError::StaleRevision)
    ));
}

#[test]
fn retained_binding_is_immutable_and_attached_control_is_unsupported() {
    let bindings = RuntimeBindings::default();
    let first = binding("binding-a", "deployment-a", 1, 31001);
    bindings.retain(first).unwrap();
    assert!(matches!(
        bindings.retain(binding("replacement", "deployment-a", 1, 31003)),
        Err(RuntimeError::Uncertain(_))
    ));

    let mut attached = binding("attached", "deployment-attached", 1, 31004);
    Arc::get_mut(&mut attached).unwrap().ownership = RuntimeOwnership::Attached;
    bindings.retain(attached).unwrap();
    for action in [
        RuntimeAction::Initialize,
        RuntimeAction::Drain,
        RuntimeAction::Park,
        RuntimeAction::Restore,
        RuntimeAction::Stop,
    ] {
        assert!(matches!(
            bindings.control("deployment-attached", 1, action),
            Err(RuntimeError::Unsupported)
        ));
    }
    assert!(bindings
        .control("deployment-attached", 1, RuntimeAction::Probe)
        .is_ok());
    assert!(bindings
        .control("deployment-attached", 1, RuntimeAction::Inspect)
        .is_ok());
}
