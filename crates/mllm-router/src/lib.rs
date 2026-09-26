//! Router crate: public inference surface (F1 design §5). The router owns
//! admission, queue bounds, and dispatch; it never chooses models or runs a
//! second scheduler (SPEC §10), and never imports engine crates — dispatch
//! reaches engines through adapters behind `ChatForward`.

pub mod admission;
pub mod balance;
pub mod chat;
pub mod forwarders;
// SPEC §10 step 1 (W10): bounded waiting for a deployment to become servable.
pub mod queue;
pub mod stream;
pub mod switch;
// SPEC §17 (M80): per-request timings and their bounded distributions.
pub mod timing;

pub use switch::WakeJoin;

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;

/// Queue bounds (F1 design §5 / T19): bounded queues, explicit deadlines.
#[derive(Debug, Clone)]
pub struct QueueLimits {
    pub max_requests_per_deployment: usize,
    pub max_buffered_bytes_total: usize,
}

#[derive(Clone)]
pub struct RouterDeps {
    /// The lifecycle authority, named by port rather than by implementation, so
    /// which authority runs is a wiring decision rather than a compile-time one.
    pub controller: Arc<dyn mllm_controller::LifecyclePort>,
    /// Where a request's forwarder comes from. Resolved per request rather than
    /// held as a table: a leased port and a per-launch key belong to one launch
    /// (SPEC §3), so a forwarder built at boot addresses a runtime that may no
    /// longer exist.
    pub forwards: Arc<dyn forwarders::ForwarderSource>,
    pub limits: QueueLimits,
    /// Shared inference API key (F1: single-owner lab; per-client keys later).
    /// `None` only when the operator turned authentication off (design §9).
    pub api_key: Option<String>,
    /// Conservative in-flight accounting (released only on confirmed end).
    pub inflight: Arc<admission::InFlight>,
    /// Activation join (T15 at the router tier): concurrent requests waking
    /// the same non-READY deployment join ONE wake — no double-spawn, no
    /// duplicate Start operations.
    pub activation_join: Arc<WakeJoin<(StatusCode, Json<serde_json::Value>)>>,
}

#[derive(Clone)]
struct AppState {
    deps: RouterDeps,
}

pub fn serve_router(deps: RouterDeps) -> axum::Router {
    let state = AppState { deps };
    axum::Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .layer(axum::middleware::map_response(retry_after))
        .with_state(state)
}

/// SPEC §13.3, T37: the inference key is compared in constant time, and the
/// `Bearer` scheme is matched case-insensitively (RFC 9110 §11.1).
fn authorized(headers: &HeaderMap, deps: &RouterDeps) -> bool {
    use subtle::ConstantTimeEq;
    match &deps.api_key {
        // Design §9: no key only when the operator chose `authentication:
        // none` (document, `--no-inference-auth` or MLLM_INFERENCE_AUTH); the
        // role warns at start when that listener is not on loopback.
        None => true,
        Some(expected) => headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, key)| bool::from(key.trim_start().as_bytes().ct_eq(expected.as_bytes())))
            .unwrap_or(false),
    }
}

fn err_json(code: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        match code {
            "unauthorized" => StatusCode::UNAUTHORIZED,
            "unknown_model" => StatusCode::NOT_FOUND,
            // SPEC §10 (T19): bounded queues answer "too many requests, retry";
            // only an oversized body is 413, since retrying it cannot succeed.
            "queue_full" => StatusCode::TOO_MANY_REQUESTS,
            "request_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            // SPEC §14: a malformed request is the client's error, not a 500.
            "invalid_request" => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        Json(if code == "queue_full" {
            serde_json::json!({ "code": code, "message": message, "retryable": true })
        } else {
            serde_json::json!({ "code": code, "message": message })
        }),
    )
}

/// SPEC §10 (T19): every 429 the router answers (a full queue, admission
/// blocked for resources) tells the client when to retry.
async fn retry_after(mut response: axum::response::Response) -> axum::response::Response {
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        response
            .headers_mut()
            .entry(axum::http::header::RETRY_AFTER)
            .or_insert(axum::http::HeaderValue::from_static(RETRY_AFTER_SECONDS));
    }
    response
}

/// How long a refused client should wait before retrying (seconds).
const RETRY_AFTER_SECONDS: &str = "1";

/// `GET /v1/models` — lists enabled public ids; NEVER wakes a deployment.
async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !authorized(&headers, &state.deps) {
        return Err(err_json("unauthorized", "missing or invalid api key"));
    }
    let ids = state
        .deps
        .controller
        .list_enabled_route_ids()
        .map_err(|e| err_json("internal", &format!("store: {e}")))?;
    let data: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| serde_json::json!({"id": id, "object": "model"}))
        .collect();
    Ok(Json(serde_json::json!({ "object": "list", "data": data })))
}

/// `POST /v1/chat/completions` — authenticate → resolve → admit → dispatch.
#[axum::debug_handler]
async fn chat_completions(
    state: axum::extract::State<AppState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    // SPEC §10: a stream's first event is due by the request deadline,
    // counted from arrival, so activation is included.
    let received = tokio::time::Instant::now();
    // SPEC §17 (M80): the router's clock starts when the handler does.
    let mut timing = timing::RequestTiming::start(state.deps.inflight.latency.clone());
    if !authorized(&headers, &state.deps) {
        return Err(err_json(
            "unauthorized",
            "missing or valid api key required",
        ));
    }
    // SPEC §10 (T19): the body is read under the router's configured bound
    // and no other. axum's implicit 2 MiB `Bytes` limit would refuse a
    // multimodal body the configured bound admits (with a plain-text 413) and
    // ignore a smaller one. An oversized declared length is refused before
    // any of it is read; the request is buffered once, then parsed.
    let body = axum::body::to_bytes(body, state.deps.limits.max_buffered_bytes_total)
        .await
        .map_err(|_| err_json("request_too_large", "request exceeds buffered-bytes bound"))?;
    let v: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| err_json("invalid_request", &format!("bad json: {e}")))?;
    let Some(model) = v["model"].as_str().map(str::to_owned) else {
        return Err(err_json("invalid_request", "model is required"));
    };
    // SPEC §10, T19 T21: a request that can never be forwarded is refused
    // before any accounting, activation or engine is touched.
    chat::validate_request(&v)?;
    let body_bytes = body.len();
    // The raw bytes are no longer needed; only the parsed request is held.
    drop(body);
    if v["stream"].as_bool() == Some(true) {
        // Streaming path: resolve + activate, then bridge the engine's SSE.
        // SPEC §10 step 1 and T19: a request that must wait holds its body's
        // bytes, and one that finds the in-flight bound reached waits for a
        // slot in arrival order within its deadline. Accounting is registered
        // before the response is built; the stream guard releases when the
        // backend stream ends.
        let (deployment_id, guard) =
            chat::admit_timed(&state.deps, &model, body_bytes, received, &mut timing).await?;
        // ADR 0013 §10 (I3): choose an instance; SPEC §10: its durable lease is
        // held from before the first byte reaches the engine until the backend
        // stream ends. Refusals before any offer are ordinary HTTP errors.
        let started = std::time::Instant::now();
        let mut plan = balance::Plan::new(&state.deps, &deployment_id)?;
        timing.selected(started.elapsed());
        let started = std::time::Instant::now();
        let first = plan.next().await?;
        timing.leased(started.elapsed());
        // SPEC §17: the phases known before the first byte; the rest follow
        // in the stream's timing comment.
        let header = timing.header_enabled().then(|| timing.header_value());
        let bounds =
            stream::StreamBounds::for_request(received, &state.deps.inflight.waiting.limits());
        let sse = stream::stream_planned_timed(first, Some(plan), v, guard, timing, bounds);
        let mut response = sse.into_response();
        if let Some(value) = header.and_then(|h| h.parse().ok()) {
            response.headers_mut().insert(timing::TIMING_HEADER, value);
        }
        return Ok(response);
    }
    let (json, timing) =
        chat::dispatch_timed(&state.deps, &model, v, body_bytes, received, timing).await?;
    let mut response = Json(json).into_response();
    if timing.header_enabled() {
        if let Ok(value) = timing.header_value().parse() {
            response.headers_mut().insert(timing::TIMING_HEADER, value);
        }
    }
    Ok(response)
}
