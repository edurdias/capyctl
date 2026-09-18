//! Generation machinery (F0 deferral closed in F1 design G3): every
//! successful transition bumps the deployment generation, writes
//! generation_history, and stale-generation dispatch is rejected (T18).
//! Reservations are persisted at acceptance.

use std::sync::{Arc, Mutex};

use mllm_controller::{Controller, DeployRequest};
use mllm_domain::LifecycleState;
use mllm_store::Store;

fn controller() -> (Arc<Controller>, Arc<Mutex<Store>>) {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let c = Arc::new(Controller::new(
        store.clone(),
        Arc::new(mllm_testkit::FakeEngine::new()),
        Arc::new(mllm_testkit::FakeLauncher::new()),
    ));
    (c, store)
}

fn req(name: &str) -> DeployRequest {
    DeployRequest {
        name: name.into(),
        kind: "model".into(),
        manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(name.into()),
    }
}

#[tokio::test]
async fn transitions_bump_generation_and_write_history() {
    let (c, store) = controller();
    let id = c.submit_deploy(req("gen-m")).await.unwrap();

    let gen0 = store.lock().unwrap().get_deployment(&id).unwrap().unwrap().current_generation;
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op).await.unwrap();
    let gen1 = store.lock().unwrap().get_deployment(&id).unwrap().unwrap().current_generation;
    assert!(gen1 > gen0, "generation bumps on transition ({gen0} -> {gen1})");

    let op2 = c
        .request_transition(&id, mllm_domain::LifecycleAction::Park)
        .await
        .unwrap();
    c.wait_terminal(&op2).await.unwrap();
    let gen2 = store.lock().unwrap().get_deployment(&id).unwrap().unwrap().current_generation;
    assert!(gen2 > gen1);

    let history = store.lock().unwrap().generation_history(&id).unwrap();
    assert!(history.len() >= 2, "each transition recorded: {history:?}");
}

#[tokio::test]
async fn stale_generation_dispatch_rejected() {
    let (c, _store) = controller();
    let id = c.submit_deploy(req("stale-m")).await.unwrap();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op).await.unwrap();

    let gen_before = {
        let s = c.store_ref();
        let s = s.lock().unwrap();
        s.get_deployment(&id).unwrap().unwrap().current_generation
    };
    let op2 = c.request_transition(&id, mllm_domain::LifecycleAction::Park).await.unwrap();
    c.wait_terminal(&op2).await.unwrap();
    let gen_after = {
        let s = c.store_ref();
        let s = s.lock().unwrap();
        s.get_deployment(&id).unwrap().unwrap().current_generation
    };
    assert!(gen_after > gen_before);
    // Dispatch carrying the stale generation must be rejected (T18).
    let out = c.check_dispatch_generation(&id, gen_before);
    assert!(out.is_err(), "stale generation rejected");
    let out = c.check_dispatch_generation(&id, gen_after);
    assert!(out.is_ok(), "current generation accepted");
}

#[tokio::test]
async fn generation_survives_new_controller_over_same_store() {
    let (c, store) = controller();
    let id = c.submit_deploy(req("persist-m")).await.unwrap();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op).await.unwrap();
    let gen = store.lock().unwrap().get_deployment(&id).unwrap().unwrap().current_generation;

    // Fresh controller over the same store: generations continue (never reset).
    let c2 = Controller::new(
        store.clone(),
        Arc::new(mllm_testkit::FakeEngine::new()),
        Arc::new(mllm_testkit::FakeLauncher::new()),
    );
    let op2 = c2
        .request_transition(&id, mllm_domain::LifecycleAction::Stop)
        .await
        .unwrap();
    let end = c2.wait_terminal(&op2).await.unwrap();
    assert_eq!(end, LifecycleState::Stopped);
    let gen2 = store.lock().unwrap().get_deployment(&id).unwrap().unwrap().current_generation;
    assert!(gen2 > gen);
}

#[tokio::test]
async fn acceptance_persists_reservation_rows() {
    let (c, store) = controller();
    let id = c.submit_deploy(req("res-m")).await.unwrap();
    let reservations = store.lock().unwrap().reservations_for_owner(&id).unwrap();
    assert!(
        !reservations.is_empty(),
        "initial reservation intent persisted at acceptance: {reservations:?}"
    );
}