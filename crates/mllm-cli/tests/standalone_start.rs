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

use support::{boot, boot_configured, safe_state_dir};

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
    assert!(
        notices[0].contains("server.tls") && notices[0].contains("ignored"),
        "{notices:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        legacy,
        "the file is not rewritten"
    );
}

/// T03 (SPEC §15.3): the current generated document reports nothing ignored.
#[tokio::test]
async fn standalone_reports_nothing_ignored_for_the_current_generated_document() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    assert!(
        app.config_notices().is_empty(),
        "{:?}",
        app.config_notices()
    );
}

/// Nothing a refused explicit document could have produced exists under the
/// state root: no generated document, no credentials, no store.
fn assert_untouched(state: &std::path::Path) {
    for leaf in [
        "config/standalone.yaml",
        "identity/credentials",
        "server/srv.sqlite3",
    ] {
        assert!(!state.join(leaf).exists(), "{leaf} was created");
    }
}

/// T03 (SPEC §15.2, R13): `start standalone --config` naming a file that does
/// not exist refuses, names the path, and is never replaced by the generated
/// default.
#[tokio::test]
async fn an_explicit_standalone_document_that_is_missing_refuses_without_fallback() {
    let dir = safe_state_dir();
    let missing = dir.path().join("elsewhere").join("standalone.yaml");
    let error = boot_configured(dir.path(), &missing)
        .await
        .err()
        .expect("a missing explicit document refuses the boot");
    assert!(
        matches!(error, mllm_cli::roles::StartError::Config(_)),
        "{error:?}"
    );
    assert!(error.to_string().contains("does not exist"), "{error}");
    assert_eq!(
        mllm_cli::output::StructuredError::from(error).code,
        "invalid_config"
    );
    assert_untouched(dir.path());
}

/// T03 (SPEC §§15.2, 15.3, R13): an invalid explicit document (here a
/// duplicate key) refuses, even when a valid implicit document exists; the
/// implicit one is not used in its place and neither file is rewritten.
#[tokio::test]
async fn an_explicit_standalone_document_that_is_invalid_is_not_replaced_by_the_implicit_one() {
    let dir = safe_state_dir();
    let (implicit, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, dir.path()).unwrap();
    let implicit_before = std::fs::read_to_string(&implicit).unwrap();
    let explicit = dir.path().join("explicit.yaml");
    let invalid = "schema_version: 1\nkind: standalone\nname: local\nname: again\n";
    std::fs::write(&explicit, invalid).unwrap();
    let error = boot_configured(dir.path(), &explicit)
        .await
        .err()
        .expect("an invalid explicit document refuses the boot");
    assert!(
        matches!(error, mllm_cli::roles::StartError::Config(_)),
        "{error:?}"
    );
    assert_eq!(std::fs::read_to_string(&explicit).unwrap(), invalid);
    assert_eq!(std::fs::read_to_string(&implicit).unwrap(), implicit_before);
    assert!(!dir.path().join("server/srv.sqlite3").exists());
}

/// T03 (SPEC §15.3): an explicit document naming another state directory is
/// refused before any side effect; the state root comes from the environment.
#[tokio::test]
async fn an_explicit_standalone_document_naming_another_state_dir_refuses() {
    let dir = safe_state_dir();
    let explicit = dir.path().join("explicit.yaml");
    std::fs::write(
        &explicit,
        "schema_version: 1\nkind: standalone\nname: local\nserver:\n  name: local\n  state_dir: /somewhere/else\n",
    )
    .unwrap();
    let error = boot_configured(dir.path(), &explicit)
        .await
        .err()
        .expect("another state directory refuses the boot");
    assert!(error.to_string().contains("server.state_dir"), "{error}");
    assert_untouched(dir.path());
}

/// T02 T03 (SPEC §15.2, R13): a valid explicit document is the one honoured.
/// On a state root that has never served, the boot creates the protected
/// credentials once and the store, but generates no implicit document. The
/// explicit document here is the legacy generated shape, whose `server.tls`
/// block is reported as ignored, which shows it is the file that was read.
#[tokio::test]
async fn standalone_honours_a_valid_explicit_document() {
    let dir = safe_state_dir();
    let explicit = dir.path().join("explicit.yaml");
    let text = legacy_generated(&dir.path().to_string_lossy());
    std::fs::write(&explicit, &text).unwrap();

    let app = boot_configured(dir.path(), &explicit)
        .await
        .expect("a valid explicit document boots");

    let notices = app.config_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("server.tls"), "{notices:?}");
    assert_eq!(
        std::fs::read_to_string(&explicit).unwrap(),
        text,
        "not rewritten"
    );
    assert!(
        !dir.path().join("config/standalone.yaml").exists(),
        "no implicit document is generated beside an explicit one"
    );
    let credentials = dir.path().join("identity/credentials");
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        std::fs::metadata(&credentials)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!app.api_key().is_empty() && app.api_key() != "mllm-local");
}

/// T03 T33 (SPEC §15.2): a state root that has served and lost its
/// credentials is not repaired by an explicit document; it refuses for
/// recovery instead of minting a new identity.
#[tokio::test]
async fn an_explicit_document_does_not_recreate_lost_credentials() {
    let dir = safe_state_dir();
    let explicit = dir.path().join("explicit.yaml");
    std::fs::write(
        &explicit,
        "schema_version: 1\nkind: standalone\nname: local\n",
    )
    .unwrap();
    let app = boot_configured(dir.path(), &explicit)
        .await
        .expect("first boot");
    let _ = app.shutdown().await;
    std::fs::remove_file(dir.path().join("identity/credentials")).unwrap();
    let error = boot_configured(dir.path(), &explicit)
        .await
        .err()
        .expect("lost credentials refuse the boot");
    assert!(
        matches!(error, mllm_cli::roles::StartError::MissingCredentials),
        "{error:?}"
    );
    assert!(!dir.path().join("identity/credentials").exists());
}

/// Design §1: integrated and discrete GPUs on one host are refused at boot
/// rather than published as a guess, and the refusal names its code.
// T26
#[tokio::test]
async fn standalone_refuses_mixed_integrated_and_discrete_gpus() {
    use mllm_agent::gpu_memory::{GpuDevice, GpuMemory, GpuSample};
    let dir = safe_state_dir();
    let device = |index: u32, memory: Option<GpuMemory>| GpuDevice {
        index,
        uuid: format!("GPU-{index:08}-2222-3333-4444-555555555555"),
        pci_bus_id: format!("00000000:0{index}:00.0"),
        name: "GPU".into(),
        memory,
    };
    let mixed = move || {
        Some(GpuSample {
            devices: vec![
                device(0, None),
                device(
                    1,
                    Some(GpuMemory {
                        total_bytes: 16 << 30,
                        used_bytes: 0,
                        free_bytes: 16 << 30,
                    }),
                ),
            ],
            sampled_at_ms: 1,
        })
    };
    let error = support::try_boot_with_gpu(dir.path(), mixed)
        .await
        .err()
        .expect("a mixed host must refuse to boot");
    assert!(
        matches!(error, mllm_cli::roles::StartError::GpuTopology(_)),
        "{error:?}"
    );
    assert!(error.to_string().starts_with("unsupported_gpu_topology"));
    let structured = mllm_cli::output::StructuredError::from(error);
    assert_eq!(structured.code, "unsupported_gpu_topology");
}

/// Design §1: a unified host (every device integrated) boots with today's
/// single `unified` domain.
// T26
#[tokio::test]
async fn standalone_on_a_unified_host_keeps_the_unified_shape() {
    use mllm_agent::gpu_memory::{GpuDevice, GpuSample, HostShape};
    let dir = safe_state_dir();
    let unified = || {
        Some(GpuSample {
            devices: vec![GpuDevice {
                index: 0,
                uuid: "GPU-00000000-2222-3333-4444-555555555555".into(),
                pci_bus_id: "0000000F:01:00.0".into(),
                name: "GB10".into(),
                memory: None,
            }],
            sampled_at_ms: 1,
        })
    };
    let app = support::try_boot_with_gpu(dir.path(), unified)
        .await
        .expect("a unified host boots");
    assert_eq!(app.gpu_shape, HostShape::Unified);
}
