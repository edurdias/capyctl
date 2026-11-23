//! KTD11: prune recomputes deployment references for each request.
mod support;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn copy(store: &std::path::Path, key: &str) {
    std::fs::create_dir_all(store.join(key)).unwrap();
    std::fs::write(store.join(key).join("weights.bin"), b"123").unwrap();
    let state = store.join("sources/.capyctl");
    std::fs::create_dir_all(&state).unwrap();
    let digest = Sha256::digest(key.as_bytes());
    let id = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    std::fs::write(
        state.join(format!("{id}.verified")),
        json!({"key":key,"state":"verified","bytes":3,"files":1}).to_string(),
    )
    .unwrap();
}

// SPEC §6.3, ADR 0040 §10: a reference added after listing protects its copy.
#[tokio::test]
async fn prune_uses_current_references_instead_of_the_last_listing() {
    let state = support::safe_state_dir();
    let models = state.path().join("models");
    std::fs::create_dir_all(&models).unwrap();
    let config = state.path().join("standalone.yaml");
    std::fs::write(&config, "schema_version: 1\nkind: standalone\nname: local\nhost:\n  name: local\n  runtime_profiles: {}\n").unwrap();
    let app = support::boot_with_models(
        state.path(),
        Some(&config),
        Some(models.clone()),
        &Default::default(),
        None,
        || None,
    )
    .await
    .unwrap();
    let source = capyctl_config::effective::ModelSource::Http {
        url: Some("https://example.invalid/model.tar".into()),
        url_ref: None,
        sha256: "a".repeat(64),
        archive: capyctl_config::model_source::Archive::None,
    };
    let kept = source.store_key().unwrap();
    let removed = "sources/http/unreferenced";
    copy(&models, &kept);
    copy(&models, removed);
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
    let url = format!("http://{address}/management/v1/hosts/standalone/model-store");
    let listed: Value = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["copies"].as_array().unwrap().len(), 2, "{listed}");
    assert!(listed["copies"]
        .as_array()
        .unwrap()
        .iter()
        .all(|copy| copy["referenced"] == false));
    let deployment = capyctl_cli::standalone_config::deployment_document(
        "retained-source",
        "retained-source",
        &source,
        capyctl_config::engine_policy::Engine::Vllm,
        &capyctl_cli::standalone_config::TemplateMemory::Unified {
            capacity_bytes: support::TEST_CAPACITY_BYTES,
        },
        capyctl_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        false,
        "local",
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
    let listed: Value = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listed["copies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|copy| copy["key"] == kept && copy["referenced"] == true),
        "{listed}"
    );
    let response = client
        .post(format!("{url}/prune"))
        .bearer_auth(token)
        .json(&json!({"apply":true}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let pruned: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status} {pruned}");
    assert_eq!(
        pruned["removed"],
        json!([{"key":removed,"bytes":3}]),
        "{pruned}"
    );
    assert!(models.join(&kept).is_dir());
    assert!(!models.join(removed).exists());
    server.abort();
    let _ = app.shutdown().await;
}
