//! Starting a deployment standalone created for itself.
//!
//! Standalone declares a restart-only deployment, so its binding is identified by
//! the recipe and host it was admitted against rather than by a qualification. The
//! start path must accept that identity: a validator that only recognises the
//! qualified shape reports the store as corrupt and no deployment can ever run.

use mllm_controller::LifecyclePort as _;

fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

#[tokio::test]
async fn a_declared_deployment_accepts_a_start_command() {
    let dir = safe_state_dir();
    let app = mllm_cli::roles::start_standalone(dir.path())
        .await
        .expect("standalone boots");
    let id = app
        .deploy("declared-start", "/models/declared-start")
        .expect("standalone creates its own deployment");

    let started = app
        .controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await;

    // The command's own acceptance is what is under test. What the runtime does
    // afterwards is the launcher's business and is covered elsewhere.
    match started {
        Ok(_) => {}
        Err(error) => panic!("start was refused before it reached the runtime: {error:?}"),
    }
}
