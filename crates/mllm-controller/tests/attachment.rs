//! Attachment (F1 design §6, T11): routing works; lifecycle operations are
//! rejected — ownership ≠ reachability. Attached usage is represented
//! conservatively; restart guarantees are marked unavailable without a
//! supervisor integration.

use std::sync::{Arc, Mutex};

use mllm_controller::Controller;
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

#[tokio::test]
async fn attach_registers_route_and_rejects_lifecycle() {
    let (c, store) = controller();
    let id = c
        .attach(mllm_controller::AttachRequest {
            name: "external-vllm".into(),
            endpoint: "http://127.0.0.1:9100".into(),
            route_model_id: Some("external-m".into()),
            manifest: br#"kind: attached"#.to_vec(),
        })
        .await
        .unwrap();

    let row = store.lock().unwrap().get_deployment(&id).unwrap().unwrap();
    assert_eq!(row.kind, "attached");

    // Routing works through the attached deployment's endpoint (the
    // forwarder map carries attached endpoints; lifecycle does not).
    // Lifecycle operations are rejected — no sleep/kill/restart rights.
    for action in [
        mllm_domain::LifecycleAction::Start,
        mllm_domain::LifecycleAction::Park,
        mllm_domain::LifecycleAction::Stop,
        mllm_domain::LifecycleAction::Preinitialize,
    ] {
        let out = c.request_transition(&id, action).await;
        assert!(out.is_err(), "{action:?} must be rejected on attached");
        let err = out.unwrap_err().to_string();
        assert!(err.contains("attached"), "error names attachment: {err}");
    }
}

#[tokio::test]
async fn attached_usage_is_conservative_not_reclaimable() {
    let (c, store) = controller();
    let id = c
        .attach(mllm_controller::AttachRequest {
            name: "ext".into(),
            endpoint: "http://127.0.0.1:9101".into(),
            route_model_id: Some("ext-m".into()),
            manifest: br#"kind: attached"#.to_vec(),
        })
        .await
        .unwrap();
    // The attached deployment holds a reservation that admission treats as
    // charged (never reclaimable capacity without evidence).
    let reservations = store.lock().unwrap().reservations_for_owner(&id).unwrap();
    assert!(
        !reservations.is_empty(),
        "attached usage charged conservatively"
    );
}

#[tokio::test]
async fn restart_guarantees_marked_unavailable() {
    let (c, store) = controller();
    let id = c
        .attach(mllm_controller::AttachRequest {
            name: "ext2".into(),
            endpoint: "http://127.0.0.1:9102".into(),
            route_model_id: None,
            manifest: br#"kind: attached"#.to_vec(),
        })
        .await
        .unwrap();
    // No supervisor integration configured: status marks restart
    // guarantees unavailable (SPEC §5.2).
    assert!(!store.lock().unwrap().has_supervisor_guarantee(&id).unwrap());
}
