//! Router core (F1 design §5): /v1/models never wakes; chat completions
//! admit against bounds and dispatch only to READY deployments; auth is
//! API-key; queue limits return structured errors.


use axum::body::Body;
use tower::ServiceExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mllm_adapters::fake::FakeEngine;
use mllm_controller::Controller;
use mllm_router::{QueueLimits, RouterDeps};
use mllm_store::Store;

async fn app() -> (axum::Router, Arc<Mutex<Store>>, Arc<Controller>, mllm_store::Store) {
    let store = Store::open_in_memory().unwrap();
    let shared = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let fake = Arc::new(FakeEngine::new());
    let adapter = fake.clone() as Arc<dyn mllm_adapters::ChatForward>;
    let controller = Arc::new(Controller::new(
        shared.clone(),
        fake.clone() as Arc<dyn mllm_adapters::EngineAdapter>,
        Arc::new(mllm_adapters::fake::FakeLauncher::new()),
    ));
    let deps = RouterDeps {
        store: shared.clone(),
        controller: controller.clone(),
        forwards: HashMap::from([("fake".to_string(), adapter)]),
        limits: QueueLimits { max_requests_per_deployment: 2, max_buffered_bytes_total: 1024 },
        api_key: Some("test-key".into()),
    };
    let file_store = Store::open_in_memory().unwrap();
    (mllm_router::serve_router(deps, "127.0.0.1:0".parse().unwrap()), shared, controller, file_store)
}

async fn deploy_ready(
    _router: &axum::Router,
    controller: &Controller,
    name: &str,
) -> String {
    let id = controller
        .submit_deploy(mllm_controller::DeployRequest {
            name: name.into(),
            kind: "fake".into(),
            manifest: format!("name: {name}\nkind: fake\n").into_bytes(),
            route_model_id: Some(name.into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    id
}

#[tokio::test]
async fn models_lists_enabled_never_wakes() {
    let (router, store, controller, _fs) = app().await;
    let _ = deploy_ready(&router, &controller, "m1").await;
    let _ = store;
    let res = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/models")
                .header("Authorization", "Bearer test-key")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["data"][0]["id"], "m1");
    // Never wakes: observed state unchanged (Ready, since it was ready — but
    // a STOPPED deployment stays STOPPED).
}

#[tokio::test]
async fn models_does_not_activate_stopped_deployment() {
    let (router, store, controller, _fs) = app().await;
    let id = controller
        .submit_deploy(mllm_controller::DeployRequest {
            name: "stopped-m".into(),
            kind: "fake".into(),
            manifest: b"name: stopped-m\n".to_vec(),
            route_model_id: Some("stopped-m".into()),
        })
        .await
        .unwrap();
    let res = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/models")
                .header("Authorization", "Bearer test-key")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    // The stopped deployment is still listed (enabled route), NOT activated.
    let row = store.lock().unwrap().get_deployment(&id).unwrap().unwrap();
    assert_eq!(row.observed_state, mllm_domain::LifecycleState::Stopped);
}

#[tokio::test]
async fn unauthenticated_requests_rejected() {
    let (router, _s, _c, _f) = app().await;
    let res = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/models")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn chat_dispatches_to_ready_deployment() {
    let (router, _s, controller, _f) = app().await;
    deploy_ready(&router, &controller, "ready-m").await;
    let res = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "ready-m", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["choices"][0]["message"]["content"].is_string());
}

#[tokio::test]
async fn queue_bounds_return_structured_error() {
    let (router, _s, controller, _f) = app().await;
    deploy_ready(&router, &controller, "bounded-m").await;
    // max_buffered_bytes_total = 1024; a body larger than that is rejected.
    let big = "x".repeat(4096);
    let res = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "bounded-m", "messages": [{"role": "user", "content": big}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 413);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "queue_full");
}