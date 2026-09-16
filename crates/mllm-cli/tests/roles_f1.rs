//! F1 roles wiring: `start_standalone` boots the embedded graph and serves
//! the router; the CLI deploy path submits + activates through the
//! controller; status reads without activating.



/// A state directory the controller lock will accept.
///
/// The lock walks every ancestor of the state path and refuses any that is group- or
/// other-writable, because such an ancestor lets another account replace the
/// directory the lock guards. `/tmp` is 1777 and a checkout is commonly 0775, so
/// neither can hold controller state. The home directory is the usual root that
/// satisfies the rule.
fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

use mllm_controller::LifecyclePort as _;
use mllm_cli::roles;
use mllm_controller::DeployRequest;

// Pending: standalone declares no runtime profile.
//
// The coordinator starts only a qualified deployment, and qualification requires a
// succeeded managed_configuration_create, which in turn requires a host policy
// carrying a runtime profile. The standalone default generates `runtime_profiles: {}`
// (mllm-config defaults), so no deployment created here can be qualified. F1 could
// start an unqualified deployment because it admitted work itself; the coordinator
// deliberately will not.
//
// These return once standalone declares its engine as a runtime profile, which is
// the engine-installation concept in ADR 0008.
#[ignore = "pending: standalone declares no runtime profile, so nothing can be qualified"]
#[tokio::test]
async fn standalone_boots_and_serves_router() {
    let dir = safe_state_dir();
    let app = roles::start_standalone(dir.path()).await.unwrap();
    // The App carries a servable router (F1: the router listener is the
    // standalone role's inference surface, 127.0.0.1-only).
    let router = app.router();
    let _ = router; // servable; full serve loop covered by run_standalone

    // Deploy through the controller (the CLI deploy path) and activate.
    let id = app
        .controller
        .submit_deploy("standalone", &DeployRequest {
            name: "wired-m".into(),
            kind: "model".into(),
            manifest: br#"{"kind":"model","name":"wired-m"}"#.to_vec(),
            route_model_id: Some("wired-m".into()),
        })
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    let state = app.controller.wait_terminal(&op).await.unwrap();
    assert_eq!(state, mllm_domain::LifecycleState::Ready);

    // Chat dispatch through the wired router deps reaches the fake engine.
    let resp = app
        .deps()
        .forwards
        .get("model")
        .unwrap()
        .forward_chat(&serde_json::json!({"model": "wired-m", "messages": []}))
        .await
        .unwrap();
    assert!(resp["choices"][0]["message"]["content"].is_string());
}

// Pending: standalone declares no runtime profile.
//
// The coordinator starts only a qualified deployment, and qualification requires a
// succeeded managed_configuration_create, which in turn requires a host policy
// carrying a runtime profile. The standalone default generates `runtime_profiles: {}`
// (mllm-config defaults), so no deployment created here can be qualified. F1 could
// start an unqualified deployment because it admitted work itself; the coordinator
// deliberately will not.
//
// These return once standalone declares its engine as a runtime profile, which is
// the engine-installation concept in ADR 0008.
#[ignore = "pending: standalone declares no runtime profile, so nothing can be qualified"]
#[tokio::test]
async fn router_serves_models_and_chat_over_http() {
    let dir = safe_state_dir();
    let app = roles::start_standalone(dir.path()).await.unwrap();
    let id = app
        .controller
        .submit_deploy("standalone", &DeployRequest {
            name: "http-m".into(),
            kind: "model".into(),
            manifest: br#"{"kind":"model","name":"http-m"}"#.to_vec(),
            route_model_id: Some("http-m".into()),
        })
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    app.controller.wait_terminal(&op).await.unwrap();

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
    assert!(ids["data"].as_array().unwrap().iter().any(|m| m["id"] == "http-m"));

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
}