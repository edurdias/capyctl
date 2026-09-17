use super::*;
use crate::lifecycle::BindingDto;
use crate::Store;
use mllm_config::effective::{resolve_effective, Residency};
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
        .accept_qualified_start(&session, &fence, 100, 100_100)
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
