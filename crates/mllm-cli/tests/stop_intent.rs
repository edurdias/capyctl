//! SPEC §6.3: an administrative stop suspends automatic activation; an idle
//! eviction does not.
//!
//! "Required explicit stop behavior MUST NOT be undone by the next inference
//! request" — and the converse matters just as much: an idle eviction that
//! suspended activation would leave a deployment down until an operator noticed.
//! The two commands differ only in that intent, so they are tested together.

use mllm_controller::LifecyclePort as _;
use mllm_domain::{LifecycleAction, LifecycleState};

fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

/// Boot standalone and bring one deployment to Ready.
async fn ready() -> (tempfile::TempDir, mllm_cli::roles::App, String) {
    let dir = safe_state_dir();
    let app = mllm_cli::roles::start_standalone(dir.path())
        .await
        .expect("standalone boots");
    let id = app.deploy("intent-m", "/models/intent-m").expect("deployed");
    settle(&app, &id, LifecycleAction::Start, LifecycleState::Ready).await;
    (dir, app, id)
}

async fn settle(
    app: &mllm_cli::roles::App,
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
        Err(mllm_controller::LifecycleFault::Blocked(reason)) => {
            assert!(reason.contains("explicitly stopped"), "{reason}")
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
