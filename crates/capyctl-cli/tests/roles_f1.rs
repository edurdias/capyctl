//! F1 roles wiring: `start_standalone_with` boots the embedded graph and serves
//! the router; the CLI deploy path submits + activates through the
//! controller; status reads without activating.

mod support;

use capyctl_config::effective::ModelSource;
use capyctl_controller::LifecyclePort as _;
use support::{boot, safe_state_dir, stub_engine};

#[tokio::test]
async fn standalone_boots_and_serves_router() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    // The App carries a servable router (F1: the router listener is the
    // standalone role's inference surface, 127.0.0.1-only).
    let router = app.router();
    let _ = router; // servable; full serve loop covered by run_standalone

    // Deploy through the controller (the CLI deploy path) and activate.
    let id = app
        .deploy(
            "wired-m",
            ModelSource::Local {
                path: "/models/wired-m".into(),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    let state = app.controller.wait_terminal(&op).await.unwrap();
    assert_eq!(state, capyctl_domain::LifecycleState::Ready);

    // Dispatch resolves the forwarder from what the launch recorded, so the engine
    // has to be at that address for the wiring to be exercised at all.
    let engine = stub_engine(&app.controller, &id).await;
    let resp = app
        .deps()
        .forwards
        .forwarder(&id)
        .expect("a ready deployment has a forwarder")
        .forward_chat(&serde_json::json!({"model": "wired-m", "messages": []}))
        .await
        .unwrap();
    assert!(resp["choices"][0]["message"]["content"].is_string());
    engine.abort();
}

#[tokio::test]
async fn router_serves_models_and_chat_over_http() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "http-m",
            ModelSource::Local {
                path: "/models/http-m".into(),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    app.controller.wait_terminal(&op).await.unwrap();
    let engine = stub_engine(&app.controller, &id).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = reqwest::Client::new();
    let key = app.api_key();
    let models = client
        .get(format!("http://{addr}/v1/models"))
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let ids: serde_json::Value = models.json().await.unwrap();
    assert!(ids["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "http-m"));

    let chat = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({"model": "http-m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 200);
    let body: serde_json::Value = chat.json().await.unwrap();
    assert!(body["choices"][0]["message"]["content"].is_string());
    engine.abort();
}
