use super::*;
use crate::lifecycle::BindingDto;
use crate::Store;
use mllm_config::effective::{resolve_effective, Residency};
use mllm_domain::completion::ProcessIdentity;
use mllm_domain::resources::MemoryObservation;
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    (value["deployment"].clone(), value["host"].clone())
}

/// ADR 0011 decision 1: a parking deployment is identified by its recipe and host,
/// exactly as a restart-only one is. It used to require a qualification catalog
/// entry, which no real engine could produce and which nothing read when parking.
#[test]
fn a_parking_deployment_gets_a_declared_identity() {
    let (config, mut host) = fixture();
    // `deep` does not need a distinct domain (ADR 0010 decision 5 only refuses
    // `host_backed` on a unified one), but a distinct domain keeps this fixture
    // unambiguous about which tier is under test.
    host["resource_policy"]["domains"]["unified"]["memory"] = json!("distinct");
    let effective = resolve_effective(&config, &host).unwrap();
    assert_eq!(effective.residency, Residency::Deep);

    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let observations = vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 60 << 30,
        sampled_at_ms: 1,
    }];
    store
        .import_resource_policy(&session, &effective.host, &observations, 1)
        .unwrap();
    let body = json!({"config": config}).to_string();
    let receipt = store
        .create_stopped_managed_configuration(&session, "principal", "key", &body, &host, 10)
        .unwrap();
    let fence = DeploymentFence {
        deployment_id: receipt.deployment_id.clone(),
        revision: receipt.revision,
        generation: receipt.generation,
    };

    // ADR 0011: there is no catalog to consult. A parking deployment gets its
    // binding from the ordinary path alone.
    store
        .accept_start(&session, &fence, 100, 100_100)
        .expect("a parking deployment must get a binding");

    let binding_json: String = store
        .conn
        .query_row(
            "SELECT binding_json FROM runtime_bindings WHERE deployment_id=?1",
            [&receipt.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let binding: BindingDto = decode(&binding_json).unwrap();
    assert!(
        binding.identity_id.starts_with("declared:"),
        "expected a declared identity, got {}",
        binding.identity_id
    );
    // `DeclaredBindingV1::kind` is `&'static str`, so it is only ever serialized,
    // never deserialized back through the crate's `decode` helper (which requires
    // `DeserializeOwned`); read the payload as a `Value` instead to check the
    // residency it recorded.
    let descriptor: Value = serde_json::from_str(&binding.payload).unwrap();
    assert_eq!(descriptor["residency"], "deep");
}

pub(super) fn identity(role: &str, pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: role.into(),
        pid,
        boot_id: "boot-1".into(),
        start_ticks: 1,
    }
}

/// An ordinary managed deployment, accepted and armed, ready for its owned
/// launch to be recorded. Mirrors `a_parking_deployment_gets_a_declared_identity`'s
/// setup one step further: through `arm_step` and `initialize_execution`.
pub(super) fn armed_ordinary() -> (Store, CoordinatorSession, DeploymentFence, StepExecutionContext) {
    let (config, host) = fixture();
    let effective = resolve_effective(&config, &host).unwrap();
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let observations = vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 60 << 30,
        sampled_at_ms: 1,
    }];
    store
        .import_resource_policy(&session, &effective.host, &observations, 1)
        .unwrap();
    let body = json!({"config": config}).to_string();
    let receipt = store
        .create_stopped_managed_configuration(&session, "principal", "key", &body, &host, 10)
        .unwrap();
    let fence = DeploymentFence {
        deployment_id: receipt.deployment_id.clone(),
        revision: receipt.revision,
        generation: receipt.generation,
    };
    let accepted = store.accept_start(&session, &fence, 100, 100_100).unwrap();
    let policy = store
        .resource_policy(&effective.host.name)
        .unwrap()
        .unwrap();
    let limits: Vec<_> = policy
        .controls
        .domains
        .iter()
        .map(|(domain, d)| MemoryLimit {
            domain: domain.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    let context = AdmissionContext::new(
        &observations,
        &limits,
        100,
        policy.controls.observation_ttl_ms,
        policy.controls.max_parked as usize,
    );
    store.arm_step(&session, &accepted.step_id, context).unwrap();
    let execution = store
        .initialize_execution(&session, &accepted.step_id)
        .unwrap();
    (store, session, fence, execution)
}

/// Spec §3: the durable launcher records the API identity before the engine runs.
/// Recording the completed launch must accept that, and only that, prior content.
#[test]
fn record_launch_accepts_the_api_identity_the_association_wrote() {
    let (store, session, fence, execution) = armed_ordinary();
    let api = identity("api", 10);
    let worker0 = identity("worker-0", 11);
    store
        .record_api_identity(&session, &fence, &execution.binding_id, &api)
        .unwrap();
    let receipt = OwnedLaunchReceipt {
        binding_id: execution.binding_id.clone(),
        incarnation: execution.incarnation.clone(),
        identities: vec![api, worker0],
        observed_at_ms: execution.issued_at_ms,
        receipt: "vllm ready".into(),
    };
    store
        .record_owned_launch(&session, &execution.token.step_id, &receipt, execution.issued_at_ms)
        .unwrap();
}

/// A binding holding a different identity than the receipt's API process is refused.
#[test]
fn record_launch_refuses_a_mismatched_prior_identity() {
    let (store, session, fence, execution) = armed_ordinary();
    store
        .record_api_identity(&session, &fence, &execution.binding_id, &identity("api", 999))
        .unwrap();
    let receipt = OwnedLaunchReceipt {
        binding_id: execution.binding_id.clone(),
        incarnation: execution.incarnation.clone(),
        identities: vec![identity("api", 7), identity("worker-0", 8)],
        observed_at_ms: execution.issued_at_ms,
        receipt: "vllm ready".into(),
    };
    assert!(matches!(
        store.record_owned_launch(&session, &execution.token.step_id, &receipt, execution.issued_at_ms),
        Err(LifecycleError::Conflict)
    ));
}

/// Spec §4: a tensor-parallel launch has several workers. The store accepts api plus
/// worker-0..worker-N and still refuses a set without a worker.
#[test]
fn canonical_members_accepts_many_workers_and_refuses_none() {
    let api = identity("api", 10);
    let w0 = identity("worker-0", 11);
    let w1 = identity("worker-1", 12);
    assert!(canonical_members(&[api.clone(), w0.clone(), w1.clone()]).is_ok());
    assert!(canonical_members(std::slice::from_ref(&api)).is_err());
    assert!(
        canonical_members(&[api, identity("worker-1", 12)]).is_err(),
        "workers are contiguous from 0"
    );
}
