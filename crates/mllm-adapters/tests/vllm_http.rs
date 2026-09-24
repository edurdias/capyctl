//! Contract tests for the vLLM HTTP engine client, run against a local axum
//! mock implementing vLLM's OpenAI-compatible surfaces.

use std::net::SocketAddr;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use axum::Json;
use futures::stream::Stream;
use mllm_adapters::vllm::{EngineHttp, HttpError, SleepOutcome, StreamEnd};
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
struct MockState {
    models_hit: Arc<AtomicUsize>,
    sleep_hit: Arc<AtomicUsize>,
    wake_hit: Arc<AtomicUsize>,
    drop_sleep: Arc<AtomicBool>,
}

async fn health() -> &'static str {
    "OK"
}

async fn models(State(st): State<MockState>) -> Json<serde_json::Value> {
    st.models_hit.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({"object": "list", "data": [
        {"id": "toy-model", "object": "model"}
    ]}))
}

async fn do_sleep(
    State(st): State<MockState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    st.sleep_hit.fetch_add(1, Ordering::SeqCst);
    if st.drop_sleep.load(Ordering::SeqCst) {
        // Simulate the effect applied but the connection dying before the
        // response: the client must report Uncertain, never Applied.
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let level = q.get("level").cloned().unwrap_or_default();
    Ok(Json(serde_json::json!({"sleep": true, "level": level})))
}

async fn do_wake(State(st): State<MockState>) -> Json<serde_json::Value> {
    st.wake_hit.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({"awake": true}))
}

async fn reload(Json(body): Json<serde_json::Value>) -> StatusCode {
    assert_eq!(body["method"], "reload_weights");
    StatusCode::OK
}

async fn sse() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let body = futures::stream::iter(vec![
        Ok(Event::default().data(r#"{"delta":"hel"}"#)),
        Ok(Event::default().data(r#"{"delta":"lo"}"#)),
        Ok(Event::default().data("[DONE]")),
    ]);
    Sse::new(body)
}

async fn spawn_mock() -> (SocketAddr, MockState) {
    let st = MockState::default();
    let app = axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/sleep", post(do_sleep))
        .route("/wake_up", post(do_wake))
        .route("/collective_rpc", post(reload))
        .route("/v1/chat/completions", post(sse))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, st)
}

#[tokio::test]
async fn health_and_models_read() {
    let (addr, _st) = spawn_mock().await;
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    assert!(http.health().await.unwrap());
    let ids = http.list_models().await.unwrap();
    assert_eq!(ids, vec!["toy-model".to_string()]);
}

#[tokio::test]
async fn collective_rpc_requests_weight_reload() {
    let (addr, _) = spawn_mock().await;
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    http.collective_rpc().await.unwrap();
}

#[tokio::test]
async fn prefix_reset_requires_explicit_boolean_success_and_authenticates() {
    for (body, success) in [
        (r#"{"success":true}"#, true),
        (r#"{"success":false}"#, false),
        (r#"{"success":"true"}"#, false),
        ("{}", false),
        ("", false),
        ("not json", false),
    ] {
        let app = axum::Router::new().route(
            "/reset_prefix_cache",
            post(move |headers: axum::http::HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer test-secret");
                body
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = EngineHttp::new(
            format!("http://{addr}").parse().unwrap(),
            Some("test-secret".into()),
        );
        assert_eq!(
            http.reset_prefix_cache().await.is_ok(),
            success,
            "acknowledgement: {body:?}"
        );
        server.abort();
    }
}

#[tokio::test]
async fn checkpoint_reload_can_exceed_short_control_timeout() {
    let app = axum::Router::new().route(
        "/collective_rpc",
        post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(31)).await;
            StatusCode::OK
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    let result = http.collective_rpc().await;
    server.abort();
    result.expect("checkpoint reload must outlive the 30-second control timeout");
}

#[tokio::test]
async fn sleep_applies_and_wake_applies() {
    let (addr, st) = spawn_mock().await;
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    assert!(matches!(
        http.sleep(2).await.unwrap(),
        SleepOutcome::Applied
    ));
    assert_eq!(st.sleep_hit.load(Ordering::SeqCst), 1);
    assert!(matches!(
        http.wake().await.unwrap(),
        mllm_adapters::vllm::WakeOutcome::Applied
    ));
    assert_eq!(st.wake_hit.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sleep_with_dropped_response_is_uncertain() {
    // Raw TCP server: accepts the connection, reads the request, then drops
    // the connection without responding — the transport-level picture of
    // "effect dispatched, ack lost". The client must report Uncertain.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
        // Drop without responding (sock dropped at scope end).
    });
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    assert!(matches!(http.sleep(2).await, Err(HttpError::Uncertain(_))));
}

#[tokio::test]
async fn sse_chunks_arrive_in_order_until_done() {
    let (addr, _st) = spawn_mock().await;
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    let mut seen = Vec::new();
    let end = http
        .chat_completion_stream(&serde_json::json!({"model": "toy-model"}), |chunk| {
            seen.push(chunk.text.clone());
        })
        .await
        .unwrap();
    assert!(matches!(end, StreamEnd::Completed));
    assert_eq!(seen, vec![r#"{"delta":"hel"}"#, r#"{"delta":"lo"}"#]);
}

#[tokio::test]
async fn unreachable_engine_errors() {
    // Bind a port, then drop the listener: nothing is listening.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    assert!(matches!(
        http.health().await,
        Err(HttpError::Unreachable(_))
    ));
}

// T21 T37: SPEC §9.1 (engine controls never leave loopback). A redirect from
// the engine port must not move a keyed control or read somewhere else: the
// client never follows it, so the key is never presented to the target.
#[tokio::test]
async fn engine_client_never_follows_a_redirect() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let hits = target_hits.clone();
    let target = axum::Router::new().route(
        "/elsewhere",
        get(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({"data": [{"id": "toy-model"}], "is_sleeping": false}))
            }
        })
        .post(|| async { StatusCode::OK }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, target).await.unwrap() });
    let location = format!("http://{target_addr}/elsewhere");
    let redirecting = axum::Router::new().fallback(move || {
        let location = location.clone();
        async move { (StatusCode::TEMPORARY_REDIRECT, [("location", location)]) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, redirecting).await.unwrap() });
    let http = EngineHttp::new(
        format!("http://{addr}").parse().unwrap(),
        Some("engine-key".into()),
    );
    assert!(http.list_models().await.is_err());
    assert!(http.is_sleeping().await.is_err());
    assert!(http.sleep(1).await.is_err());
    assert!(http.wake().await.is_err());
    assert!(http.collective_rpc().await.is_err());
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
}

// T21 T22: a read from the engine is bounded (4 MiB), whether or not the body
// declares its length, so a misbehaving engine cannot exhaust agent memory.
#[tokio::test]
async fn engine_reads_are_bounded() {
    use axum::body::Body;
    let app = axum::Router::new()
        .route(
            "/v1/models",
            get(|| async {
                // Chunked: no declared length. Valid JSON after the padding,
                // so only the bound refuses it.
                let chunk = axum::body::Bytes::from(vec![b' '; 1 << 20]);
                let tail = axum::body::Bytes::from_static(br#"{"data":[{"id":"toy-model"}]}"#);
                let stream = futures::stream::iter(
                    (0..6)
                        .map(move |_| Ok::<_, Infallible>(chunk.clone()))
                        .chain(std::iter::once(Ok(tail))),
                );
                Body::from_stream(stream)
            }),
        )
        .route(
            "/metrics",
            get(|| async { vec![b'#'; 5 << 20] }),
        )
        .route(
            "/v1/chat/completions",
            post(|| async {
                // An SSE frame that never ends.
                let chunk = axum::body::Bytes::from(vec![b'x'; 1 << 20]);
                let stream = futures::stream::iter(
                    (0..6).map(move |_| Ok::<_, Infallible>(chunk.clone())),
                );
                Body::from_stream(stream)
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), None);
    assert!(matches!(http.list_models().await, Err(HttpError::Body(_))));
    assert!(matches!(http.work_counts().await, Err(HttpError::Body(_))));
    let result = http
        .chat_completion_stream(&serde_json::json!({}), |_| {})
        .await;
    assert!(matches!(result, Err(HttpError::Body(_))));
}

/// A mock of mllm's key guard (`runtime/mllm_vllm_guard.py`): inference paths
/// take `inference`, every other path takes `control`. Each request records its
/// path and whether it was admitted.
async fn spawn_keyed_guard(
    inference: &'static str,
    control: &'static str,
) -> (SocketAddr, Arc<std::sync::Mutex<Vec<(String, bool)>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    let guard = axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let record = record.clone();
            async move {
                let path = request.uri().path().to_owned();
                let presented = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let inference_path = path.starts_with("/v1/") || path == "/metrics";
                let expected = if inference_path { inference } else { control };
                let admitted = presented.as_deref() == Some(&format!("Bearer {expected}"));
                record.lock().unwrap().push((path, admitted));
                if admitted {
                    next.run(request).await
                } else {
                    axum::response::IntoResponse::into_response(StatusCode::UNAUTHORIZED)
                }
            }
        },
    );
    let app = axum::Router::new()
        .route("/v1/models", get(|| async {
            Json(serde_json::json!({"object": "list", "data": [{"id": "toy-model"}]}))
        }))
        .route("/v1/chat/completions", post(sse))
        .route("/sleep", post(|| async { Json(serde_json::json!({})) }))
        .route("/wake_up", post(|| async { Json(serde_json::json!({})) }))
        .route("/is_sleeping", get(|| async { Json(serde_json::json!({"is_sleeping": false})) }))
        .route("/collective_rpc", post(reload))
        .route(
            "/reset_prefix_cache",
            post(|| async { Json(serde_json::json!({"success": true})) }),
        )
        .layer(guard);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

async fn drive_every_route(http: &EngineHttp) {
    assert_eq!(http.list_models().await.unwrap(), vec!["toy-model".to_string()]);
    http.chat_completion_stream(&serde_json::json!({"model": "toy-model"}), |_| {})
        .await
        .unwrap();
    http.sleep(1).await.unwrap();
    http.wake().await.unwrap();
    http.wake_tag(mllm_adapters::vllm::WakeTag::Weights).await.unwrap();
    assert!(!http.is_sleeping().await.unwrap());
    http.collective_rpc().await.unwrap();
    http.reset_prefix_cache().await.unwrap();
}

/// SPEC §9.1 / T21, ADR 0012: with an admin key the client presents it on the
/// development and control routes (`/sleep`, `/wake_up`, `/is_sleeping`,
/// `/collective_rpc`, `/reset_prefix_cache`) and the inference key only on
/// `/v1`. A guard that keys the two apart admits every call; the inference key
/// is refused on the control routes.
// T21 T37
#[tokio::test]
async fn control_routes_take_the_admin_key_and_inference_takes_the_inference_key() {
    let (addr, seen) = spawn_keyed_guard("inference-key", "admin-key").await;
    let base: reqwest::Url = format!("http://{addr}").parse().unwrap();
    let keyed = EngineHttp::new(base.clone(), Some("inference-key".into()))
        .with_admin_key("admin-key".into());
    drive_every_route(&keyed).await;
    let recorded = seen.lock().unwrap().clone();
    assert!(recorded.iter().all(|(_, admitted)| *admitted), "{recorded:?}");
    for path in ["/sleep", "/wake_up", "/is_sleeping", "/collective_rpc", "/reset_prefix_cache"] {
        assert!(recorded.iter().any(|(seen, _)| seen == path), "{path} not driven");
    }

    // The inference key alone opens nothing on the control surface.
    seen.lock().unwrap().clear();
    let inference_only = EngineHttp::new(base, Some("inference-key".into()));
    assert!(inference_only.sleep(1).await.is_err());
    assert!(inference_only.is_sleeping().await.is_err());
    assert!(inference_only.collective_rpc().await.is_err());
    assert!(inference_only.reset_prefix_cache().await.is_err());
    assert!(seen.lock().unwrap().iter().all(|(_, admitted)| !admitted));
}

/// ADR 0012 migration: an engine launched before the admin role keeps the
/// single-key guard until it restarts, so a client without an admin key still
/// presents the one engine key on every route.
// T21
#[tokio::test]
async fn without_an_admin_key_every_route_takes_the_engine_key() {
    let (addr, seen) = spawn_keyed_guard("engine-key", "engine-key").await;
    let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), Some("engine-key".into()));
    drive_every_route(&http).await;
    assert!(seen.lock().unwrap().iter().all(|(_, admitted)| *admitted));
}
