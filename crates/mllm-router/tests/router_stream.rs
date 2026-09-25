//! Streaming chat (SSE pass-through) and conservative in-flight accounting
//! (T17 groundwork): client disconnect is NOT proof the engine stopped —
//! the guard stays registered until the backend stream ends or the work is
//! released.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mllm_controller::Controller;
use mllm_router::admission::InFlight;
use mllm_router::forwarders::StaticForwarders;
use mllm_router::{QueueLimits, RouterDeps};
use mllm_store::Store;
use mllm_testkit::FakeEngine;
use tower::ServiceExt;

async fn app_streaming() -> (axum::Router, Arc<InFlight>, Arc<Controller>) {
    let shared = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let fake = Arc::new(FakeEngine::new());
    let controller = Arc::new(Controller::new(
        shared.clone(),
        fake.clone() as Arc<dyn mllm_adapters::EngineAdapter>,
        Arc::new(mllm_testkit::FakeLauncher::new()),
    ));
    let inflight = Arc::new(InFlight::default());
    let deps = RouterDeps {
        controller: controller.clone(),
        forwards: Arc::new(StaticForwarders(HashMap::from([(
            "fake".to_string(),
            fake.clone() as Arc<dyn mllm_adapters::ChatForward>,
        )]))),
        limits: QueueLimits {
            max_requests_per_deployment: 8,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: Some("test-key".into()),
        inflight: Arc::new(mllm_router::admission::InFlight::default()),
        activation_join: Arc::new(mllm_router::WakeJoin::new()),
    };
    (mllm_router::serve_router(deps), inflight, controller)
}

fn req(_id: &str, name: &str) -> mllm_controller::DeployRequest {
    mllm_controller::DeployRequest {
        name: name.into(),
        kind: "fake".into(),
        manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(name.into()),
    }
}

#[tokio::test]
async fn streaming_chat_returns_sse_events_in_order() {
    // (deployed through the switch task's full path in Task 11; here the
    // router dispatches the READY deployment directly)
    let (router, _inflight, controller) = app_streaming().await;
    let id = controller
        .submit_deploy(req("s1", "stream-m"))
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();

    let res = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "stream-m", "stream": true,
                        "messages": [{"role": "user", "content": "hi"}]})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let data_lines: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("data: "))
        .map(|l| &l[6..])
        .collect();
    assert_eq!(
        data_lines.last(),
        Some(&"[DONE]"),
        "SSE ends with [DONE]: {text}"
    );
}

#[tokio::test]
async fn abandoned_request_keeps_inflight_until_confirmed() {
    let inflight = InFlight::default();
    // Client disconnect = abandon; accounting stays conservative until the
    // backend completes or the cancellation is confirmed.
    let guard = inflight.guard("d1");
    assert_eq!(inflight.current("d1"), 1);
    // Simulate the client leaving mid-stream: the request is abandoned but
    // the guard is still held (engine may still be working).
    let abandoned = guard.abandon();
    assert_eq!(
        inflight.current("d1"),
        1,
        "abandoned ≠ stopped: accounting retained"
    );
    // Backend stream ends → confirmed → released.
    abandoned.release();
    assert_eq!(inflight.current("d1"), 0);
}

#[tokio::test]
async fn completion_releases_accounting() {
    let inflight = InFlight::default();
    let guard = inflight.guard("d2");
    assert_eq!(inflight.current("d2"), 1);
    drop(guard); // normal completion releases
    assert_eq!(inflight.current("d2"), 0);
}
