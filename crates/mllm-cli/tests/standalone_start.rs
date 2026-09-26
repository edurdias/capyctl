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
/// the variable that would fix it.
#[tokio::test]
async fn standalone_refuses_to_boot_without_an_engine_installation() {
    let dir = safe_state_dir();
    std::env::remove_var("MLLM_VLLM_BIN");
    std::env::remove_var("MLLM_MODELS_ROOT");

    let error =
        mllm_cli::roles::start_standalone_with_config_home(dir.path(), &dir.path().join(".config"))
            .await
            .err()
            .expect("a host with no engine must refuse to boot");

    assert!(
        matches!(error, mllm_cli::roles::StartError::NoEngineInstallation(_)),
        "{error:?}"
    );
    let said = error.to_string();
    // Owner decision 2026-09-25: the models directory defaults to ~/models,
    // so only the engine is demanded.
    assert!(
        said.contains("MLLM_VLLM_BIN") && !said.contains("MLLM_MODELS_ROOT"),
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
    let error =
        mllm_cli::roles::start_standalone_with_config_home(dir.path(), &dir.path().join(".config"))
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
/// generated still starts. Its `server.tls` block is reported as ignored and
/// kept. T02 (ADR 0019, design §9): the only change the role makes to the file
/// is the one-time move of the old loopback inference default, with the
/// original kept beside it; the role then binds `0.0.0.0:8443`, and a second
/// start changes nothing.
#[tokio::test]
async fn standalone_starts_from_a_document_an_older_generator_wrote() {
    use mllm_config::listener_migration::{Migration, MARKER};
    let dir = safe_state_dir();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, dir.path()).unwrap();
    let legacy = legacy_generated(&dir.path().to_string_lossy());
    std::fs::write(&path, &legacy).unwrap();

    // A restart passes the same engine port range, as an unchanged
    // environment would.
    let ports = support::engine_ports();
    let app = support::try_boot_on(dir.path(), ports).await.unwrap();

    let notices = app.config_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("server.tls") && notices[0].contains("ignored"),
        "{notices:?}"
    );
    let backup = path.with_file_name("standalone.yaml.pre-0.1.0");
    assert_eq!(
        app.listener_migration(),
        &Migration::Rewritten {
            backup: backup.clone()
        }
    );
    assert_eq!(app.inference_bind().to_string(), "0.0.0.0:8443");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        legacy.replace("\"127.0.0.1:8443\"", "\"0.0.0.0:8443\""),
        "only the inference bind changes"
    );
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), legacy);
    assert!(dir.path().join(MARKER).exists());
    let _ = app.shutdown().await;

    let app = support::try_boot_on(dir.path(), ports).await.unwrap();
    assert_eq!(app.listener_migration(), &Migration::NotNeeded);
    assert_eq!(app.inference_bind().to_string(), "0.0.0.0:8443");
    let _ = app.shutdown().await;
}

/// T02 (design §9, `config_migration_failed`): a legacy document that cannot
/// be rewritten unambiguously is left byte for byte as it was, and the role
/// still binds `0.0.0.0:8443` for this run; the API key stays required.
#[tokio::test]
async fn an_ambiguous_legacy_document_binds_the_new_default_without_a_rewrite() {
    use mllm_config::listener_migration::Migration;
    let dir = safe_state_dir();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, dir.path()).unwrap();
    let legacy = format!(
        "# inference was 127.0.0.1:8443\n{}",
        legacy_generated(&dir.path().to_string_lossy())
    );
    std::fs::write(&path, &legacy).unwrap();

    let app = boot(dir.path()).await;

    assert!(
        matches!(app.listener_migration(), Migration::BindOnly { .. }),
        "{:?}",
        app.listener_migration()
    );
    assert_eq!(app.inference_bind().to_string(), "0.0.0.0:8443");
    assert!(!app.api_key().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
    assert!(!path.with_file_name("standalone.yaml.pre-0.1.0").exists());
    let _ = app.shutdown().await;
}

/// T02 T37 (design §9): a new standalone installation binds inference on every
/// interface (the API key stays required); the role reads it from its document.
#[tokio::test]
async fn standalone_binds_inference_on_every_interface_by_default() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    assert_eq!(app.inference_bind().to_string(), "0.0.0.0:8443");
    assert!(!app.api_key().is_empty());
}

/// Serve `router` on a free loopback port and return `GET /v1/models`'s
/// status for each `Authorization` header.
async fn models_status(router: axum::Router, headers: &[Option<String>]) -> Vec<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let client = reqwest::Client::new();
    let mut statuses = Vec::new();
    for header in headers {
        let mut request = client.get(format!("http://{address}/v1/models"));
        if let Some(value) = header {
            request = request.header("authorization", value);
        }
        statuses.push(request.send().await.unwrap().status().as_u16());
    }
    server.abort();
    statuses
}

/// T37 (design §9): a fresh installation serves only its generated
/// per-install key; the constant `mllm-local` key is never accepted.
#[tokio::test]
async fn a_fresh_installation_accepts_no_constant_key() {
    use mllm_cli::exposure::InferenceAuth;
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    assert_eq!(app.inference_auth(), InferenceAuth::ApiKey);
    let generated = std::fs::read_to_string(dir.path().join("identity/credentials"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("api_key: "))
        .unwrap()
        .to_owned();
    assert_eq!(app.api_key(), generated);
    let router = app.inference_router("0.0.0.0:8443".parse().unwrap(), InferenceAuth::ApiKey);
    let statuses = models_status(
        router,
        &[
            None,
            Some("Bearer mllm-local".into()),
            Some(format!("Bearer {generated}")),
        ],
    )
    .await;
    assert_eq!(statuses, [401, 401, 200]);
    let _ = app.shutdown().await;
}

/// T03 T37 (design §9): `listeners.inference.authentication: none` in the
/// document turns the key off; the router then serves without a key.
#[tokio::test]
async fn a_document_may_turn_inference_authentication_off() {
    use mllm_cli::exposure::InferenceAuth;
    let dir = safe_state_dir();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, dir.path()).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("authentication: api_key"), "{text}");
    std::fs::write(
        &path,
        text.replace("authentication: api_key", "authentication: none"),
    )
    .unwrap();
    let app = boot(dir.path()).await;
    assert_eq!(app.inference_auth(), InferenceAuth::None);
    let router = app.inference_router("127.0.0.1:8443".parse().unwrap(), InferenceAuth::None);
    assert_eq!(models_status(router, &[None]).await, [200]);
    let _ = app.shutdown().await;
}

/// T03 (design §9): the document's inference bind is the one the role binds,
/// for example a tailnet address. After the one-time migration has run, the
/// old loopback default is an operator's choice and is honoured as written.
#[tokio::test]
async fn standalone_binds_the_inference_address_its_document_states() {
    use mllm_config::listener_migration::MARKER;
    for bind in ["100.64.0.5:8443", "127.0.0.1:8443"] {
        let dir = safe_state_dir();
        let marker = dir.path().join(MARKER);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "").unwrap();
        let explicit = dir.path().join("explicit.yaml");
        let text = legacy_generated(&dir.path().to_string_lossy())
            .replace("\"127.0.0.1:8443\"", &format!("\"{bind}\""));
        std::fs::write(&explicit, &text).unwrap();
        let app = boot_configured(dir.path(), &explicit)
            .await
            .expect("a valid explicit document boots");
        assert_eq!(app.inference_bind().to_string(), bind);
        assert_eq!(std::fs::read_to_string(&explicit).unwrap(), text);
    }
}

/// T03 (design §9, owner rule): `--listen` wins over `MLLM_INFERENCE_ADDR`,
/// which wins over the document, for the server and standalone roles alike
/// (one function decides both). The deprecated
/// `MLLM_STANDALONE_INFERENCE_ADDR` is read after `MLLM_INFERENCE_ADDR`. Any
/// unicast address with a port is accepted; a multicast address or port 0 is
/// refused, naming the variable.
#[test]
fn listen_beats_environment_beats_document() {
    use mllm_cli::roles::{
        deprecated_inference_env_warning, effective_inference_address,
        DEPRECATED_INFERENCE_ADDR_ENV, INFERENCE_ADDR_ENV,
    };
    use std::net::SocketAddr;
    let standalone: SocketAddr = "0.0.0.0:8443".parse().unwrap();
    // The server's document bind, as the server parses it.
    let server = mllm_config::remote_roles::ServerConfig::parse(
        &mllm_config::remote_roles::ServerConfig::template(std::path::Path::new("/srv/mllm"))
            .replace("0.0.0.0:8443", "100.64.0.9:8443"),
    )
    .unwrap()
    .inference;
    assert_eq!(server.to_string(), "100.64.0.9:8443");
    std::env::remove_var(INFERENCE_ADDR_ENV);
    std::env::remove_var(DEPRECATED_INFERENCE_ADDR_ENV);
    for doc in [standalone, server] {
        assert_eq!(effective_inference_address(doc, None).unwrap(), doc);
        std::env::set_var(INFERENCE_ADDR_ENV, "100.64.0.5:8443");
        assert_eq!(
            effective_inference_address(doc, None).unwrap().to_string(),
            "100.64.0.5:8443"
        );
        assert_eq!(
            effective_inference_address(doc, Some("127.0.0.1:1".parse().unwrap()))
                .unwrap()
                .to_string(),
            "127.0.0.1:1"
        );
        std::env::remove_var(INFERENCE_ADDR_ENV);
    }
    assert_eq!(deprecated_inference_env_warning(None), None);
    std::env::set_var(DEPRECATED_INFERENCE_ADDR_ENV, "127.0.0.1:2");
    assert_eq!(
        effective_inference_address(standalone, None)
            .unwrap()
            .to_string(),
        "127.0.0.1:2"
    );
    assert!(deprecated_inference_env_warning(None)
        .unwrap()
        .contains("MLLM_STANDALONE_INFERENCE_ADDR is deprecated; use MLLM_INFERENCE_ADDR"));
    std::env::set_var(INFERENCE_ADDR_ENV, "127.0.0.1:3");
    assert_eq!(
        effective_inference_address(standalone, None)
            .unwrap()
            .to_string(),
        "127.0.0.1:3"
    );
    assert!(deprecated_inference_env_warning(None)
        .unwrap()
        .contains("ignored"));
    std::env::remove_var(DEPRECATED_INFERENCE_ADDR_ENV);
    for bad in ["224.0.0.1:8443", "0.0.0.0:0", "nonsense"] {
        std::env::set_var(INFERENCE_ADDR_ENV, bad);
        let error = effective_inference_address(standalone, None).unwrap_err();
        assert!(error.to_string().contains(INFERENCE_ADDR_ENV), "{error}");
    }
    std::env::remove_var(INFERENCE_ADDR_ENV);
    assert!(effective_inference_address(standalone, Some("0.0.0.0:0".parse().unwrap())).is_err());
    assert!(effective_inference_address(standalone, Some("224.0.0.1:1".parse().unwrap())).is_err());
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
        // ADR 0019: a refused document is never migrated.
        mllm_config::listener_migration::MARKER,
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
    // ADR 0019, design §9: an explicit document goes through the same
    // one-time migration; nothing but the inference bind changes.
    assert_eq!(
        std::fs::read_to_string(&explicit).unwrap(),
        text.replace("\"127.0.0.1:8443\"", "\"0.0.0.0:8443\""),
        "only the inference bind is rewritten"
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

/// A 16 GB discrete card with 1.5 GiB of desktop use (the discrete-GPU laptop
/// host's shape).
fn discrete_card() -> Option<mllm_agent::gpu_memory::GpuSample> {
    use mllm_agent::gpu_memory::{GpuDevice, GpuMemory, GpuSample};
    Some(GpuSample {
        devices: vec![GpuDevice {
            index: 0,
            uuid: "GPU-00000000-2222-3333-4444-555555555555".into(),
            pci_bus_id: "00000000:01:00.0".into(),
            name: "RTX".into(),
            memory: Some(GpuMemory {
                total_bytes: 16376 << 20,
                used_bytes: 1536 << 20,
                free_bytes: (16376 - 1536) << 20,
            }),
        }],
        // Sampled now: a device reading older than the observation TTL is
        // unobserved, and the host's policy would not publish.
        sampled_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    })
}

/// A model store holding one checkpoint whose weights are `weights` bytes (a
/// sparse file: only its size is read).
fn store_with_checkpoint(weights: u64) -> tempfile::TempDir {
    let store = safe_state_dir();
    let checkpoint = store.path().join("m");
    std::fs::create_dir(&checkpoint).unwrap();
    std::fs::write(checkpoint.join("config.json"), "{}").unwrap();
    std::fs::File::create(checkpoint.join("model.safetensors"))
        .unwrap()
        .set_len(weights)
        .unwrap();
    store
}

/// Design §3, spec §5 (owner decision 2): on a discrete host the generated
/// deployment states a device request sized from the checkpoint's weights,
/// derives its phases on the GPU the picker chooses, and parks to host RAM
/// when the system domain has parked room for the copy; otherwise it parks
/// deep. The suite's 32 GiB host parks 8 GiB: a 3 GiB checkpoint's copy plus
/// the engine's host overhead fits, an 8 GiB one does not.
// T26 T23
#[tokio::test]
async fn a_discrete_standalone_sizes_its_deployment_from_the_checkpoint() {
    let gpu = discrete_card().unwrap().devices[0].memory.clone().unwrap();
    let limits = mllm_cli::standalone_config::device_limits(&gpu, 4);
    // The system domain holds 8 GiB parked here: a 2 GiB model's pinned copy
    // (1.5 x 2 GiB) plus the engine's 4 GiB fits, a 3 GiB one's does not.
    for (weights, residency) in [
        (2_i64 << 30, "host_backed"),
        (3 << 30, "deep"),
        (8 << 30, "deep"),
    ] {
        let dir = safe_state_dir();
        let store = store_with_checkpoint(weights as u64);
        let app = support::try_boot_discrete(dir.path(), discrete_card, store.path(), true)
            .await
            .expect("a discrete host boots");
        let id = app
            .deploy("m", ModelSource::Local { path: "m".into() })
            .expect("a model that fits the card deploys");
        let effective = app
            .store
            .effective_configuration(&id)
            .unwrap()
            .expect("the deployment has an effective revision")
            .effective;
        assert_eq!(effective["residency"], residency, "{effective}");
        let (request, _) = mllm_cli::standalone_config::device_request(
            mllm_config::engine_policy::Engine::Vllm,
            weights,
            limits.managed_limit,
            gpu.total_bytes,
        );
        // The card is charged the request and the engine's CUDA context and
        // graphs (ADR 0019).
        let on_card = request + mllm_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
        let ready = &effective["resources"]["ready"]["allocations"];
        assert_eq!(ready[0]["domain"], "gpu0", "{effective}");
        assert_eq!(ready[0]["bytes"], on_card, "{effective}");
        assert_eq!(ready[1]["domain"], "system", "{effective}");
        // The startup peak on the card is the request: a derived peak of
        // weights x 1.6 plus a margin would not fit a 16 GB card.
        let cold = &effective["resources"]["cold"]["allocations"];
        assert_eq!(cold[0]["bytes"], on_card, "{effective}");
        let _ = app.shutdown().await;
    }
}

/// Spec §3, §11: a model the card can never hold is refused at deploy with the
/// numbers, under `insufficient_device_memory` (exit 4), and nothing is stored.
// T26
#[tokio::test]
async fn a_model_larger_than_the_card_is_refused_at_deploy() {
    let dir = safe_state_dir();
    let store = store_with_checkpoint(16 << 30);
    let app = support::try_boot_discrete(dir.path(), discrete_card, store.path(), true)
        .await
        .expect("a discrete host boots");
    let error = app
        .deploy("m", ModelSource::Local { path: "m".into() })
        .expect_err("a model larger than the card is refused");
    assert!(
        matches!(error, mllm_cli::roles::StartError::Template(_)),
        "{error:?}"
    );
    let structured = mllm_cli::output::StructuredError::from(error);
    assert_eq!(structured.code, "insufficient_device_memory");
    assert_eq!(
        structured.exit_code(),
        mllm_cli::output::ExitCode::INSUFFICIENT_RESOURCES
    );
    assert_eq!(
        app.store.deployment_count().unwrap(),
        0,
        "nothing is stored"
    );
}

/// Review decision (design §3): the KV cache the operator stated with
/// `MLLM_KV_CACHE_BYTES` is honoured on a discrete host when it fits the card
/// with the checkpoint, and refused at deploy with the numbers and the
/// variable when it does not; it is never silently replaced by the template's.
// T26
#[tokio::test]
async fn a_discrete_standalone_honours_a_declared_kv_cache() {
    let dir = safe_state_dir();
    let store = store_with_checkpoint(3 << 30);
    let app = support::try_boot_discrete_with_kv(
        dir.path(),
        discrete_card,
        store.path(),
        true,
        Some("2GiB"),
    )
    .await
    .expect("a discrete host boots");
    let id = app
        .deploy("m", ModelSource::Local { path: "m".into() })
        .expect("a 2 GiB KV cache fits the card beside a 3 GiB checkpoint");
    let effective = app
        .store
        .effective_configuration(&id)
        .unwrap()
        .expect("the deployment has an effective revision")
        .effective;
    assert_eq!(
        effective["engine_config"]["memory"]["kv_cache_bytes"],
        2_i64 << 30,
        "{effective}"
    );
    let _ = app.shutdown().await;

    let dir = safe_state_dir();
    let app = support::try_boot_discrete_with_kv(
        dir.path(),
        discrete_card,
        store.path(),
        true,
        Some("14GiB"),
    )
    .await
    .expect("a discrete host boots");
    let error = app
        .deploy("m", ModelSource::Local { path: "m".into() })
        .expect_err("a 14 GiB KV cache does not fit a 16 GB card beside the weights");
    let text = error.to_string();
    assert!(text.contains("MLLM_KV_CACHE_BYTES"), "{text}");
    let structured = mllm_cli::output::StructuredError::from(error);
    assert_eq!(structured.code, "insufficient_device_memory");
    assert_eq!(
        app.store.deployment_count().unwrap(),
        0,
        "nothing is stored"
    );
}

/// Review decision: a Hugging Face source is not refused on a discrete host
/// for being remote. Standalone decides it exactly as a unified host does, by
/// the host's own `model_sources` policy (allowed by default since the owner
/// decision of 2026-09-25); the sizing never refuses it. On a host that allows the source the
/// template states the KV cache alone and the revision is sized once the
/// download is measured (`standalone_config` tests).
// T26
#[tokio::test]
async fn a_discrete_standalone_treats_a_remote_source_as_a_unified_one_does() {
    let source = || ModelSource::HuggingFace {
        repo: "org/model".into(),
        revision: "0123456789abcdef0123456789abcdef01234567".into(),
        files: vec![],
        token_ref: None,
    };
    // Owner decision 2026-09-25: remote sources are allowed by default, so
    // both hosts accept the deployment; neither refuses it for its size.
    let dir = safe_state_dir();
    let store = store_with_checkpoint(3 << 30);
    let discrete = support::try_boot_discrete(dir.path(), discrete_card, store.path(), true)
        .await
        .expect("a discrete host boots");
    discrete
        .deploy("m", source())
        .expect("a remote source is accepted on a discrete host");
    let _ = discrete.shutdown().await;
    let dir = safe_state_dir();
    let unified = support::try_boot_with_gpu(dir.path(), || None)
        .await
        .expect("a host without a GPU boots");
    unified
        .deploy("m", source())
        .expect("a remote source is accepted on a unified host");
    let _ = unified.shutdown().await;
}

/// ADR 0019 (upgrade of a generated policy), found live on the discrete-GPU
/// laptop host: 0.1.0-rc.4 recorded that machine as one `unified` domain, and
/// the upgraded standalone refused to start (`resource policy revision
/// conflict`). The generated policy is replaced once, with a notice; a
/// deployment whose stored document resolves on the new shape is re-sized for
/// it, one that states the old domain is named with what to do; the second
/// start is quiet.
// T26 T13
#[tokio::test]
async fn an_upgraded_standalone_migrates_its_generated_policy_to_the_discrete_shape() {
    let dir = safe_state_dir();
    let store = store_with_checkpoint(2 << 30);
    let ports = support::engine_ports();
    // The first release saw no discrete card: one `unified` domain.
    let app = support::try_boot_discrete_on(dir.path(), || None, store.path(), ports)
        .await
        .expect("the unified boot");
    let name = app.host_document()["name"].as_str().unwrap().to_owned();
    assert_eq!(
        app.store
            .resource_policy(&name)
            .unwrap()
            .unwrap()
            .context
            .domain_ids
            .into_iter()
            .collect::<Vec<_>>(),
        ["unified"]
    );
    let templated = app
        .deploy("templated", ModelSource::Local { path: "m".into() })
        .expect("the unified template deploys");
    let minimal = serde_json::json!({"name": "minimal", "engine": "local", "model": "m"});
    let minimal = app
        .controller
        .create_configuration(
            "standalone",
            "minimal",
            &serde_json::json!({ "config": minimal }).to_string(),
            &app.host_document(),
        )
        .expect("a minimal document deploys")
        .deployment_id;
    let _ = app.shutdown().await;

    let app = support::try_boot_discrete_on(dir.path(), discrete_card, store.path(), ports)
        .await
        .expect("the upgraded discrete boot migrates instead of refusing");
    let policy = app.store.resource_policy(&name).unwrap().unwrap();
    assert_eq!(
        policy.context.domain_ids.iter().collect::<Vec<_>>(),
        ["gpu0", "system"]
    );
    assert_eq!(policy.revision, 2);
    let notices = app.config_notices().join("\n");
    assert!(
        notices.contains("resource policy mllm generated for it was replaced"),
        "{notices}"
    );
    assert!(
        notices.contains("[unified] are now [gpu0, system]"),
        "{notices}"
    );
    assert!(
        notices.contains("re-sized for this machine's resource policy: minimal"),
        "{notices}"
    );
    assert!(notices.contains("mllm deploy --file"), "{notices}");
    assert!(notices.contains("templated"), "{notices}");
    let effective = app
        .store
        .effective_configuration(&minimal)
        .unwrap()
        .unwrap()
        .effective;
    assert_eq!(
        effective["resources"]["ready"]["allocations"][0]["domain"], "gpu0",
        "{effective}"
    );
    assert_eq!(app.store.current_revision(&templated).unwrap(), Some(1));
    let _ = app.shutdown().await;

    let app = support::try_boot_discrete_on(dir.path(), discrete_card, store.path(), ports)
        .await
        .expect("the next start");
    let notices = app.config_notices().join("\n");
    assert!(
        !notices.contains("was replaced"),
        "one-time notice: {notices}"
    );
    assert_eq!(
        app.store.resource_policy(&name).unwrap().unwrap().revision,
        2
    );
    let _ = app.shutdown().await;
}

/// ADR 0019, SPEC §7, §13.2: an engine left Ready under the previous
/// generated policy holds memory the new shape cannot account for. The
/// upgraded start stops it with the ordinary Stop (verified cleanup releases
/// its charge) before it replaces the policy; nothing is released on the
/// observation alone.
// T26 T13 T33
#[tokio::test]
async fn an_engine_charged_under_the_previous_policy_is_stopped_before_the_migration() {
    let dir = safe_state_dir();
    let store = store_with_checkpoint(2 << 30);
    let ports = support::engine_ports();
    let app = support::try_boot_discrete_on(dir.path(), || None, store.path(), ports)
        .await
        .expect("the unified boot");
    let id = app
        .deploy("held", ModelSource::Local { path: "m".into() })
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        mllm_domain::LifecycleState::Ready
    );
    assert_eq!(app.store.resource_snapshot().unwrap().owners.len(), 1);
    let _ = app.shutdown().await;

    let app = support::try_boot_discrete_on(dir.path(), discrete_card, store.path(), ports)
        .await
        .expect("the upgraded boot stops the engine, then migrates");
    let notices = app.config_notices().join("\n");
    assert!(
        notices.contains("stopped with verified cleanup first: held"),
        "{notices}"
    );
    assert!(
        app.store.resource_snapshot().unwrap().owners.is_empty(),
        "released by the Stop's verified cleanup"
    );
    let name = app.host_document()["name"].as_str().unwrap().to_owned();
    assert_eq!(
        app.store
            .resource_policy(&name)
            .unwrap()
            .unwrap()
            .context
            .domain_ids
            .iter()
            .collect::<Vec<_>>(),
        ["gpu0", "system"]
    );
    let _ = app.shutdown().await;
}

/// ADR 0019 (final review I3): the device domain is charged the request plus
/// the engine's CUDA context and graphs. A card too small for any vLLM
/// template no longer fails the whole start (it did below about 4 GiB, from
/// a sized probe at boot); the host boots and each deployment is refused
/// with `insufficient_device_memory` and its numbers.
// T26
#[tokio::test]
async fn a_card_too_small_for_vllm_boots_and_refuses_the_deployment() {
    use mllm_agent::gpu_memory::{GpuDevice, GpuMemory, GpuSample};
    let small = || {
        Some(GpuSample {
            devices: vec![GpuDevice {
                index: 0,
                uuid: "GPU-00000000-2222-3333-4444-555555555555".into(),
                pci_bus_id: "00000000:01:00.0".into(),
                name: "small".into(),
                memory: Some(GpuMemory {
                    total_bytes: 3 << 30,
                    used_bytes: 0,
                    free_bytes: 3 << 30,
                }),
            }],
            sampled_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
        })
    };
    let dir = safe_state_dir();
    let store = store_with_checkpoint(1 << 30);
    let app = support::try_boot_discrete(dir.path(), small, store.path(), true)
        .await
        .expect("a small card boots");
    let error = app
        .deploy("m", ModelSource::Local { path: "m".into() })
        .expect_err("vLLM cannot fit a 3 GiB card");
    let structured = mllm_cli::output::StructuredError::from(error);
    assert_eq!(structured.code, "insufficient_device_memory");
    let _ = app.shutdown().await;
}

/// T15 (final review M12): the on-demand activation key names the
/// deployment's latest operation (so a request after a failed launch is a
/// new command). Once the first arrival's activation exists, that latest
/// operation changes, so a second arrival derives another key; it must still
/// join the activation in flight rather than start a second one.
// T15
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arrivals_during_one_activation_join_it() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "joined",
            ModelSource::Local {
                path: "/models/joined".into(),
            },
        )
        .unwrap();
    let controller = app.controller.clone();
    let requests: Vec<_> = (0..4)
        .map(|_| {
            let controller = controller.clone();
            let id = id.clone();
            tokio::spawn(async move { controller.activate_for_request(&id).await })
        })
        .collect();
    for request in requests {
        request.await.unwrap().expect("every arrival is served");
    }
    let starts = app.store.operations_of_kind(&id, "initialize").unwrap();
    assert_eq!(
        starts.len(),
        1,
        "one activation for every arrival: {starts:?}"
    );
    let _ = app.shutdown().await;
}

/// T10 T14 (final review M11, found live on the discrete-GPU laptop host): a
/// request for a deployment whose checkpoint is still being measured (its
/// revision is provisional until the digest sizes it, ADR 0014 §7) was
/// answered 429 "no room could be made ... after 3 switch rounds". It is
/// starting: the answer is retryable and says so.
// T10 T14
#[tokio::test]
async fn a_request_while_the_checkpoint_is_measured_is_told_it_is_starting() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let minimal =
        serde_json::json!({"name": "measured", "engine": "local", "model": "/models/measured"});
    let id = app
        .controller
        .create_configuration(
            "standalone",
            "measured",
            &serde_json::json!({ "config": minimal }).to_string(),
            &app.host_document(),
        )
        .expect("a minimal document deploys")
        .deployment_id;
    let revision = app.store.current_revision(&id).unwrap().unwrap();
    assert!(
        app.store
            .checkpoint_digest(&id, revision)
            .unwrap()
            .unwrap()
            .provisional,
        "sized once measured"
    );
    let refused = app
        .controller
        .activate_for_request(&id)
        .await
        .expect_err("not startable until measured");
    match refused {
        mllm_controller::LifecycleFault::Unavailable(message) => {
            assert!(message.contains("is starting"), "{message}");
            assert!(message.contains("being measured"), "{message}");
        }
        other => panic!("a retryable starting answer, not {other:?}"),
    }
    let _ = app.shutdown().await;
}
