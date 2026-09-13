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
    for (body, success) in [(r#"{"success":true}"#, true), (r#"{"success":false}"#, false),
                            (r#"{"success":"true"}"#, false), ("{}", false), ("", false), ("not json", false)] {
        let app = axum::Router::new().route("/reset_prefix_cache", post(move |headers: axum::http::HeaderMap| async move {
            assert_eq!(headers.get("authorization").unwrap(), "Bearer test-secret");
            body
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener,app).await.unwrap() });
        let http = EngineHttp::new(format!("http://{addr}").parse().unwrap(), Some("test-secret".into()));
        assert_eq!(http.reset_prefix_cache().await.is_ok(), success, "acknowledgement: {body:?}");
        server.abort();
    }
}

#[tokio::test]
async fn checkpoint_reload_can_exceed_short_control_timeout() {
    let app = axum::Router::new().route("/collective_rpc", post(|| async {
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        StatusCode::OK
    }));
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
    assert!(matches!(http.sleep(2).await.unwrap(), SleepOutcome::Applied));
    assert_eq!(st.sleep_hit.load(Ordering::SeqCst), 1);
    assert!(matches!(http.wake().await.unwrap(), mllm_adapters::vllm::WakeOutcome::Applied));
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
    assert!(matches!(http.health().await, Err(HttpError::Unreachable(_))));
}
