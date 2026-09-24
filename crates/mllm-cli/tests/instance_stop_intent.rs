//! Owner decisions Q5 and Q7 (ADR 0013 as amended): an operator's `stop
//! instance` survives the next inference request, and an explicit `start
//! deployment` lifts it. Fake-engine test only; not qualification.

mod support;

use mllm_config::effective::ModelSource;
use mllm_controller::LifecyclePort as _;
use mllm_domain::{LifecycleAction, LifecycleState};
use support::{boot, safe_state_dir};

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

// T10 T18
#[tokio::test]
async fn a_stopped_instance_survives_inference_and_start_deployment_lifts_it() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "instance-intent-m",
            ModelSource::Local {
                path: "/models/instance-intent-m".into(),
            },
        )
        .expect("deployed");
    settle(&app, &id, LifecycleAction::Start, LifecycleState::Ready).await;
    let evicted = app.controller.idle_stop(&id).await.expect("idle stop");
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&evicted),
    )
    .await
    .expect("the stop settles")
    .expect("the stop reaches a terminal state");
    // The operator's per-instance stop of the only realized instance (test
    // shortcut for the management command, which records the same mark).
    app.store
        .set_instance_operator_stopped(&id, 0, true)
        .expect("instance 0 exists");
    match app.controller.auto_activate(&id).await {
        Err(mllm_controller::LifecycleFault::Blocked(reason)) => {
            assert!(reason.contains("explicitly stopped"), "{reason}")
        }
        other => panic!("a request must not undo an operator's instance stop: {other:?}"),
    }
    settle(&app, &id, LifecycleAction::Start, LifecycleState::Ready).await;
    assert!(!app.store.on_demand_instance_stopped(&id).unwrap());
}
