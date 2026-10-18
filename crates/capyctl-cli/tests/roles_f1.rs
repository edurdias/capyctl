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
/// load reads, with the launch's key: `/metrics` with 3 running, 1 waiting, a
/// quarter of the KV pool in use and two requests in its time-to-first-token
/// histogram, and `/v1/loads?include=core` with a running limit of 24 over two
/// data-parallel ranks (SGLang 0.5.20 shapes).
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
                 sglang:token_usage{model_name=\"m\"} 0.25\n\
                 # TYPE sglang:time_to_first_token_seconds histogram\n\
                 sglang:time_to_first_token_seconds_bucket{le=\"0.1\",model_name=\"m\"} 1.0\n\
                 sglang:time_to_first_token_seconds_bucket{le=\"1.0\",model_name=\"m\"} 2.0\n\
                 sglang:time_to_first_token_seconds_bucket{le=\"+Inf\",model_name=\"m\"} 2.0\n\
                 sglang:time_to_first_token_seconds_sum{model_name=\"m\"} 0.55\n\
                 sglang:time_to_first_token_seconds_count{model_name=\"m\"} 2.0\n",
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

/// A standalone role with one Ready deployment named `name`, the SGLang load
/// engine at its recorded endpoint, and its management API served on
/// loopback.
struct SampledStandalone {
    app: capyctl_cli::roles::App,
    id: String,
    address: std::net::SocketAddr,
    admin: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl SampledStandalone {
    async fn start(dir: &std::path::Path, name: &str) -> Self {
        let app = boot(dir).await;
        let id = app
            .deploy(
                name,
                ModelSource::Local {
                    path: format!("/models/{name}"),
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

        let credentials = std::fs::read_to_string(dir.join("identity/credentials")).unwrap();
        let admin = credentials
            .lines()
            .find_map(|line| line.strip_prefix("admin_token: "))
            .unwrap()
            .to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = app.management_router();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            app,
            id,
            address,
            admin,
            tasks: vec![engine, server],
        }
    }

    /// The first instance of the deployment in the management read `view`
    /// (`load` or `latency`), polled until `ready` holds or ten seconds pass.
    /// The embedded host samples once a second; a few periods is ample.
    async fn instance(
        &self,
        view: &str,
        ready: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let response = reqwest::Client::new()
                .get(format!(
                    "http://{}/management/v1/metrics/{view}?deployment={}",
                    self.address, self.id
                ))
                .bearer_auth(&self.admin)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let instance = response.json::<serde_json::Value>().await.unwrap()["deployments"][0]
                ["instances"][0]
                .clone();
            if ready(&instance) || std::time::Instant::now() > deadline {
                return instance;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

impl Drop for SampledStandalone {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

// T18 (SPEC §§10, 17, owner decision 2026-10-08): standalone samples its own
// engine as a host agent does, so the management load read shows a fresh
// sample with the engine's gauges and its reported running limit (`source:
// engine`), and the router scores the instance on that engine load. Fake
// engine; not qualification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_reports_its_engine_load() {
    let dir = safe_state_dir();
    let role = SampledStandalone::start(dir.path(), "loaded-m").await;
    let instance = role
        .instance("load", |instance| !instance["sample"].is_null())
        .await;
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
    let serving = role
        .app
        .controller
        .serving_instances(&role.id)
        .unwrap()
        .expect("the coordinator reports serving instances");
    let (gauges, _) = capyctl_router::balance::usable_load(&serving[0])
        .expect("the router scores the embedded instance on its engine's load");
    assert_eq!((gauges.running, gauges.waiting), (3, 1));
}

// T40 (SPEC §17, M80; found live 2026-10-10): the engine histograms
// standalone's own samples carry reach its latency read, as a host's reach a
// server's: the instance names the engine family and carries the engine's
// time-to-first-token distribution with `tier: engine`, `source: engine`.
// Before, standalone's latency read held router series only. Fake engine;
// not qualification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_reports_its_engine_latency() {
    let dir = safe_state_dir();
    let role = SampledStandalone::start(dir.path(), "timed-m").await;
    let engine_series = |instance: &serde_json::Value| {
        instance["series"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|series| series["source"] == "engine")
            .cloned()
    };
    let instance = role
        .instance("latency", |instance| engine_series(instance).is_some())
        .await;
    let series = engine_series(&instance).unwrap_or_else(|| panic!("no engine series: {instance}"));
    assert_eq!(instance["engine"], "sglang", "{instance}");
    assert_eq!(
        (&series["name"], &series["tier"], &series["count"]),
        (
            &serde_json::json!("engine_time_to_first_token"),
            &serde_json::json!("engine"),
            &serde_json::json!(2)
        ),
        "{instance}"
    );
}

/// llama.cpp v0.7.0's answer to an input over its batch size (found live
/// 2026-10-17 on host A, decision deployments at context 8192).
const LLAMA_TOO_LARGE: &str = r#"{"error":{"code":500,"message":"input (8195 tokens) is too large to process. increase the physical batch size","type":"server_error"}}"#;

/// A standalone role with one Ready deployment named `name`, an engine at its
/// recorded endpoint that answers every chat completion with a 500 and
/// [`LLAMA_TOO_LARGE`] (only its first half, then a failed transfer, when
/// `cut`), and the router served on loopback. Returns the role, the router's
/// address and the count of requests that reached the engine.
async fn erring_standalone(
    state_dir: &std::path::Path,
    name: &str,
    cut: bool,
) -> (
    capyctl_cli::roles::App,
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let app = boot(state_dir).await;
    let id = app
        .deploy(
            name,
            ModelSource::Local {
                path: format!("/models/{name}"),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    app.controller.wait_terminal(&op).await.unwrap();
    let runtime = app.controller.runtime_endpoint(&id).unwrap().unwrap();
    let url: reqwest::Url = runtime.endpoint.parse().unwrap();
    let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let chat = axum::routing::post(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            let body = if cut {
                axum::body::Body::from_stream(tokio_stream::iter([
                    Ok(axum::body::Bytes::from_static(
                        &LLAMA_TOO_LARGE.as_bytes()[..LLAMA_TOO_LARGE.len() / 2],
                    )),
                    Err(std::io::Error::other("the engine went away")),
                ]))
            } else {
                axum::body::Body::from(LLAMA_TOO_LARGE)
            };
            axum::response::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(body)
                .unwrap()
        }
    });
    let engine = tokio::net::TcpListener::bind(&address)
        .await
        .unwrap_or_else(|error| panic!("the recorded endpoint {address} is bindable: {error}"));
    tokio::spawn(async move {
        let _ = axum::serve(
            engine,
            axum::Router::new().route("/v1/chat/completions", chat),
        )
        .await;
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let router = listener.local_addr().unwrap();
    let served = app.router();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    (app, router, hits)
}

/// One chat completion through the router: its status and JSON answer.
async fn chat_once(
    app: &capyctl_cli::roles::App,
    router: std::net::SocketAddr,
    name: &str,
) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{router}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {}", app.api_key()))
        .json(&serde_json::json!({"model": name, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_default())
}

// T17 T19 (SPEC §10, amended 2026-10-17; found live 2026-10-17 on host A):
// an engine answering every request with a 500 and its complete JSON error
// used to leave each lease uncertain for the life of the session, so after
// the outstanding-request bound the deployment refused everything with 429
// `queue_full` until it was deleted. Each such answer is now relayed as a
// 502 `engine_error` with the engine's message and its lease is cancelling,
// closed by the real coordinator once the engine reads quiescent, so three
// times the bound in answers still reach the engine. Fake engine; not
// qualification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_error_answers_never_exhaust_the_deployment() {
    let dir = safe_state_dir();
    let (app, router, hits) = erring_standalone(dir.path(), "erring-m", false).await;
    let bound = capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT as usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut answered = 0;
    while answered < 3 * bound {
        let (status, body) = chat_once(&app, router, "erring-m").await;
        if status == 429 {
            // The bound is reached until the engine's quiescence settles the
            // cancelling leases; it must not stay reached.
            assert_eq!(body["code"], "queue_full", "{body}");
            assert!(
                tokio::time::Instant::now() < deadline,
                "the deployment stayed exhausted after {answered} engine errors"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        assert_eq!(status, 502, "{body}");
        assert_eq!(body["code"], "engine_error", "{body}");
        assert_eq!(body["retryable"], false, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .starts_with("input (8195 tokens) is too large to process"),
            "{body}"
        );
        answered += 1;
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        answered,
        "never replayed"
    );
}

// T17 T19 (SPEC §10, amended 2026-10-17): a 500 whose body is cut short is no
// answer, so its lease stays uncertain: the engine's quiescence does not
// settle it, and past the bound the deployment refuses with 429 `queue_full`
// rather than release a reservation without evidence. Fake engine; not
// qualification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_engine_error_answer_stays_uncertain() {
    let dir = safe_state_dir();
    let (app, router, hits) = erring_standalone(dir.path(), "cut-m", true).await;
    let bound = capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT as usize;
    for _ in 0..bound {
        let (status, body) = chat_once(&app, router, "cut-m").await;
        assert_eq!(status, 500, "{body}");
        assert_eq!(body["code"], "engine_error", "{body}");
        assert_eq!(body["message"], "backend completion unverified", "{body}");
    }
    // Several quiescence questions later, every lease is still charged.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let (status, body) = chat_once(&app, router, "cut-m").await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["code"], "queue_full", "{body}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), bound);
}
