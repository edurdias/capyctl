//! The A1 gate: deploy, start and serve one inference through the router, with the
//! coordinator as the sole lifecycle authority.
//!
//! This is the first end-to-end evidence the project has, and it is deliberately
//! one test rather than several: the gate is the whole path, and a suite that
//! proves each hop in isolation is exactly what let the project believe it worked
//! while nothing ran. Every assertion here is on the public surface a user reaches
//! — the CLI's own deploy, the durable state it records, and the HTTP the router
//! serves — never on an internal seam.
//!
//! This runs on the embedded Fake engine, because that is what standalone declares
//! when no live profile is configured. Passing it is not qualification of a native
//! recipe and must never be reported as one (SPEC §18).

use mllm_controller::LifecyclePort as _;

/// A state directory the controller lock will accept. The lock refuses any
/// group- or other-writable ancestor, which rules out `/tmp` and a checkout.
fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

#[tokio::test]
async fn standalone_deploys_starts_and_serves_one_inference() {
    let dir = safe_state_dir();
    let app = mllm_cli::roles::start_standalone(dir.path())
        .await
        .expect("standalone boots");

    // Deploy through the path the CLI uses: a managed configuration, which is what
    // gives the coordinator an effective revision to admit and start against.
    let id = app
        .deploy("gate-m", "/models/gate-m")
        .expect("standalone creates its own deployment");

    let started = app
        .controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .expect("the coordinator accepts the start");
    // Bounded, because the failure this guards against is a start that is accepted
    // and then never settles. Waiting on that forever reports a regression as a
    // hung suite rather than as a failure.
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&started),
    )
    .await
    .expect("the start settles rather than hanging")
    .expect("the start reaches a terminal state");
    assert_eq!(
        state,
        mllm_domain::LifecycleState::Ready,
        "the runtime must actually come up, not merely be admitted"
    );

    // The durable record, not the in-memory one. A lifecycle that only advanced in
    // process state would not survive the restart the store exists to survive.
    let row = app
        .controller
        .get_deployment(&id)
        .expect("the deployment is readable")
        .expect("the deployment exists");
    assert_eq!(row.observed_state, mllm_domain::LifecycleState::Ready);
    let operation = app
        .controller
        .latest_operation(&id)
        .expect("operations are readable")
        .expect("the start recorded an operation");
    assert_eq!(operation.state, mllm_store::OpState::Succeeded);

    // The route the deployment serves is offered for dispatch. A managed
    // configuration records its routes separately from the legacy column, so this
    // also proves the router reads the form the CLI actually writes.
    assert!(
        app.controller
            .list_enabled_route_ids()
            .expect("routes are readable")
            .contains(&"gate-m".to_string()),
        "a ready deployment offers its route"
    );

    // Serve over the real listener, as a user reaches it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router();
    let served = tokio::spawn(async move { axum::serve(listener, router).await });

    let client = reqwest::Client::new();
    let key = app.api_key();

    // The inference surface is authenticated. An unauthenticated caller reaching
    // the engine would make every gate below meaningless.
    let anonymous = client
        .get(format!("http://{addr}/v1/models"))
        .send()
        .await
        .expect("the listener answers");
    assert_ne!(
        anonymous.status(),
        200,
        "the router must not serve an unauthenticated caller"
    );

    let models = client
        .get(format!("http://{addr}/v1/models"))
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("the listener answers");
    assert_eq!(models.status(), 200);
    let listed: serde_json::Value = models.json().await.unwrap();
    assert!(
        listed["data"]
            .as_array()
            .expect("a model list")
            .iter()
            .any(|model| model["id"] == "gate-m"),
        "the deployed route is listed: {listed}"
    );

    let chat = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({
            "model": "gate-m",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("the listener answers");
    assert_eq!(chat.status(), 200, "one inference is served");
    let completion: serde_json::Value = chat.json().await.unwrap();
    assert!(
        completion["choices"][0]["message"]["content"]
            .as_str()
            .is_some_and(|content| !content.is_empty()),
        "the completion carries content from the engine: {completion}"
    );

    served.abort();
}
