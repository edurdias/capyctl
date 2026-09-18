//! Starting a deployment standalone created for itself, and refusing to start at
//! all when this host has no engine to start.
//!
//! Standalone declares a restart-only deployment, so its binding is identified by
//! the recipe and host it was admitted against rather than by a qualification. The
//! start path must accept that identity: a validator that only recognises the
//! qualified shape reports the store as corrupt and no deployment can ever run.

mod support;

use mllm_config::effective::ModelSource;
use mllm_controller::LifecyclePort as _;

use support::{boot, safe_state_dir};

#[tokio::test]
async fn a_declared_deployment_accepts_a_start_command() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "declared-start",
            ModelSource::Local {
                path: "/models/declared-start".into(),
            },
        )
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

/// Spec §8: no engine installation, no boot. A host that came up serving nothing
/// would report itself healthy and refuse every deployment later, at the point
/// where the refusal is hardest to read, so the refusal happens at boot and names
/// the variables that would fix it.
#[tokio::test]
async fn standalone_refuses_to_boot_without_an_engine_installation() {
    let dir = safe_state_dir();
    std::env::remove_var("MLLM_VLLM_BIN");
    std::env::remove_var("MLLM_MODELS_ROOT");

    let error = mllm_cli::roles::start_standalone(dir.path())
        .await
        .err()
        .expect("a host with no engine must refuse to boot");

    assert!(
        matches!(error, mllm_cli::roles::StartError::NoEngineInstallation(_)),
        "{error:?}"
    );
    let said = error.to_string();
    assert!(
        said.contains("MLLM_VLLM_BIN") && said.contains("MLLM_MODELS_ROOT"),
        "the refusal names what it expected: {said}"
    );
}
