//! Profile-level development-mode gate (F1 design §7, T21): launching the
//! experimental vllm-sleep profile (development flags set at launch)
//! requires the host-policy opt-in — the gate covers the profile itself,
//! not just park/reload operations. Stock profiles launch ungated.

use std::sync::{Arc, Mutex};

use capyctl_adapters::ParkPolicy;
use capyctl_controller::{Controller, DeployRequest};
use capyctl_store::Store;

fn controller(policy: ParkPolicy) -> (Arc<Controller>, Arc<Mutex<Store>>) {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let c = Arc::new(Controller::new_with_policy(
        store.clone(),
        Arc::new(capyctl_testkit::FakeEngine::new()),
        Arc::new(capyctl_testkit::FakeLauncher::new()),
        policy,
    ));
    (c, store)
}

fn req(name: &str, kind: &str) -> DeployRequest {
    DeployRequest {
        name: name.into(),
        kind: kind.into(),
        manifest: format!(r#"{{"kind":"{kind}","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(name.into()),
    }
}

#[tokio::test]
async fn vllm_sleep_profile_launch_denied_by_default() {
    let (c, store) = controller(ParkPolicy::Disabled);
    let id = c.submit_deploy(req("sleepy", "vllm-sleep")).await.unwrap();
    let out = c
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await;
    assert!(
        out.is_err(),
        "development-mode profile denied without opt-in (T21)"
    );
    // Denial journaled as policy evidence.
    let evidence = store
        .lock()
        .unwrap()
        .journal_evidence_of(&id)
        .unwrap()
        .join("\n");
    assert!(
        evidence.contains("policy_denied"),
        "denial recorded: {evidence}"
    );
    // No operation ran: the deployment stays Stopped.
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_deployment(&id)
            .unwrap()
            .unwrap()
            .observed_state,
        capyctl_domain::LifecycleState::Stopped
    );
}

#[tokio::test]
async fn opt_in_enables_the_experimental_profile() {
    let (c, _store) = controller(ParkPolicy::Enabled);
    let id = c.submit_deploy(req("sleepy", "vllm-sleep")).await.unwrap();
    let op = c
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    let state = c.wait_terminal(&op).await.unwrap();
    assert_eq!(state, capyctl_domain::LifecycleState::Ready);
}

#[tokio::test]
async fn stock_profile_launches_without_optin() {
    let (c, _store) = controller(ParkPolicy::Disabled);
    let id = c.submit_deploy(req("stock", "model")).await.unwrap();
    let op = c
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    let state = c.wait_terminal(&op).await.unwrap();
    assert_eq!(state, capyctl_domain::LifecycleState::Ready);
}
