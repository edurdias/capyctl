//! ADR 0018 §5: standalone runs engine registration in one process: the
//! same engines.yaml write, the same socket, the same retirement. Fake-engine
//! tests; not qualification.
mod support;
use capyctl_agent::control_socket::{request, ControlRequest, SOCKET_NAME};
use capyctl_config::registration::{
    engines_beside, lock_engines, write_engines, EnginesFile, ProfileSpec,
};
use capyctl_controller::LifecyclePort as _;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn standalone_doc(state: &std::path::Path) -> std::path::PathBuf {
    let config = state.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = config.join("standalone.yaml");
    std::fs::write(
        &path,
        "schema_version: 1\nkind: standalone\nname: local\nhost:\n  name: local\n  runtime_profiles: {}\n",
    )
    .unwrap();
    path
}

/// `engine add`'s write: `name` into engines.yaml beside the standalone document.
fn register(document: &std::path::Path, name: &str) {
    let path = engines_beside(document);
    let lock = lock_engines(&path).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert(
        name.into(),
        capyctl_config::registration::profile_document(&ProfileSpec {
            engine: capyctl_config::engine_policy::Engine::Vllm,
            executable: "/bin/true".into(),
            build_fingerprint: "0.29.0".into(),
            deep_park: false,
            installation_drift: capyctl_config::effective::InstallationDrift::Warn,
            args: vec![],
            cuda_home: None,
        }),
    );
    write_engines(&engines, &lock, None).unwrap();
}

/// `engine remove`'s write: `name` out of engines.yaml beside the document.
fn unregister(document: &std::path::Path, name: &str) {
    let path = engines_beside(document);
    let lock = lock_engines(&path).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.remove(name);
    write_engines(&engines, &lock, None).unwrap();
}

/// Deploy `name` on `profile` through the management API, as `capyctl deploy`
/// does: the embedded configuration source composes it against the host
/// document the role publishes now.
async fn deploy(
    app: &capyctl_cli::roles::App,
    state: &std::path::Path,
    name: &str,
    profile: &str,
) -> (reqwest::StatusCode, serde_json::Value) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let credentials = std::fs::read_to_string(state.join("identity/credentials")).unwrap();
    let admin = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .unwrap()
        .to_owned();
    let config = capyctl_cli::standalone_config::deployment_document(
        name,
        name,
        &capyctl_config::effective::ModelSource::Local {
            path: format!("/models/{name}"),
        },
        capyctl_config::engine_policy::Engine::Vllm,
        &capyctl_cli::standalone_config::TemplateMemory::Unified {
            capacity_bytes: support::TEST_CAPACITY_BYTES,
        },
        capyctl_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        false,
        profile,
    )
    .expect("the unified template");
    let response = reqwest::Client::new()
        .post(format!("http://{address}/management/v1/deployments"))
        .bearer_auth(admin)
        .header("idempotency-key", ulid::Ulid::new().to_string())
        .json(&serde_json::json!({"config": config, "activate": false}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.json().await.unwrap_or(serde_json::Value::Null);
    server.abort();
    (status, body)
}

// T07 (ADR 0018 §3, §5): a profile added while standalone runs is published
// without a restart and a deployment can name it; the socket is owner-only.
#[tokio::test]
async fn standalone_add_is_usable_without_restart() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    let app = support::boot_configured(state.path(), &document)
        .await
        .expect("standalone boots");
    assert_eq!(app.profiles(), vec!["local".to_string()]);
    let socket = state.path().join(SOCKET_NAME);
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let (status, body) = deploy(&app, state.path(), "patched-model", "vllm-patched").await;
    assert!(!status.is_success(), "not published yet: {status} {body}");
    register(&document, "vllm-patched");
    let reply = request(&socket, &ControlRequest::Add, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(reply["published"], "published", "{reply}");
    assert_eq!(
        app.profiles(),
        vec!["local".to_string(), "vllm-patched".to_string()]
    );
    assert!(app.host_document()["runtime_profiles"]["vllm-patched"].is_object());
    let (status, body) = deploy(&app, state.path(), "patched-model", "vllm-patched").await;
    assert!(status.is_success(), "{status} {body}");
    // Nothing changed on disk: a second add is `unchanged`.
    let again = request(&socket, &ControlRequest::Add, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(again["published"], "unchanged", "{again}");
    // `list` reports both profiles and who uses them.
    let listed = request(&socket, &ControlRequest::List, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(listed["accepted"]["vllm-patched"].is_object(), "{listed}");
    assert!(listed["accepted"]["local"].is_object(), "{listed}");
    let _ = app.shutdown().await;
}

// T02 T16 T32 (ADR 0018 §4, A2): the last registered profile is removed; the
// role keeps running with none, and a profile added again is published live.
#[tokio::test]
async fn the_last_profile_is_removed_and_a_new_one_is_published_live() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "only");
    let app = support::boot_registered_only(state.path(), &document)
        .await
        .expect("standalone boots");
    assert_eq!(app.profiles(), vec!["only".to_string()]);
    let socket = state.path().join(SOCKET_NAME);
    let reply = request(
        &socket,
        &ControlRequest::Remove {
            profile: "only".into(),
            drain: false,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(reply, serde_json::json!({"ok": true, "retired": true}));
    unregister(&document, "only");
    let reload = request(&socket, &ControlRequest::Add, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(reload["published"], "published", "{reload}");
    assert!(app.profiles().is_empty());
    register(&document, "again");
    let reload = request(&socket, &ControlRequest::Add, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(reload["published"], "published", "{reload}");
    let (status, body) = deploy(&app, state.path(), "m", "again").await;
    assert!(status.is_success(), "{status} {body}");
    let _ = app.shutdown().await;
}

// T16 T32 (ADR 0018 §4, §5; review decision C1): an unused registered
// profile is retired without the role writing anything; the CLI's rewrite
// and reload unpublish it; an environment profile cannot be removed; the
// name can be registered again afterwards.
#[tokio::test]
async fn standalone_remove_retires_and_the_reload_unpublishes() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "vllm-patched");
    let app = support::boot_configured(state.path(), &document)
        .await
        .expect("standalone boots");
    assert_eq!(
        app.profiles(),
        vec!["local".to_string(), "vllm-patched".to_string()]
    );
    let socket = state.path().join(SOCKET_NAME);
    let reply = request(
        &socket,
        &ControlRequest::Remove {
            profile: "vllm-patched".into(),
            drain: false,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(reply, serde_json::json!({"ok": true, "retired": true}));
    assert!(EnginesFile::load(&engines_beside(&document))
        .unwrap()
        .profiles
        .contains_key("vllm-patched"));
    unregister(&document, "vllm-patched");
    let reload = request(&socket, &ControlRequest::Add, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(reload["published"], "published", "{reload}");
    assert_eq!(app.profiles(), vec!["local".to_string()]);
    let refused = request(
        &socket,
        &ControlRequest::Remove {
            profile: "local".into(),
            drain: false,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(refused["code"], "invalid_config");
    // The confirmed retirement does not keep the name out: registered again,
    // it is published and placeable.
    register(&document, "vllm-patched");
    let reply = request(&socket, &ControlRequest::Add, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(reply["published"], "published", "{reply}");
    let (status, body) = deploy(&app, state.path(), "again-model", "vllm-patched").await;
    assert!(status.is_success(), "{status} {body}");
    let id = body["deployment_id"].as_str().unwrap().to_owned();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        capyctl_domain::LifecycleState::Ready,
        "placement is open on the re-registered profile"
    );
    let _ = app.shutdown().await;
}

// T16 T32 (ADR 0018 §4): a profile a deployment uses is not removed without
// --drain; engines.yaml keeps it and it stays published.
#[tokio::test]
async fn standalone_remove_in_use_is_refused_with_the_list() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "vllm-patched");
    let app = support::boot_configured(state.path(), &document)
        .await
        .expect("standalone boots");
    let (status, body) = deploy(&app, state.path(), "busy-model", "vllm-patched").await;
    assert!(status.is_success(), "{status} {body}");
    let id = body["deployment_id"].as_str().unwrap().to_owned();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        capyctl_domain::LifecycleState::Ready,
        "the deployment starts on the registered profile"
    );
    let mut listed = serde_json::Value::Null;
    for _ in 0..200 {
        listed = request(
            &state.path().join(SOCKET_NAME),
            &ControlRequest::List,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        if listed["users"]["vllm-patched"].is_array() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        listed["users"]["vllm-patched"],
        serde_json::json!(["busy-model"])
    );
    let reply = request(
        &state.path().join(SOCKET_NAME),
        &ControlRequest::Remove {
            profile: "vllm-patched".into(),
            drain: false,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(reply["code"], "profile_in_use", "{reply}");
    assert_eq!(reply["deployments"], serde_json::json!(["busy-model"]));
    assert!(EnginesFile::load(&engines_beside(&document))
        .unwrap()
        .profiles
        .contains_key("vllm-patched"));
    assert!(app.profiles().contains(&"vllm-patched".to_string()));
    let _ = app.shutdown().await;
}

// T03 (ADR 0018 §5): a registered profile named like an environment profile
// is refused at start with `profile_exists`.
#[tokio::test]
async fn a_name_in_both_is_refused_at_start() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "local");
    let refused = match support::boot_configured(state.path(), &document).await {
        Ok(_) => panic!("a name declared twice must refuse the boot"),
        Err(error) => error,
    };
    let structured: capyctl_cli::output::StructuredError = refused.into();
    assert_eq!(structured.code, "profile_exists", "{}", structured.message);
}

/// Deploy `name` on `profile` and start it to Ready; its deployment id.
async fn ready_on(
    app: &capyctl_cli::roles::App,
    state: &std::path::Path,
    name: &str,
    profile: &str,
) -> String {
    let (status, body) = deploy(app, state, name, profile).await;
    assert!(status.is_success(), "{status} {body}");
    let id = body["deployment_id"].as_str().unwrap().to_owned();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    id
}

// T16 T32 (ADR 0018 §4, §5; review decision I3): after a drained removal
// the deployment that used the profile still exists, stopped; starting it
// again must not place the removed engine on the embedded host.
#[tokio::test]
async fn a_removed_profile_never_starts_again() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "vllm-patched");
    let app = support::boot_configured(state.path(), &document)
        .await
        .expect("standalone boots");
    let id = ready_on(&app, state.path(), "busy-model", "vllm-patched").await;
    let socket = state.path().join(SOCKET_NAME);
    let reply = request(
        &socket,
        &ControlRequest::Remove {
            profile: "vllm-patched".into(),
            drain: true,
        },
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    // The operator's CLI writes engines.yaml, then asks for the reload.
    if EnginesFile::load(&engines_beside(&document))
        .unwrap()
        .profiles
        .contains_key("vllm-patched")
    {
        unregister(&document, "vllm-patched");
    }
    let reload = request(&socket, &ControlRequest::Add, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(reload["ok"], true, "{reload}");
    assert_eq!(app.profiles(), vec!["local".to_string()]);
    let started = match app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
    {
        Ok(op) => app.controller.wait_terminal(&op).await.ok(),
        Err(_) => None,
    };
    assert_ne!(
        started,
        Some(capyctl_domain::LifecycleState::Ready),
        "the removed profile was placed again"
    );
    let _ = app.shutdown().await;
}

// T03 T16 (ADR 0018 §4, §5; review decision I3): engines.yaml losing a
// published profile outside `engine remove` is not published by a reload:
// the profile stays published until it is retired.
#[tokio::test]
async fn a_reload_never_drops_a_published_profile() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "vllm-patched");
    let app = support::boot_configured(state.path(), &document)
        .await
        .expect("standalone boots");
    unregister(&document, "vllm-patched");
    let reply = request(
        &state.path().join(SOCKET_NAME),
        &ControlRequest::Add,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(reply["code"], "publish_rejected", "{reply}");
    assert!(
        reply["message"].as_str().unwrap().contains("engine remove"),
        "{reply}"
    );
    assert!(app.profiles().contains(&"vllm-patched".to_string()));
    let _ = app.shutdown().await;
}

// T32 (ADR 0018 §4; review decision I2): a retirement left standing by a
// standalone that stopped mid-drain is expired once past its deadline, so the
// profile is placeable again and not wedged.
#[tokio::test]
async fn a_stale_retirement_expires_when_standalone_starts() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "vllm-patched");
    let ports = support::engine_ports();
    let app = support::boot_configured_on(state.path(), &document, ports)
        .await
        .expect("standalone boots");
    let id = ready_on(&app, state.path(), "busy-model", "vllm-patched").await;
    let host = app.host_document()["name"].as_str().unwrap().to_owned();
    let _ = app.shutdown().await;
    {
        // The role stopped while a drained retirement waited on its stop.
        let store =
            capyctl_store::Store::open(&state.path().join("server").join("srv.sqlite3")).unwrap();
        let start = store
            .begin_profile_retirement(&host, "vllm-patched", "stale", 1, 2, true)
            .unwrap();
        assert!(
            matches!(
                start,
                capyctl_store::profile_retirement::RetirementStart::Draining(_)
            ),
            "{start:?}"
        );
    }
    let app = support::boot_configured_on(state.path(), &document, ports)
        .await
        .expect("standalone boots again");
    assert!(app
        .store
        .profile_retirement(&host, "vllm-patched")
        .unwrap()
        .is_none());
    let _ = id;
    let _ = app.shutdown().await;
}

fn script(path: &std::path::Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}

/// A TensorFold venv as `engine add` finds one (`engine_cli.rs`).
fn tensorfold_env(root: &std::path::Path, version: &str, tools: &[&str]) -> std::path::PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("tensorfold")).unwrap();
    std::fs::create_dir_all(site.join(format!("tensorfold-{version}.dist-info"))).unwrap();
    std::fs::write(
        site.join(format!("tensorfold-{version}.dist-info/METADATA")),
        format!("Name: tensorfold\nVersion: {version}\n"),
    )
    .unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(
        &root.join("bin/tensorfold"),
        &format!("echo tensorfold {version}"),
    );
    let report = serde_json::json!({"schema": "capyctl/engine-capabilities/v1", "engine": "tensorfold",
        "capabilities": {"core": [], "deep_park": ["unsupported"], "metrics": []}});
    script(&root.join("bin/python3"), &format!("echo '{report}'"));
    for tool in tools {
        script(&root.join("bin").join(tool), "exit 0");
    }
    root.to_path_buf()
}

// T41 T07 (ADR 0023 §2, ADR 0018 §5): a TensorFold profile registered in
// engines.yaml is published by the standalone provider as `tensorfold`,
// engine `tensorfold`, deep park disabled.
#[test]
fn standalone_publishes_a_registered_tensorfold_profile() {
    use capyctl_controller::EngineProvider as _;
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    let env = tensorfold_env(&state.path().join("tf"), "0.6.0", &["ninja", "nvcc", "c++"]);
    let path = engines_beside(&document);
    let lock = lock_engines(&path).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert(
        "tensorfold".into(),
        capyctl_config::registration::profile_document(&ProfileSpec {
            engine: capyctl_config::engine_policy::Engine::Tensorfold,
            executable: env.join("bin/tensorfold"),
            build_fingerprint: "0.6.0".into(),
            deep_park: false,
            installation_drift: capyctl_config::effective::InstallationDrift::Warn,
            args: vec![],
            cuda_home: None,
        }),
    );
    write_engines(&engines, &lock, None).unwrap();
    drop(lock);
    let registered = EnginesFile::load(&path).unwrap().profiles;
    let runtime = state.path().join("runtime");
    let provider = capyctl_cli::roles::EnvEngineProvider::with_managed_runtime(runtime);
    let all = provider
        .installations(&registered)
        .expect("the registered TensorFold profile is an installation");
    let published = all
        .iter()
        .find(|named| named.profile == "tensorfold")
        .expect("published as `tensorfold`");
    assert_eq!(
        published.installation.engine,
        capyctl_config::engine_policy::Engine::Tensorfold
    );
    assert!(!published.installation.deep_park);
    assert_eq!(published.installation.build_fingerprint, "0.6.0");
    assert_eq!(
        published.installation.executable,
        env.join("bin/tensorfold")
    );
}
