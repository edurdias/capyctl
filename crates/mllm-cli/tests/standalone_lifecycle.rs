//! F0 exit gate: `mllm start standalone` boots an embedded server+host and
//! drives the full fake lifecycle end-to-end through the durable store.

mod support;

use mllm_cli::roles::App;
use mllm_config::effective::ModelSource;
use mllm_controller::DeployRequest;
use mllm_controller::LifecyclePort as _;
use mllm_domain::{LifecycleAction, LifecycleState};
use support::{boot, safe_state_dir};

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
    let app = boot(dir.path()).await; // in-process server+host
    let dep = app
        .deploy(
            "m1",
            ModelSource::Local {
                path: "/models/m1".into(),
            },
        )
        .unwrap();
    assert!(app.store.get_deployment(&dep).unwrap().is_some());
    async fn run(app: &App, dep: &str, action: LifecycleAction, want: LifecycleState) {
        let op = app
            .controller
            .request_transition(dep, action)
            .await
            .unwrap();
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
    let app = boot(dir.path()).await;
    let first = app
        .controller
        .submit_deploy("standalone", &req_fake_engine("m1"))
        .unwrap();
    let second = app
        .controller
        .submit_deploy("standalone", &req_fake_engine("m1"))
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(app.store.deployment_count().unwrap(), 1);
}

/// SPEC §6.1, §6.3 (live M16, M53): an operator's Stop is always accepted.
/// Nothing was ever started, so there is nothing to clean up; the Stop records
/// the operator's intent at once (automatic activation suspended) instead of
/// being refused as a stale-view conflict, and a later Start lifts it. This
/// replaces the earlier pin `stop_is_illegal_from_stopped`, a deliberate change.
// T10
#[tokio::test]
async fn stop_from_stopped_is_recorded_at_once() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let dep = app
        .deploy(
            "m1",
            ModelSource::Local {
                path: "/models/m1".into(),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&dep, LifecycleAction::Stop)
        .await
        .expect("an operator Stop of a stopped deployment was refused");
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&op),
    )
    .await
    .expect("the Stop settles rather than hanging")
    .unwrap();
    assert_eq!(state, LifecycleState::Stopped);
    assert!(app.store.is_admin_stopped(&dep).unwrap());
    let op = app
        .controller
        .request_transition(&dep, LifecycleAction::Start)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&op),
    )
    .await
    .expect("the Start settles rather than hanging")
    .unwrap();
    assert!(!app.store.is_admin_stopped(&dep).unwrap());
}
