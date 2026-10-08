//! Owner decisions 2026-09-25: standalone keeps models in `~/models` unless
//! `--models-root`, `CAPYCTL_MODELS_ROOT` or `host.model_store.path` names
//! another, and it downloads declared Hugging Face and HTTP sources by
//! default into `~/models/sources` (owner ruling: under the models
//! directory, so copies from earlier releases are reused), exactly as an
//! enrolled host does (standalone is a server plus one host).
//!
//! These tests set `HOME` and the model variables, so they live in their own
//! binary and run one at a time. No test reaches the network: downloads are
//! served from a loopback origin, or from one nothing listens on. Fake
//! installations only; nothing here qualifies an engine recipe (SPEC §18).

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use capyctl_cli::roles::ModelOverrides;
use capyctl_config::effective::ModelSource;
use capyctl_config::model_source::{Archive, SourceSwitch};
use capyctl_store::model_sources::SourceState;
use sha2::Digest as _;

/// The variables and `HOME` are process-wide: one test at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A private home directory for one test, set as `HOME` with the model
/// variables cleared. Returned so it outlives the test.
fn fake_home() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    static REAL_HOME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let real = REAL_HOME.get_or_init(|| std::env::var("HOME").expect("HOME is set"));
    let home = tempfile::TempDir::new_in(real).unwrap();
    // The controller lock refuses a group- or other-writable ancestor.
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_var("HOME", home.path());
    for name in [
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_MODEL_SOURCES",
        "CAPYCTL_MODEL_SOURCES_MAX",
        "XDG_CONFIG_HOME",
    ] {
        std::env::remove_var(name);
    }
    home
}

fn state_in(home: &Path) -> tempfile::TempDir {
    tempfile::TempDir::new_in(home).unwrap()
}

/// An installation that names no models directory, as `CAPYCTL_MODELS_ROOT`
/// unset does.
fn unnamed() -> Option<PathBuf> {
    Some(PathBuf::new())
}

fn no_gpu() -> Option<capyctl_agent::gpu_memory::GpuSample> {
    None
}

fn discrete_card() -> Option<capyctl_agent::gpu_memory::GpuSample> {
    use capyctl_agent::gpu_memory::{GpuDevice, GpuMemory, GpuSample};
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
        sampled_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    })
}

fn hugging_face() -> ModelSource {
    ModelSource::HuggingFace {
        repo: "org/model".into(),
        revision: "0123456789abcdef0123456789abcdef01234567".into(),
        files: vec![],
        token_ref: None,
    }
}

/// A standalone document at `<state>/standalone.yaml` whose `host:` block is
/// `host` (YAML).
fn document(state: &Path, host: &str) -> PathBuf {
    let path = state.join("standalone.yaml");
    std::fs::write(
        &path,
        format!("schema_version: 1\nkind: standalone\nname: local\nhost:\n{host}"),
    )
    .unwrap();
    path
}

// T14 T03 (owner decision 2026-09-25): standalone starts without
// CAPYCTL_MODELS_ROOT. The models directory is ~/models (created), downloads
// are allowed with the 500 GiB ceiling into ~/models/sources.
#[tokio::test]
async fn standalone_starts_without_a_models_root_and_uses_home_models() {
    let _serial = SERIAL.lock().await;
    let home = fake_home();
    let state = state_in(home.path());
    let app = support::boot_with_models(
        state.path(),
        None,
        unnamed(),
        &ModelOverrides::default(),
        None,
        no_gpu,
    )
    .await
    .expect("standalone starts with no models directory named");
    let host = app.host_document();
    let models = home.path().join("models");
    assert_eq!(host["model_store"]["path"], models.to_str().unwrap());
    assert!(models.is_dir(), "the default models directory is created");
    let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
    assert_eq!(policy.model_sources.huggingface, SourceSwitch::Allowed);
    assert_eq!(policy.model_sources.http, SourceSwitch::Allowed);
    assert_eq!(policy.model_sources.max_bytes, Some(500 << 30));
    assert_eq!(policy.model_sources.root(&policy.model_store), models);
    let _ = app.shutdown().await;
}

// T14 T03 (owner rule 2026-09-25): flag > environment > document > default,
// for the models directory and for the source switch and ceiling.
#[tokio::test]
async fn standalone_model_settings_follow_flag_env_document_default() {
    let _serial = SERIAL.lock().await;
    let home = fake_home();
    let (yaml_root, env_root, flag_root) = (
        state_in(home.path()),
        state_in(home.path()),
        state_in(home.path()),
    );
    let boot = |flags: ModelOverrides| {
        let yaml_root = yaml_root.path().to_path_buf();
        async move {
            let state = state_in(Path::new(&std::env::var("HOME").unwrap()));
            let config = document(
                state.path(),
                &format!(
                    "  model_store:\n    path: {}\n  model_sources:\n    huggingface: disabled\n    http: disabled\n    max_bytes: 100GiB\n",
                    yaml_root.display()
                ),
            );
            let app = support::boot_with_models(
                state.path(),
                Some(&config),
                unnamed(),
                &flags,
                None,
                no_gpu,
            )
            .await
            .expect("standalone starts");
            let host = app.host_document();
            let _ = app.shutdown().await;
            capyctl_config::effective::normalize_host_policy(&host).unwrap()
        }
    };
    // The document alone: its directory, and sources off as it says.
    let policy = boot(ModelOverrides::default()).await;
    assert_eq!(policy.model_store, yaml_root.path());
    assert_eq!(policy.model_sources.huggingface, SourceSwitch::Denied);
    assert_eq!(policy.model_sources.max_bytes, Some(100 << 30));
    // The environment wins over the document.
    std::env::set_var("CAPYCTL_MODELS_ROOT", env_root.path());
    std::env::set_var("CAPYCTL_MODEL_SOURCES", "allowed");
    std::env::set_var("CAPYCTL_MODEL_SOURCES_MAX", "200GiB");
    let policy = support::boot_with_models(
        state_in(home.path()).path(),
        None,
        // The environment's installation names CAPYCTL_MODELS_ROOT.
        Some(env_root.path().to_path_buf()),
        &ModelOverrides::default(),
        None,
        no_gpu,
    )
    .await
    .map(|app| app.host_document())
    .map(|host| capyctl_config::effective::normalize_host_policy(&host).unwrap())
    .unwrap();
    assert_eq!(policy.model_store, env_root.path());
    assert_eq!(policy.model_sources.http, SourceSwitch::Allowed);
    assert_eq!(policy.model_sources.max_bytes, Some(200 << 30));
    let policy = boot(ModelOverrides::default()).await;
    assert_eq!(policy.model_sources.huggingface, SourceSwitch::Allowed);
    assert_eq!(policy.model_sources.max_bytes, Some(200 << 30));
    // The flags win over both.
    let policy = boot(ModelOverrides {
        models_root: Some(flag_root.path().to_path_buf()),
        sources: Some(SourceSwitch::Denied),
        sources_max: Some("300GiB".into()),
        ..Default::default()
    })
    .await;
    assert_eq!(policy.model_store, flag_root.path());
    assert_eq!(policy.model_sources.huggingface, SourceSwitch::Denied);
    assert_eq!(policy.model_sources.http, SourceSwitch::Denied);
    assert_eq!(policy.model_sources.max_bytes, Some(300 << 30));
    // A malformed variable refuses the start.
    std::env::set_var("CAPYCTL_MODEL_SOURCES", "sometimes");
    let error = support::boot_with_models(
        state_in(home.path()).path(),
        None,
        unnamed(),
        &ModelOverrides::default(),
        None,
        no_gpu,
    )
    .await
    .err()
    .expect("a malformed switch is refused");
    assert!(
        error.to_string().contains("CAPYCTL_MODEL_SOURCES"),
        "{error}"
    );
    for name in [
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_MODEL_SOURCES",
        "CAPYCTL_MODEL_SOURCES_MAX",
    ] {
        std::env::remove_var(name);
    }
}

// T14 T26 (owner decision 2026-09-25, ADR 0014 §7): a Hugging Face
// deployment on a discrete standalone host is accepted provisionally with
// its source pending on the embedded host; the download (here against an
// origin nothing listens on) never reaches the network.
#[tokio::test]
async fn a_hugging_face_deployment_is_accepted_provisionally() {
    let _serial = SERIAL.lock().await;
    let home = fake_home();
    let state = state_in(home.path());
    let app = support::boot_with_models(
        state.path(),
        None,
        unnamed(),
        &ModelOverrides::default(),
        None,
        discrete_card,
    )
    .await
    .expect("a discrete host boots");
    let id = app
        .deploy("m", hugging_face())
        .expect("a Hugging Face source is allowed by default");
    let revision = app.store.current_revision(&id).unwrap().unwrap();
    let digest = app
        .store
        .checkpoint_digest(&id, revision)
        .unwrap()
        .expect("a digest is pending");
    assert!(digest.provisional, "sized once the download is measured");
    let source = app.store.model_sources(&id, revision).unwrap();
    assert_eq!(source.len(), 1, "{source:?}");
    assert_ne!(source[0].state, SourceState::Verified);
    let _ = app.shutdown().await;
}

// T14 (owner decision 2026-09-25): a document that turns a source kind off
// keeps it off; the deployment is refused before anything is stored.
#[tokio::test]
async fn an_explicitly_disabled_source_is_refused() {
    let _serial = SERIAL.lock().await;
    let home = fake_home();
    let state = state_in(home.path());
    let config = document(
        state.path(),
        "  model_sources:\n    huggingface: disabled\n",
    );
    let app = support::boot_with_models(
        state.path(),
        Some(&config),
        unnamed(),
        &ModelOverrides::default(),
        None,
        no_gpu,
    )
    .await
    .expect("standalone starts");
    let error = app
        .deploy("m", hugging_face())
        .expect_err("disabled stays disabled");
    assert!(error.to_string().contains("model_sources"), "{error}");
    let _ = app.shutdown().await;
}

// T14 T34 (ADR 0008, owner decision 2026-09-25): the embedded host
// materializes a declared source into ~/models/sources, as an
// enrolled host does; the revision resolves to that copy, which the
// checkpoint digest measures (ADR 0014 §7) once the Fake-free role runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_embedded_host_downloads_into_the_sources_store() {
    let _serial = SERIAL.lock().await;
    let home = fake_home();
    let state = state_in(home.path());
    let weights = vec![5_u8; 4096];
    let sha: String = sha2::Sha256::digest(&weights)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let served = weights.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app_server = axum::Router::new().route(
        "/w.bin",
        axum::routing::get(move || {
            let served = served.clone();
            async move { served }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app_server).await.unwrap() });
    let app = support::boot_with_models(
        state.path(),
        None,
        unnamed(),
        &ModelOverrides::default(),
        Some(origin),
        no_gpu,
    )
    .await
    .expect("standalone starts");
    let id = app
        .deploy(
            "m",
            ModelSource::Http {
                url: Some("https://weights.example.test/w.bin".into()),
                url_ref: None,
                sha256: sha.clone(),
                archive: Archive::None,
            },
        )
        .expect("an HTTP source is allowed by default");
    let revision = app.store.current_revision(&id).unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !app
        .store
        .model_sources(&id, revision)
        .unwrap()
        .iter()
        .any(|record| record.state == SourceState::Verified)
    {
        assert!(
            Instant::now() < deadline,
            "not materialized: {:?}",
            app.store.model_sources(&id, revision).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let copy = home.path().join(format!("models/sources/http/{sha}/w.bin"));
    assert_eq!(std::fs::read(&copy).unwrap(), weights);
    // The revision loads from that copy, and its checkpoint is contained in
    // the sources store under the models directory.
    let pending = app.store.pending_model_sources().unwrap();
    assert!(pending.is_empty(), "{pending:?}");
    let host = app.host_document();
    let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
    let measured = capyctl_agent::checkpoint::CheckpointVerifier::in_memory()
        .measure(
            policy.model_sources.root(&policy.model_store),
            copy.parent().unwrap(),
        )
        .expect("the copy measures inside the sources store");
    assert_eq!(measured.manifest.weights_bytes, weights.len() as i64);
    assert!(
        !state.path().join("models").exists(),
        "downloads stay out of the state directory"
    );
    let _ = app.shutdown().await;
}

// T14 T34 (ADR 0008, owner ruling 2026-09-25): a verified Hugging Face copy
// already in <model_store>/sources, as an earlier release left it, is reused
// after the upgrade: the source is verified from the existing copy, and
// nothing is fetched (the origin is one nothing listens on).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_existing_download_in_the_models_directory_is_reused() {
    let _serial = SERIAL.lock().await;
    let home = fake_home();
    let state = state_in(home.path());
    let source = hugging_face();
    let key = source.store_key().unwrap();
    assert_eq!(
        key,
        "sources/huggingface/org--model@0123456789abcdef0123456789abcdef01234567"
    );
    let models = home.path().join("models");
    let copy = models.join(&key);
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("config.json"), b"{}").unwrap();
    std::fs::write(copy.join("model.safetensors"), vec![1_u8; 64]).unwrap();
    // The store's verified marker, as the earlier release committed it.
    let id: String = sha2::Sha256::digest(key.as_bytes())[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let markers = models.join("sources/.capyctl");
    std::fs::create_dir_all(&markers).unwrap();
    std::fs::write(
        markers.join(format!("{id}.verified")),
        serde_json::json!({"key": key, "state": "verified", "bytes": 66, "files": 2}).to_string(),
    )
    .unwrap();
    let app = support::boot_with_models(
        state.path(),
        None,
        unnamed(),
        &ModelOverrides::default(),
        None,
        no_gpu,
    )
    .await
    .expect("standalone starts");
    let id = app
        .deploy("m", source)
        .expect("a Hugging Face source is allowed by default");
    let revision = app.store.current_revision(&id).unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let records = app.store.model_sources(&id, revision).unwrap();
        if records
            .iter()
            .any(|record| record.state == SourceState::Verified)
        {
            break;
        }
        assert!(
            records
                .iter()
                .all(|record| record.state != SourceState::Failed),
            "the existing copy was not reused: {records:?}"
        );
        assert!(Instant::now() < deadline, "not verified: {records:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!markers.join("partial").exists(), "no download was started");
    assert!(!state.path().join("models").exists());
    let _ = app.shutdown().await;
}
