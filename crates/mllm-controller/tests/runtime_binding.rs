use std::sync::Arc;

use mllm_adapters::fake::FakeEngine;
use mllm_adapters::traits::RenderedCommand;
use mllm_controller::{
    DurableRuntimeSupervisor, RuntimeAction, RuntimeBinding, RuntimeBindings, RuntimeError,
    RuntimeOwnership,
};
use mllm_domain::resources::{Allocation, PhaseFootprint, RecipeFootprints, ResourcePhase};
use mllm_domain::{DeploymentId, LifecycleState, OperationId};
use mllm_launchers::DurableSpawnOutcome;
use mllm_store::lifecycle::{DeploymentFence, ReserveBinding};
use mllm_store::{AcceptDeployment, Store};

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

#[test]
fn durable_spawn_attempt_survives_supervisor_recreation() {
    let store = Store::open_in_memory().unwrap();
    let deployment = DeploymentId::new();
    store
        .accept_deployment(AcceptDeployment {
            id: deployment,
            name: "durable-supervisor".into(),
            kind: "model".into(),
            route_model_id: None,
            desired_state: LifecycleState::Stopped,
            schema_version: 1,
            idempotency_key: "durable-supervisor".into(),
            initial_operation_id: OperationId("durable-supervisor-operation".into()),
        })
        .unwrap();
    let deployment_id = deployment.to_string();
    let fence = DeploymentFence {
        deployment_id: deployment_id.clone(),
        revision: 1,
        generation: 1,
    };
    let session = store.begin_coordinator_session().unwrap();
    store
        .reserve_runtime_binding(
            &session,
            &ReserveBinding {
                id: "durable-binding".into(),
                fence: fence.clone(),
                incarnation: "durable-incarnation".into(),
                qualification_id: "qualification".into(),
                ownership: "managed".into(),
                endpoint_host: "127.0.0.1".into(),
                endpoint_port: 31011,
                credential_ref: "credential-reference".into(),
                binding_payload: "recipe-reference".into(),
            },
        )
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("initialized");
    let command = RenderedCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("touch '{}'", marker.display()),
        ],
        env: Default::default(),
    };
    let first = DurableRuntimeSupervisor::new(&store, &session)
        .spawn(&fence, "durable-binding", &command)
        .unwrap();
    assert!(matches!(
        first,
        DurableSpawnOutcome::Uncertain {
            initialization_acknowledged: true,
            ..
        }
    ));
    let second =
        DurableRuntimeSupervisor::new(&store, &session).spawn(&fence, "durable-binding", &command);
    assert!(matches!(second, Err(RuntimeError::Uncertain(_))));
    let retained = store.runtime_binding(&deployment_id).unwrap().unwrap();
    assert_eq!(retained.state, "uncertain");
    assert_eq!(retained.endpoint, "127.0.0.1:31011");
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
        RuntimeAction::Probe,
        RuntimeAction::Inspect,
    ] {
        assert!(matches!(
            bindings.control("deployment-attached", 1, action),
            Err(RuntimeError::Unsupported)
        ));
    }
}
