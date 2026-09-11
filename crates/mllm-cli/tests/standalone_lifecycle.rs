//! F0 exit gate: `mllm start standalone` boots an embedded server+host and
//! drives the full fake lifecycle end-to-end through the durable store.

use mllm_cli::roles::{self, App};
use mllm_controller::DeployRequest;
use mllm_domain::{LifecycleAction, LifecycleState};

fn req_fake_engine(name: &str) -> DeployRequest {
    DeployRequest {
        name: name.to_string(),
        kind: "model".to_string(),
        manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
    }
}

#[tokio::test]
async fn standalone_boot_runs_full_fake_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let app = roles::start_standalone(dir.path()).await.unwrap(); // in-process server+host
    let dep = app.controller.submit_deploy(req_fake_engine("m1")).await.unwrap(); // durable ID
    assert!(app.store.get_deployment(&dep).unwrap().is_some());
    async fn run(app: &App, dep: &str, action: LifecycleAction, want: LifecycleState) {
        let op = app.controller.request_transition(dep, action).await.unwrap();
        app.controller.wait_terminal(&op).await.unwrap();
        let row = app.store.get_deployment(dep).unwrap().unwrap();
        assert_eq!(row.observed_state, want);
    }
    run(&app, &dep, LifecycleAction::Start, LifecycleState::Ready).await;
    // park/wake cycle:
    run(&app, &dep, LifecycleAction::Park, LifecycleState::Parked).await;
    run(&app, &dep, LifecycleAction::Start, LifecycleState::Ready).await;
    // stop:
    run(&app, &dep, LifecycleAction::Stop, LifecycleState::Stopped).await;
}

#[tokio::test]
async fn resubmitting_the_same_request_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let app = roles::start_standalone(dir.path()).await.unwrap();
    let first = app.controller.submit_deploy(req_fake_engine("m1")).await.unwrap();
    let second = app.controller.submit_deploy(req_fake_engine("m1")).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(app.store.deployment_count().unwrap(), 1);
}

#[tokio::test]
async fn stop_is_illegal_from_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let app = roles::start_standalone(dir.path()).await.unwrap();
    let dep = app.controller.submit_deploy(req_fake_engine("m1")).await.unwrap();
    let err = app
        .controller
        .request_transition(&dep, LifecycleAction::Stop)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        mllm_controller::ControllerError::IllegalTransition { .. }
    ));
}
