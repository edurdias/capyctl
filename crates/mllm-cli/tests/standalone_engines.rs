//! ADR 0018 §5: standalone runs engine registration in one process: the
//! same engines.yaml write, the same socket, the same retirement. Fake-engine
//! tests; not qualification.
mod support;
use mllm_agent::control_socket::{request, ControlRequest, SOCKET_NAME};
use mllm_config::registration::{
    engines_beside, lock_engines, write_engines, EnginesFile, ProfileSpec,
};
use mllm_controller::LifecyclePort as _;
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
        mllm_config::registration::profile_document(&ProfileSpec {
            engine: mllm_config::engine_policy::Engine::Vllm,
            executable: "/bin/true".into(),
            build_fingerprint: "0.29.0".into(),
            deep_park: false,
            installation_drift: mllm_config::effective::InstallationDrift::Warn,
            args: vec![],
        }),
    );
    write_engines(&engines, &lock, None).unwrap();
}

/// Deploy `name` on `profile` through the management API, as `mllm deploy`
/// does: the embedded configuration source composes it against the host
/// document the role publishes now.
async fn deploy(
    app: &mllm_cli::roles::App,
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
    let config = mllm_cli::standalone_config::deployment_document(
        name,
        name,
        &mllm_config::effective::ModelSource::Local {
            path: format!("/models/{name}"),
        },
        mllm_config::engine_policy::Engine::Vllm,
        support::TEST_CAPACITY_BYTES,
        mllm_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        false,
        profile,
    );
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

// T16 T32 (ADR 0018 §4, §5): an unused registered profile is removed and
// unpublished; an environment profile cannot be removed; the name can be
// registered again afterwards.
#[tokio::test]
async fn standalone_remove_rewrites_and_unpublishes() {
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
    assert_eq!(reply["removed"], "vllm-patched", "{reply}");
    assert!(!EnginesFile::load(&engines_beside(&document))
        .unwrap()
        .profiles
        .contains_key("vllm-patched"));
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
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        mllm_domain::LifecycleState::Ready,
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
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        mllm_domain::LifecycleState::Ready,
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
    let structured: mllm_cli::output::StructuredError = refused.into();
    assert_eq!(structured.code, "profile_exists", "{}", structured.message);
}
