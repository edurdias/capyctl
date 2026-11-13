//! KTD11: the API retains the CLI's refusal for an engine in use.
mod support;
use capyctl_controller::LifecyclePort as _;
use serde_json::{json, Value};

// T16 T32, ADR 0040 §10: no engine file is changed on an in-use refusal.
#[tokio::test]
async fn api_remove_in_use_returns_the_cli_profile_in_use_code() {
    let state = support::safe_state_dir();
    let config = state.path().join("standalone.yaml");
    std::fs::write(&config, "schema_version: 1\nkind: standalone\nname: local\nhost:\n  name: local\n  runtime_profiles: {}\n").unwrap();
    let engines = capyctl_config::registration::engines_beside(&config);
    let lock = capyctl_config::registration::lock_engines(&engines).unwrap();
    let mut file = capyctl_config::registration::EnginesFile::load(&engines).unwrap();
    file.profiles.insert(
        "busy-profile".into(),
        capyctl_config::registration::profile_document(
            &capyctl_config::registration::ProfileSpec {
                engine: capyctl_config::engine_policy::Engine::Vllm,
                executable: "/bin/true".into(),
                build_fingerprint: "0.29.0".into(),
                deep_park: false,
                installation_drift: capyctl_config::effective::InstallationDrift::Warn,
                args: vec![],
                cuda_home: None,
                approved_options: vec![],
                approved_paths: vec![],
                env: Default::default(),
                approved_env: vec![],
                trust_remote_code: false,
                lmcache: None,
            },
        ),
    );
    capyctl_config::registration::write_engines(&file, &lock, None).unwrap();
    drop(lock);
    let app = support::boot_configured(state.path(), &config)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server =
        tokio::spawn(async move { support::serve_management(listener, router).await.unwrap() });
    let credentials = std::fs::read_to_string(state.path().join("identity/credentials")).unwrap();
    let token = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .unwrap();
    let client = reqwest::Client::new();
    let deployment = capyctl_cli::standalone_config::deployment_document(
        "busy-model",
        "busy-model",
        &capyctl_config::effective::ModelSource::Local {
            path: "/models/busy-model".into(),
        },
        capyctl_config::engine_policy::Engine::Vllm,
        &capyctl_cli::standalone_config::TemplateMemory::Unified {
            capacity_bytes: support::TEST_CAPACITY_BYTES,
        },
        capyctl_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        false,
        "busy-profile",
    )
    .unwrap();
    let response = client
        .post(format!("http://{address}/management/v1/deployments"))
        .bearer_auth(token)
        .header("idempotency-key", ulid::Ulid::new().to_string())
        .json(&json!({"config":deployment,"activate":false}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let added: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status} {added}");
    let op = app
        .controller
        .request_transition(
            added["deployment_id"].as_str().unwrap(),
            capyctl_domain::LifecycleAction::Start,
        )
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    let before = std::fs::read(&engines).unwrap();
    let response = client
        .post(format!(
            "http://{address}/management/v1/hosts/standalone/engines/busy-profile/remove"
        ))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let refused: Value = response.json().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"]["code"], "profile_in_use", "{refused}");
    assert_eq!(std::fs::read(&engines).unwrap(), before);
    assert!(app.profiles().contains(&"busy-profile".into()));
    server.abort();
    let _ = app.shutdown().await;
}
