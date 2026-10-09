//! F1 roles wiring: `start_standalone_with` boots the embedded graph and serves
//! the router; the CLI deploy path submits + activates through the
//! controller; status reads without activating.

mod support;

use capyctl_config::effective::ModelSource;
use capyctl_controller::LifecyclePort as _;
use support::{boot, safe_state_dir, stub_engine};

#[tokio::test]
async fn standalone_boots_and_serves_router() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    // The App carries a servable router (F1: the router listener is the
    // standalone role's inference surface, 127.0.0.1-only).
    let router = app.router();
    let _ = router; // servable; full serve loop covered by run_standalone

    // Deploy through the controller (the CLI deploy path) and activate.
    let id = app
        .deploy(
            "wired-m",
            ModelSource::Local {
                path: "/models/wired-m".into(),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    let state = app.controller.wait_terminal(&op).await.unwrap();
    assert_eq!(state, capyctl_domain::LifecycleState::Ready);

    // Dispatch resolves the forwarder from what the launch recorded, so the engine
    // has to be at that address for the wiring to be exercised at all.
    let engine = stub_engine(&app.controller, &id).await;
    let resp = app
        .deps()
        .forwards
        .forwarder(&id)
        .expect("a ready deployment has a forwarder")
        .forward_chat(&serde_json::json!({"model": "wired-m", "messages": []}))
        .await
        .unwrap();
    assert!(resp["choices"][0]["message"]["content"].is_string());
    engine.abort();
}

#[tokio::test]
async fn router_serves_models_and_chat_over_http() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "http-m",
            ModelSource::Local {
                path: "/models/http-m".into(),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    app.controller.wait_terminal(&op).await.unwrap();
    let engine = stub_engine(&app.controller, &id).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = reqwest::Client::new();
    let key = app.api_key();
    let models = client
        .get(format!("http://{addr}/v1/models"))
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let ids: serde_json::Value = models.json().await.unwrap();
    assert!(ids["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "http-m"));

    let chat = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({"model": "http-m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 200);
    let body: serde_json::Value = chat.json().await.unwrap();
    assert!(body["choices"][0]["message"]["content"].is_string());
    engine.abort();
}

/// An SGLang engine at the launch's recorded endpoint that answers only its
/// load reads, with the launch's key: `/metrics` with 3 running, 1 waiting and
/// a quarter of the KV pool in use, and `/v1/loads?include=core` with a running
/// limit of 24 over two data-parallel ranks (SGLang 0.5.20 shapes).
async fn sglang_load_engine(
    runtime: &capyctl_controller::RuntimeEndpoint,
) -> tokio::task::JoinHandle<()> {
    let url: reqwest::Url = runtime.endpoint.parse().expect("the endpoint is a URL");
    let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let expected = format!(
        "Bearer {}",
        runtime
            .engine_key
            .clone()
            .expect("an embedded launch is given a key")
    );
    let keyed = move |headers: &axum::http::HeaderMap, body: &'static str| {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if presented == Some(expected.as_str()) {
            (axum::http::StatusCode::OK, body)
        } else {
            (axum::http::StatusCode::UNAUTHORIZED, "")
        }
    };
    let metrics = {
        let keyed = keyed.clone();
        axum::routing::get(move |headers: axum::http::HeaderMap| async move {
            keyed(
                &headers,
                "# TYPE sglang:num_running_reqs gauge\n\
                 sglang:num_running_reqs{model_name=\"m\"} 3.0\n\
                 # TYPE sglang:num_queue_reqs gauge\n\
                 sglang:num_queue_reqs{model_name=\"m\"} 1.0\n\
                 # TYPE sglang:token_usage gauge\n\
                 sglang:token_usage{model_name=\"m\"} 0.25\n",
            )
        })
    };
    let loads = axum::routing::get(move |headers: axum::http::HeaderMap| async move {
        keyed(
            &headers,
            r#"{"loads":[{"dp_rank":0,"max_running_requests":12},{"dp_rank":1,"max_running_requests":12}]}"#,
        )
    });
    let app = axum::Router::new()
        .route("/metrics", metrics)
        .route("/v1/loads", loads);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .unwrap_or_else(|error| panic!("the recorded endpoint {address} is bindable: {error}"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })
}

// T18 (SPEC §§10, 17, owner decision 2026-10-08): standalone samples its own
// engine as a host agent does, so the management load read shows a fresh
// sample with the engine's gauges and its reported running limit (`source:
// engine`), and the router scores the instance on that engine load. Fake
// engine; not qualification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_reports_its_engine_load() {
    let dir = safe_state_dir();
    let app = boot(dir.path()).await;
    let id = app
        .deploy(
            "loaded-m",
            ModelSource::Local {
                path: "/models/loaded-m".into(),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    let runtime = app
        .controller
        .runtime_endpoint(&id)
        .unwrap()
        .expect("a ready deployment has a recorded runtime");
    let engine = sglang_load_engine(&runtime).await;

    let credentials = std::fs::read_to_string(dir.path().join("identity/credentials")).unwrap();
    let admin = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .unwrap()
        .to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let read = || async {
        let response = reqwest::Client::new()
            .get(format!(
                "http://{address}/management/v1/metrics/load?deployment={id}"
            ))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.json::<serde_json::Value>().await.unwrap()["deployments"][0]["instances"][0]
            .clone()
    };
    // The embedded host samples once a second; a few periods is ample.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let instance = loop {
        let instance = read().await;
        if !instance["sample"].is_null() || std::time::Instant::now() > deadline {
            break instance;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let sample = &instance["sample"];
    assert_eq!(sample["fresh"], true, "{instance}");
    assert_eq!(
        sample["engine"],
        serde_json::json!({"running": 3, "waiting": 1, "kv_usage_ppm": 250_000}),
        "{instance}"
    );
    assert_eq!(
        instance["max_running"],
        serde_json::json!({"count": 24, "source": "engine"}),
        "{instance}"
    );

    // The router's candidate for the instance carries the same engine load.
    let serving = app
        .controller
        .serving_instances(&id)
        .unwrap()
        .expect("the coordinator reports serving instances");
    let (gauges, _) = capyctl_router::balance::usable_load(&serving[0])
        .expect("the router scores the embedded instance on its engine's load");
    assert_eq!((gauges.running, gauges.waiting), (3, 1));
    server.abort();
    engine.abort();
}
