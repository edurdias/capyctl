//! SPEC §6.3: an administrative stop suspends automatic activation; an idle
//! eviction does not.
//!
//! "Required explicit stop behavior MUST NOT be undone by the next inference
//! request" — and the converse matters just as much: an idle eviction that
//! suspended activation would leave a deployment down until an operator noticed.
//! The two commands differ only in that intent, so they are tested together.

mod support;

use capyctl_config::effective::ModelSource;
use capyctl_controller::LifecyclePort as _;
use capyctl_domain::{LifecycleAction, LifecycleState};
use support::{boot, safe_state_dir};

/// Boot standalone and bring one deployment to Ready.
async fn ready() -> (tempfile::TempDir, capyctl_cli::roles::App, String) {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "intent-m",
            ModelSource::Local {
                path: "/models/intent-m".into(),
            },
        )
        .expect("deployed");
    settle(&app, &id, LifecycleAction::Start, LifecycleState::Ready).await;
    (dir, app, id)
}

async fn settle(
    app: &capyctl_cli::roles::App,
    id: &str,
    action: LifecycleAction,
    want: LifecycleState,
) {
    let handle = app
        .controller
        .request_transition(id, action)
        .await
        .unwrap_or_else(|error| panic!("{action:?} was refused: {error:?}"));
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&handle),
    )
    .await
    .expect("the transition settles rather than hanging")
    .unwrap_or_else(|error| panic!("{action:?} did not settle: {error:?}"));
    assert_eq!(state, want);
}

#[tokio::test]
async fn an_administrative_stop_survives_the_next_inference_request() {
    let (_dir, app, id) = ready().await;
    settle(&app, &id, LifecycleAction::Stop, LifecycleState::Stopped).await;

    // `auto_activate` is what an arriving request calls. It must refuse.
    match app.controller.auto_activate(&id).await {
        // SPEC §10 (owner decision 2026-09-25): the operator's stop, with
        // how to start it again; not a capacity refusal.
        Err(capyctl_controller::LifecycleFault::Stopped(reason)) => {
            assert!(reason.contains("stopped by an operator"), "{reason}");
            assert!(
                reason.contains(&format!("capyctl start deployment {id}")),
                "{reason}"
            );
        }
        other => panic!("a request must not undo an operator's stop: {other:?}"),
    }
}

#[tokio::test]
async fn an_operator_can_start_what_they_stopped() {
    let (_dir, app, id) = ready().await;
    settle(&app, &id, LifecycleAction::Stop, LifecycleState::Stopped).await;

    // An explicit start enables the deployment, so it lifts the operator's own
    // earlier stop rather than being refused by it.
    settle(&app, &id, LifecycleAction::Start, LifecycleState::Ready).await;
}

#[tokio::test]
async fn an_idle_stop_leaves_the_deployment_on_demand_eligible() {
    let (_dir, app, id) = ready().await;
    let evicted = app
        .controller
        .idle_stop(&id)
        .await
        .expect("an idle deployment can be evicted");
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&evicted),
    )
    .await
    .expect("the eviction settles")
    .expect("the eviction reaches a terminal state");
    assert_eq!(state, LifecycleState::Stopped);

    // The next request brings it back, which is the whole point of evicting it.
    app.controller
        .auto_activate(&id)
        .await
        .expect("an evicted deployment is still activatable on demand");
}

// T10 T18 (SPEC §10, owner decision 2026-09-25): an inference request for a
// deployment the operator stopped is answered 409 `deployment_stopped`, saying
// the operator stopped it and how to start it. Before, it was 429
// `insufficient_resources`, which sent clients after capacity that was never
// the reason. The error body keeps its shape (`code`, `message`).
#[tokio::test]
async fn a_request_for_an_operator_stopped_deployment_is_409_deployment_stopped() {
    let (_dir, app, id) = ready().await;
    settle(&app, &id, LifecycleAction::Stop, LifecycleState::Stopped).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let chat = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {}", app.api_key()))
        .json(&serde_json::json!({"model": "intent-m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 409);
    let body: serde_json::Value = chat.json().await.unwrap();
    assert_eq!(body["code"], "deployment_stopped", "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("stopped by an operator"), "{message}");
    assert!(
        message.contains(&format!("capyctl start deployment {id}")),
        "{message}"
    );
    assert_eq!(
        app.store
            .get_deployment(&id)
            .unwrap()
            .unwrap()
            .observed_state,
        LifecycleState::Stopped,
        "the request did not undo the operator's stop"
    );
    server.abort();
}
