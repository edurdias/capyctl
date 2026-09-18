//! Shared wiring for the standalone integration tests.
//!
//! Every test binary that includes this module uses some of it, so unused items
//! here are expected rather than a sign of dead code.
#![allow(dead_code)]

use std::sync::Arc;

/// A state directory the controller lock will accept.
///
/// The lock walks every ancestor of the state path and refuses any that is group- or
/// other-writable, because such an ancestor lets another account replace the
/// directory the lock guards. `/tmp` is 1777 and a checkout is commonly 0775, so
/// neither can hold controller state. The home directory is the usual root that
/// satisfies the rule.
pub fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

/// Boot standalone on the testkit's Fake installation.
///
/// Passing the installation in is what keeps these tests from depending on whatever
/// engine the developer's environment happens to name. Passing this one is not
/// qualification of a native recipe and must never be reported as one (SPEC §18).
pub async fn boot(state_dir: &std::path::Path) -> mllm_cli::roles::App {
    mllm_cli::roles::start_standalone_with(state_dir, mllm_testkit::fake_provider())
        .await
        .expect("standalone boots")
}

/// A minimal engine at the endpoint the coordinator recorded for a deployment.
///
/// The Fake engine answers in process and listens on nothing, while the router now
/// forwards to the address the launch recorded (SPEC §3). Standing a server up at
/// that address is what lets these tests exercise the real dispatch path, and it
/// also proves the router forwards to the recorded endpoint rather than to anything
/// it held from boot.
pub async fn stub_engine(
    controller: &Arc<mllm_controller::CoordinatorLifecycle>,
    deployment: &str,
) -> tokio::task::JoinHandle<()> {
    use mllm_controller::LifecyclePort as _;
    let runtime = controller
        .runtime_endpoint(deployment)
        .expect("the runtime endpoint is readable")
        .expect("a ready deployment has a recorded runtime");
    let url: reqwest::Url = runtime.endpoint.parse().expect("the endpoint is a URL");
    let address = format!(
        "{}:{}",
        url.host_str().expect("the endpoint names a host"),
        url.port().expect("the endpoint names a port")
    );
    let served = runtime.served_model.clone();
    let models = {
        let served = served.clone();
        axum::routing::get(move || {
            let served = served.clone();
            async move {
                axum::Json(serde_json::json!({
                    "object": "list",
                    "data": [{"id": served, "object": "model"}]
                }))
            }
        })
    };
    // The forwarder always asks the engine to stream, and it validates every event
    // it is sent (SPEC §10), so the stub answers in the protocol an engine answers
    // in rather than with a single completion object.
    let chat = {
        let served = served.clone();
        axum::routing::post(move || {
            let served = served.clone();
            async move {
                let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                    serde_json::json!({
                        "id": "stub-1",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": served,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
                    })
                };
                let body = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(
                        serde_json::json!({"role": "assistant", "content": "ready"}),
                        serde_json::Value::Null
                    ),
                    chunk(serde_json::json!({}), serde_json::json!("stop")),
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }
        })
    };
    let app = axum::Router::new()
        .route("/v1/models", models)
        .route("/v1/chat/completions", chat);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .unwrap_or_else(|error| panic!("the recorded endpoint {address} is bindable: {error}"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })
}
