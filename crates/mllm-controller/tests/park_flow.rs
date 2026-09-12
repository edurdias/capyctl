//! Park/restore flow (F1 design §3/§7): ambiguous park reconciles (never
//! blind-repeats a collective — T20), park failure falls back to verified
//! stop, and `preinitialize` fails clearly when parking is unqualified or
//! policy-denied.

use std::sync::{Arc, Mutex};

use mllm_controller::{Controller, DeployRequest};
use mllm_adapters::fake::ParkPolicy;
use mllm_store::Store;

fn controller(policy: ParkPolicy) -> (Arc<Controller>, Arc<Mutex<Store>>, Arc<mllm_adapters::fake::FakeEngine>) {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let fake = Arc::new(mllm_adapters::fake::FakeEngine::new().with_policy(policy));
    let c = Arc::new(
        Controller::new_with_policy(
            store.clone(),
            fake.clone() as Arc<dyn mllm_adapters::traits::EngineAdapter>,
            Arc::new(mllm_adapters::fake::FakeLauncher::new()),
            policy,
        )
        .with_embedded_fake(fake.clone()),
    );
    (c, store, fake)
}

fn req(name: &str, kind: &str) -> DeployRequest {
    DeployRequest {
        name: name.into(),
        kind: kind.into(),
        manifest: format!(r#"{{"kind":"{kind}","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(name.into()),
    }
}

async fn make_ready(c: &Controller, name: &str, kind: &str) -> String {
    let id = c.submit_deploy(req(name, kind)).await.unwrap();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op).await.unwrap();
    id
}

#[tokio::test]
async fn ambiguous_park_reconciles_without_blind_repeat() {
    let (c, store, fake) = controller(ParkPolicy::ExperimentalAllowed);
    let id = make_ready(&c, "amb-m", "vllm-sleep").await;
    // Inject ambiguity: the park's effect applies but the ack is lost.
    fake.set_ambiguous_park();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Park).await.unwrap();
    assert!(c.wait_terminal(&op).await.is_err(), "ambiguous park is not a success");
    // Reconcile: uncertain → RECONCILING → FAILED; the collective is NOT
    // repeated blindly (T20).
    let evidence = store.lock().unwrap().journal_evidence_of(&id).unwrap().join("\n");
    assert!(evidence.contains("reconcil") || evidence.contains("uncertain"),
        "reconciliation recorded: {evidence}");
    let parks = store.lock().unwrap().operations_of_kind(&id, "park").unwrap().len();
    assert_eq!(parks, 1, "no blind repeated collective");
}

#[tokio::test]
async fn preinitialize_fails_clearly_on_restart_only() {
    let (c, _store, _f) = controller(ParkPolicy::Denied);
    let id = c.submit_deploy(req("plain", "model")).await.unwrap();
    let out = c.request_transition(&id, mllm_domain::LifecycleAction::Preinitialize).await;
    assert!(out.is_err(), "restart-only deployments cannot preinitialize");
    let err = out.unwrap_err().to_string();
    assert!(
        err.contains("unsupported") || err.contains("parking"),
        "failure names the capability: {err}"
    );
}

#[tokio::test]
async fn preinitialize_waits_for_qualified_parking() {
    let (c, store, _f) = controller(ParkPolicy::ExperimentalAllowed);
    let id = c.submit_deploy(req("pre-m", "vllm-sleep")).await.unwrap();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Preinitialize).await.unwrap();
    let end = c.wait_terminal(&op).await.unwrap();
    // Qualified parking: start → validate → park, ending PARKED.
    assert_eq!(end, mllm_domain::LifecycleState::Parked);
    let evidence = store.lock().unwrap().journal_evidence_of(&id).unwrap().join("\n");
    assert!(evidence.contains("parked"), "park evidence: {evidence}");
}