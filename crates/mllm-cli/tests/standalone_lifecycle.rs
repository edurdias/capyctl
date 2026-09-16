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

/// The lifecycle standalone actually supports, walked through the durable store.
///
/// There is no park here. Standalone declares its deployments restart-only, which
/// SPEC §6.2 makes first-class for exactly this case: the engine has no qualified
/// memory release to prove. Parking is covered where warm residency exists, not by
/// asserting a transition this deployment is not configured for.
#[tokio::test]
async fn standalone_boot_runs_the_restart_only_lifecycle() {
    let dir = safe_state_dir();
    let app = roles::start_standalone(dir.path()).await.unwrap(); // in-process server+host
    let dep = app.deploy("m1", "/models/m1").unwrap();
    assert!(app.store.get_deployment(&dep).unwrap().is_some());
    async fn run(app: &App, dep: &str, action: LifecycleAction, want: LifecycleState) {
        let op = app.controller.request_transition(dep, action).await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            app.controller.wait_terminal(&op),
        )
        .await
        .expect("the transition settles rather than hanging")
        .unwrap();
        // The durable row, not the returned state: a lifecycle that only advanced
        // in memory would not survive the restart the store exists to survive.
        let row = app.store.get_deployment(dep).unwrap().unwrap();
        assert_eq!(row.observed_state, want, "after {action:?}");
    }
    run(&app, &dep, LifecycleAction::Start, LifecycleState::Ready).await;
    run(&app, &dep, LifecycleAction::Stop, LifecycleState::Stopped).await;
    // Restarting is how a restart-only deployment comes back; it is not a wake.
    run(&app, &dep, LifecycleAction::Start, LifecycleState::Ready).await;
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
    let dep = app.deploy("m1", "/models/m1").unwrap();
    let err = app
        .controller
        .request_transition(&dep, LifecycleAction::Stop)
        .await
        .unwrap_err();
    // There is nothing to stop: no runtime was ever started, so no cleanup can be
    // accepted against one. What matters is that it is refused rather than reported
    // as a stop that did nothing.
    //
    // It is reported as a conflict, which reads as "your view is stale, re-read and
    // retry" — and re-reading will not help, because nothing was ever started. The
    // honest vocabulary is a refusal. Pinned here so a change is deliberate; the
    // question is recorded in the status runbook.
    assert!(
        matches!(err, mllm_controller::LifecycleFault::Conflict(_)),
        "{err:?}"
    );
}
