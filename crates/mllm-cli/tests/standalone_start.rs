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

/// SPEC §7: standalone derives its limits from the host capacity it observes.
/// The suite states that capacity explicitly (`support::test_memory`), so the
/// policy and the default deployment's footprints are the same on every
/// machine; found 2026-09-23, `a1_gate` and three standalone tests failed on
/// control-host only because little memory was free there.
#[tokio::test]
async fn standalone_limits_follow_the_stated_capacity_not_the_suite_machine() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "stated-capacity",
            ModelSource::Local {
                path: "/models/stated-capacity".into(),
            },
        )
        .expect("standalone creates its own deployment");
    let effective = app
        .store
        .effective_configuration(&id)
        .unwrap()
        .expect("the deployment has an effective revision")
        .effective;
    // 50% of the stated 32 GiB, whatever this machine has free.
    assert_eq!(
        effective["host"]["domains"]["unified"]["managed_limit"],
        support::TEST_CAPACITY_BYTES / 100 * 50,
        "{effective}"
    );
}

/// T03 (SPEC §15.3): a standalone document that states a setting the role
/// would silently ignore is refused at boot, before any side effect, and the
/// refusal names the field. Here an operator-written `server.tls` block that is
/// not the value an older generator wrote: the standalone listeners serve plain
/// HTTP on loopback.
#[tokio::test]
async fn standalone_refuses_a_document_stating_settings_it_does_not_honour() {
    let dir = safe_state_dir();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, dir.path()).unwrap();
    let text = std::fs::read_to_string(&path).unwrap().replace(
        "  listeners:\n",
        "  tls:\n    mode: managed\n    identity_dir: /nowhere\n  listeners:\n",
    );
    std::fs::write(&path, text).unwrap();
    let error = mllm_cli::roles::start_standalone(dir.path())
        .await
        .err()
        .expect("an ignored setting must refuse the boot");
    let said = error.to_string();
    assert!(said.contains("server.tls"), "{said}");
    assert!(
        !dir.path().join("server").join("srv.sqlite3").exists(),
        "refused before the store was opened"
    );
}

/// The standalone document exactly as the generator before this change wrote it
/// (`defaults.rs` at de458e1), with its `server.tls` block.
fn legacy_generated(state: &str) -> String {
    format!(
        "schema_version: 1\n\
         kind: standalone\n\
         name: local\n\
         server:\n\
         \x20 name: local\n\
         \x20 state_dir: \"{state}/server\"\n\
         \x20 listeners:\n\
         \x20   management:\n\
         \x20     bind: \"127.0.0.1:7443\"\n\
         \x20     authentication: admin_token\n\
         \x20   inference:\n\
         \x20     bind: \"127.0.0.1:8443\"\n\
         \x20     authentication: api_key\n\
         \x20 tls:\n\
         \x20   mode: managed\n\
         \x20   identity_dir: \"{state}/identity\"\n\
         host:\n\
         \x20 name: local\n\
         \x20 state_dir: \"{state}/host\"\n\
         \x20 connection: embedded\n\
         \x20 resource_policy:\n\
         \x20   allowed_devices: auto\n\
         \x20   memory:\n\
         \x20     accounting: auto\n\
         \x20     system:\n\
         \x20       managed_limit: auto\n\
         \x20       free_reserve: auto\n\
         \x20 runtime_profiles: {{}}\n"
    )
}

/// T03 (SPEC §15.2, R13): an installation whose `standalone.yaml` an older mllm
/// generated still starts. Its `server.tls` block is reported as ignored, and
/// the file is left byte for byte as it was: generated configuration is never
/// replaced.
#[tokio::test]
async fn standalone_starts_from_a_document_an_older_generator_wrote() {
    let dir = safe_state_dir();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, dir.path()).unwrap();
    let legacy = legacy_generated(&dir.path().to_string_lossy());
    std::fs::write(&path, &legacy).unwrap();

    let app = boot(dir.path()).await;

    let notices = app.config_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("server.tls") && notices[0].contains("ignored"), "{notices:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy, "the file is not rewritten");
}

/// T03 (SPEC §15.3): the current generated document reports nothing ignored.
#[tokio::test]
async fn standalone_reports_nothing_ignored_for_the_current_generated_document() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    assert!(app.config_notices().is_empty(), "{:?}", app.config_notices());
}
