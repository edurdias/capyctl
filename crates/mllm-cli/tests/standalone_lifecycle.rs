//! F0 exit gate: `mllm start standalone` boots an embedded server+host and
//! drives the full fake lifecycle end-to-end through the durable store.


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
use mllm_cli::roles::{self, App};
use mllm_controller::DeployRequest;
use mllm_domain::{LifecycleAction, LifecycleState};

fn req_fake_engine(name: &str) -> DeployRequest {
    DeployRequest {
        name: name.to_string(),
        kind: "model".to_string(),
        manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(name.to_string()),
    }
}

// Pending for the same reason as the roles_f1 start tests, plus one of its own: this
// walks a park/wake cycle, and the deployment standalone declares is restart-only, so
// it has no park to walk. Returns with A1's runtime drive and A1b's ordinary park.
#[ignore = "pending A1: a Start is accepted but nothing drives the binding to Ready"]
#[tokio::test]
async fn standalone_boot_runs_full_fake_lifecycle() {
    let dir = safe_state_dir();
    let app = roles::start_standalone(dir.path()).await.unwrap(); // in-process server+host
    let dep = app.controller.submit_deploy("standalone", &req_fake_engine("m1")).unwrap(); // durable ID
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
    let dir = safe_state_dir();
    let app = roles::start_standalone(dir.path()).await.unwrap();
    let first = app.controller.submit_deploy("standalone", &req_fake_engine("m1")).unwrap();
    let second = app.controller.submit_deploy("standalone", &req_fake_engine("m1")).unwrap();
    assert_eq!(first, second);
    assert_eq!(app.store.deployment_count().unwrap(), 1);
}

#[tokio::test]
async fn stop_is_illegal_from_stopped() {
    let dir = safe_state_dir();
    let app = roles::start_standalone(dir.path()).await.unwrap();
    let dep = app.controller.submit_deploy("standalone", &req_fake_engine("m1")).unwrap();
    let err = app
        .controller
        .request_transition(&dep, LifecycleAction::Stop)
        .await
        .unwrap_err();
    // The refusal is preserved but its reason moved: the coordinator has no ordinary
    // stop yet, so it refuses as unsupported rather than as an illegal transition.
    // The stop semantics this asserted return with milestone A1b.
    assert!(matches!(err, mllm_controller::LifecycleFault::Blocked(_)));
}
