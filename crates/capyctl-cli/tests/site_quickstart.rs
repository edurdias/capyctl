//! Website spec, Docs: the quickstart's deployment, `docs/examples/
//! deployment-standalone.yaml`, is accepted and placed by a fresh standalone
//! exactly as a reader writes it. Fake-engine test on the product's own
//! resolution and placement; it is not qualification of a native recipe
//! (SPEC §18).
mod support;

use capyctl_controller::LifecyclePort as _;

const EXAMPLE: &str = include_str!("../../../docs/examples/deployment-standalone.yaml");

// T03
#[tokio::test]
async fn the_quickstart_deployment_places_on_a_fresh_standalone() {
    let state = support::safe_state_dir();
    // ADR 0012: a fresh standalone parks deeply unless the host opts out.
    let app = support::boot_deep_parking(state.path(), vec![]).await;
    // The reader's checkpoint: a directory under the models root standalone
    // publishes (CAPYCTL_MODELS_ROOT on a real machine).
    let store = app.host_document()["model_store"]["path"]
        .as_str()
        .expect("standalone publishes its model store")
        .to_owned();
    let checkpoint = std::path::Path::new(&store).join("Qwen3-4B");
    std::fs::create_dir_all(&checkpoint).unwrap();
    std::fs::write(checkpoint.join("config.json"), "{}").unwrap();
    std::fs::write(checkpoint.join("model.safetensors"), [0u8; 64]).unwrap();
    let config = capyctl_config::parse_strict(capyctl_config::ConfigKind::Deployment, EXAMPLE)
        .expect("the example parses");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let credentials = std::fs::read_to_string(state.path().join("identity/credentials")).unwrap();
    let admin = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .unwrap()
        .to_owned();
    let response = reqwest::Client::new()
        .post(format!("http://{address}/management/v1/deployments"))
        .bearer_auth(admin)
        .header("idempotency-key", ulid::Ulid::new().to_string())
        .json(&serde_json::json!({"config": config, "activate": false}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    server.abort();
    assert!(status.is_success(), "{status} {body}");

    let id = body["deployment_id"]
        .as_str()
        .or_else(|| body["id"].as_str())
        .unwrap_or_else(|| panic!("no deployment id in {body}"))
        .to_owned();
    // Placement happens at start: the start must reach the runtime, not be
    // refused for a host or engine that does not exist.
    // Placement happens at start. A real host measures the checkpoint's files
    // before the first launch; the Fake installation has no checkpoint
    // verifier, so here a start may stop at exactly that step. Anything else,
    // such as a host or an engine that does not exist, fails the test.
    match app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
    {
        Ok(_) => {}
        Err(error) => {
            let text = format!("{error:?}");
            assert!(
                text.contains("checkpoint digest pending"),
                "the quickstart deployment was refused before launch: {text}"
            );
        }
    }
}
