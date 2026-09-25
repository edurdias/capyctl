//! F0 exit gate: `mllm start standalone` boots an embedded server+host and
//! drives the full fake lifecycle end-to-end through the durable store.

mod support;

use mllm_cli::roles::App;
use mllm_config::effective::ModelSource;
use mllm_controller::DeployRequest;
use mllm_controller::LifecyclePort as _;
use mllm_domain::{LifecycleAction, LifecycleState};
use support::{boot, boot_deep_parking, safe_state_dir};

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
/// There is no park here. The testkit's installation opts out of deep parking, so
/// standalone declares its deployments restart-only, which SPEC §6.2 makes
/// first-class. The deep-parking host's lifecycle is
/// `a_deep_parking_host_parks_its_vllm_deployment_and_wakes_the_same_launch`.
// T21
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
    // SPEC §6.2: an opted-out host's deployment is restart_only, and the store
    // never parks it: the park is refused and the launch keeps serving.
    let park = app
        .controller
        .request_transition(&dep, LifecycleAction::Park)
        .await;
    if let Ok(op) = park {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            app.controller.wait_terminal(&op),
        )
        .await
        .expect("the refused park settles rather than hanging")
        .expect_err("a restart_only deployment never parks");
    }
    assert_eq!(
        app.store
            .get_deployment(&dep)
            .unwrap()
            .unwrap()
            .observed_state,
        LifecycleState::Ready,
        "a refused park leaves the launch serving"
    );
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

/// ADR 0012, SPEC §6.2: standalone parks exactly as server mode does. On a host
/// that leaves deep parking on (the default), the generated vLLM deployment is
/// `deep`, so an explicit park releases the deployment's Ready footprint down to
/// its parked allocation and the next request wakes the same launch (same
/// endpoint, same per-launch key) instead of starting a new one. A restart_only
/// deployment is never parked by the store, which is the cold stop this guards
/// against. Fake engine only: this is mllm's own decisions, not qualification of
/// the vLLM sleep recipe (SPEC §18); the live proof is pending.
// T21 T16
#[tokio::test]
async fn a_deep_parking_host_parks_its_vllm_deployment_and_wakes_the_same_launch() {
    // The engine group the Fake stands for: two processes this test owns, so
    // the coordinator's check that the recorded group is the one alive after a
    // park and a wake reads real processes.
    struct Group(Vec<std::process::Child>);
    impl Drop for Group {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut group = Group(
        (0..2)
            .map(|_| {
                std::process::Command::new("sleep")
                    .arg("600")
                    .spawn()
                    .expect("a stand-in engine process")
            })
            .collect(),
    );
    let members = vec![
        mllm_testkit::live_identity("api", group.0[0].id()),
        mllm_testkit::live_identity("worker-0", group.0[1].id()),
    ];
    let dir = safe_state_dir();
    let app = boot_deep_parking(dir.path(), members).await;
    let dep = app
        .deploy(
            "m1",
            ModelSource::Local {
                path: "/models/m1".into(),
            },
        )
        .unwrap();
    async fn run(app: &App, dep: &str, action: LifecycleAction, want: LifecycleState) {
        let op = app
            .controller
            .request_transition(dep, action)
            .await
            .unwrap();
        let end = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            app.controller.wait_terminal(&op),
        )
        .await
        .expect("the transition settles rather than hanging")
        .unwrap();
        assert_eq!(end, want, "after {action:?}");
        let row = app.store.get_deployment(dep).unwrap().unwrap();
        assert_eq!(row.observed_state, want, "the durable row after {action:?}");
    }
    let phase = |app: &App| {
        app.store
            .resource_snapshot()
            .unwrap()
            .owners
            .get(&dep)
            .map(|owner| owner.phase)
    };

    run(&app, &dep, LifecycleAction::Start, LifecycleState::Ready).await;
    let launched = app
        .controller
        .runtime_endpoint(&dep)
        .unwrap()
        .expect("a ready deployment has a recorded runtime");
    assert_eq!(
        phase(&app),
        Some(mllm_domain::resources::ResourcePhase::Ready)
    );

    // The park is what a restart_only deployment never gets: the store parks
    // only a parking residency, and sleep mode is derived from it.
    run(&app, &dep, LifecycleAction::Park, LifecycleState::Parked).await;
    assert_eq!(
        phase(&app),
        Some(mllm_domain::resources::ResourcePhase::Parked),
        "the park released the Ready footprint to the parked allocation"
    );

    // On-demand activation wakes the parked launch in place.
    let wake = app.controller.auto_activate(&dep).await.unwrap();
    let end = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&wake),
    )
    .await
    .expect("the wake settles rather than hanging")
    .unwrap();
    assert_eq!(end, LifecycleState::Ready);
    assert_eq!(
        phase(&app),
        Some(mllm_domain::resources::ResourcePhase::Ready)
    );
    let woken = app
        .controller
        .runtime_endpoint(&dep)
        .unwrap()
        .expect("a woken deployment has a recorded runtime");
    assert_eq!(woken.endpoint, launched.endpoint, "the same launch woke");
    assert!(launched.engine_key.is_some(), "a launch carries its key");
    assert_eq!(
        woken.engine_key, launched.engine_key,
        "a wake reuses the launch's per-launch key; a restart would mint another"
    );
    // The coordinator verified the recorded group alive after the park and the
    // wake; the processes are the ones launched, never replaced.
    for child in &mut group.0 {
        assert!(
            child.try_wait().expect("the child is observable").is_none(),
            "the same processes serve after the wake"
        );
    }
}
