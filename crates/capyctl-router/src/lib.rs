//! Router crate: public inference surface (F1 design §5). The router owns
//! admission, queue bounds, and dispatch; it never chooses models or runs a
//! second scheduler (SPEC §10), and never imports engine crates — dispatch
//! reaches engines through adapters behind `ChatForward`. It answers chat
//! completions and, for decisions deployments, decision requests (ADR 0030).

pub mod admission;
pub mod balance;
// SPEC §§10, 17 (owner decision 2026-10-08): the management load read.
pub mod capacity;
pub mod chat;
// ADR 0030 §2: the inference endpoints and the API each answers.
pub mod endpoint;
pub mod forwarders;
// SPEC §10 step 1 (W10): bounded waiting for a deployment to become servable.
pub mod queue;
// SPEC §10 (owner decision 2026-10-19): the caller's and CapyCTL's request IDs.
pub mod request_id;
pub mod stream;
pub mod switch;
// SPEC §17 (owner decision 2026-10-09): per-request engine metrics.
pub mod engine_metrics;
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
    pub controller: Arc<dyn capyctl_controller::LifecyclePort>,
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
        // ADR 0030 §2: decision requests, for `api: decisions` deployments.
        .route("/v1/systemone", post(systemone))
        .layer(axum::middleware::map_response(retry_after))
        .with_state(state)
}

/// SPEC §13.3, T37: the inference key is compared in constant time, and the
/// `Bearer` scheme is matched case-insensitively (RFC 9110 §11.1).
fn authorized(headers: &HeaderMap, deps: &RouterDeps) -> bool {
    use subtle::ConstantTimeEq;
    match &deps.api_key {
        // Design §9: no key only when the operator chose `authentication:
        // none` (document, `--no-inference-auth` or CAPYCTL_INFERENCE_AUTH); the
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
/// ADR 0030 §5: each entry names the API its deployment answers, so a client
/// can tell which endpoint serves it.
async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !authorized(&headers, &state.deps) {
        return Err(err_json("unauthorized", "missing or invalid api key"));
    }
    let routes = state
        .deps
        .controller
        .list_enabled_routes()
        .map_err(|e| err_json("internal", &format!("store: {e}")))?;
    let data: Vec<serde_json::Value> = routes
        .into_iter()
        .map(|route| {
            serde_json::json!({
                "id": route.id,
                "object": "model",
                "capyctl": {"api": route.api.name()},
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "object": "list", "data": data })))
}

/// One inference request read and checked before anything else is touched.
struct Received {
    /// SPEC §10: when the request arrived; its deadline counts from here.
    at: tokio::time::Instant,
    timing: timing::RequestTiming,
    request: capyctl_adapters::request::InferenceRequest,
    model: String,
    /// The size of the body as received.
    body_bytes: usize,
}

/// Authenticate, check the caller's request ID, read the body under its
/// bound, parse it and validate it against `endpoint`'s contract, before any
/// accounting, activation or engine is touched (SPEC §10, T19 T21; ADR 0030
/// §2).
async fn receive(
    deps: &RouterDeps,
    headers: &HeaderMap,
    ids: &request_id::RequestIds,
    body: axum::body::Body,
    endpoint: endpoint::Endpoint,
) -> Result<Received, (StatusCode, Json<serde_json::Value>)> {
    // SPEC §10: a stream's first event is due by the request deadline,
    // counted from arrival, so activation is included.
    let at = tokio::time::Instant::now();
    // SPEC §17 (M80): the router's clock starts when the handler does.
    let timing = timing::RequestTiming::start(deps.inflight.latency.clone());
    if !authorized(headers, deps) {
        return Err(err_json(
            "unauthorized",
            "missing or valid api key required",
        ));
    }
    // SPEC §10 (owner decision 2026-10-19): a malformed `X-Request-Id` is the
    // client's error, refused before the body is read.
    ids.check()?;
    // SPEC §10 (T19): the body is read under the router's configured bound
    // and no other. axum's implicit 2 MiB `Bytes` limit would refuse a
    // multimodal body the configured bound admits (with a plain-text 413) and
    // ignore a smaller one. An oversized declared length is refused before
    // any of it is read; the request is buffered once, then parsed.
    let body = axum::body::to_bytes(body, deps.limits.max_buffered_bytes_total)
        .await
        .map_err(|_| err_json("request_too_large", "request exceeds buffered-bytes bound"))?;
    // Issue #220: the engine receives these bytes, not a re-encoding, so the
    // caller's key order survives; a duplicated key is refused, so what is
    // validated here is what an engine reads.
    let body_bytes = body.len();
    let request = capyctl_adapters::request::InferenceRequest::parse(body)
        .map_err(|e| err_json("invalid_request", &format!("bad json: {e}")))?;
    let Some(model) = request["model"].as_str().map(str::to_owned) else {
        return Err(err_json("invalid_request", "model is required"));
    };
    // SPEC §10, T19 T21: a request that can never be forwarded is refused
    // before any accounting, activation or engine is touched.
    endpoint.validate(&request)?;
    // The parsed request holds the bytes it was parsed from.
    Ok(Received {
        at,
        timing,
        request,
        model,
        body_bytes,
    })
}

/// `POST /v1/chat/completions` — authenticate → resolve → admit → dispatch.
/// SPEC §10 (owner decision 2026-10-19): every answer, error or success,
/// carries the request's IDs.
async fn chat_completions(
    state: axum::extract::State<AppState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let ids = Arc::new(request_id::RequestIds::read(&headers));
    let outcome = chat_completion(&state.deps, &headers, body, &ids).await;
    ids.stamp(outcome)
}

async fn chat_completion(
    deps: &RouterDeps,
    headers: &HeaderMap,
    body: axum::body::Body,
    ids: &Arc<request_id::RequestIds>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let endpoint = endpoint::Endpoint::ChatCompletions;
    let Received {
        at: received,
        mut timing,
        request: v,
        model,
        body_bytes,
    } = receive(deps, headers, ids, body, endpoint).await?;
    if v["stream"].as_bool() == Some(true) {
        // Streaming path: resolve + activate, then bridge the engine's SSE.
        // SPEC §10 step 1 and T19: a request that must wait holds its body's
        // bytes, and one that finds the in-flight bound reached waits for a
        // slot in arrival order within its deadline. Accounting is registered
        // before the response is built; the stream guard releases when the
        // backend stream ends.
        let (deployment_id, guard, view) =
            chat::admit_timed(deps, endpoint, &model, body_bytes, received, &mut timing).await?;
        // ADR 0013 §10 (I3): choose an instance; SPEC §10: its durable lease is
        // held from before the first byte reaches the engine until the backend
        // stream ends. Refusals before any offer are ordinary HTTP errors.
        let started = std::time::Instant::now();
        let deadline = received + deps.inflight.waiting.limits().deadline;
        let mut plan = balance::Plan::new(deps, &deployment_id, view, ids.clone(), deadline)?;
        timing.selected(started.elapsed());
        let started = std::time::Instant::now();
        let first = plan.next().await?;
        timing.leased(started.elapsed());
        // SPEC §17: the phases known before the first byte; the rest follow
        // in the stream's timing comment.
        let header = timing.header_enabled().then(|| timing.header_value());
        let bounds = stream::StreamBounds::for_request(received, &deps.inflight.waiting.limits());
        let sse = stream::stream_planned_timed(first, Some(plan), v, guard, timing, bounds);
        let mut response = sse.into_response();
        if let Some(value) = header.and_then(|h| h.parse().ok()) {
            response.headers_mut().insert(timing::TIMING_HEADER, value);
        }
        return Ok(response);
    }
    let (mut answer, timing) = chat::dispatch_timed(
        deps,
        endpoint,
        &model,
        v,
        body_bytes,
        received,
        timing,
        ids.clone(),
    )
    .await?;
    // SPEC §17 (owner decision 2026-10-09): every answer carries its figures.
    engine_metrics::attach(&mut answer, &timing);
    Ok(with_timing(chat::relay(&answer)?, &timing))
}

/// `POST /v1/systemone` — ADR 0030 §2: a decision request, admitted, leased
/// and settled exactly as a chat completion, for `api: decisions` deployments
/// only. It is never streamed. The answer is the engine's bytes, with `model`
/// the public name, plus the request's figures in `capyctl.metrics` (SPEC
/// §17, owner decision 2026-10-19; see [`engine_metrics::attach_decision`]);
/// everything else, `usage` and every probability, is as the engine wrote it
/// (SPEC §10, issue #223). Its timings are recorded like any request's, and
/// it carries the request's IDs as a chat answer does (SPEC §10).
async fn systemone(
    state: axum::extract::State<AppState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let ids = Arc::new(request_id::RequestIds::read(&headers));
    let outcome = decision(&state.deps, &headers, body, &ids).await;
    ids.stamp(outcome)
}

async fn decision(
    deps: &RouterDeps,
    headers: &HeaderMap,
    body: axum::body::Body,
    ids: &Arc<request_id::RequestIds>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let endpoint = endpoint::Endpoint::Decisions;
    let received = receive(deps, headers, ids, body, endpoint).await?;
    let (mut answer, timing) = chat::dispatch_timed(
        deps,
        endpoint,
        &received.model,
        received.request,
        received.body_bytes,
        received.at,
        received.timing,
        ids.clone(),
    )
    .await?;
    engine_metrics::attach_decision(&mut answer, &timing);
    Ok(with_timing(chat::relay(&answer)?, &timing))
}

/// SPEC §17: the request's timing header, when the operator enabled it.
fn with_timing(
    mut response: axum::response::Response,
    timing: &timing::RequestTiming,
) -> axum::response::Response {
    if timing.header_enabled() {
        if let Ok(value) = timing.header_value().parse() {
            response.headers_mut().insert(timing::TIMING_HEADER, value);
        }
    }
    response
}
