//! Router crate: public inference surface (F1 design §5). The router owns
//! admission, queue bounds, and dispatch; it never chooses models or runs a
//! second scheduler (SPEC §10), and never imports engine crates — dispatch
//! reaches engines through adapters behind `ChatForward`.

pub mod admission;
pub mod chat;
pub mod stream;
pub mod switch;

pub use switch::WakeJoin;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;

use mllm_adapters::traits::ChatForward;

/// Queue bounds (F1 design §5 / T19): bounded queues, explicit deadlines.
#[derive(Debug, Clone)]
pub struct QueueLimits {
    pub max_requests_per_deployment: usize,
    pub max_buffered_bytes_total: usize,
}

#[derive(Clone)]
pub struct RouterDeps {
    pub store: Arc<Mutex<mllm_store::Store>>,
    /// The lifecycle authority, named by port rather than by implementation, so
    /// which authority runs is a wiring decision rather than a compile-time one.
    pub controller: Arc<dyn mllm_controller::LifecyclePort>,
    /// profile/kind name → inference forwarder.
    pub forwards: HashMap<String, Arc<dyn ChatForward>>,
    pub limits: QueueLimits,
    /// Shared inference API key (F1: single-owner lab; per-client keys later).
    pub api_key: Option<String>,
    /// Conservative in-flight accounting (released only on confirmed end).
    pub inflight: Arc<admission::InFlight>,
    /// Activation join (T15 at the router tier): concurrent requests waking
    /// the same non-READY deployment join ONE wake — no double-spawn, no
    /// duplicate Start operations.
    pub activation_join:
        Arc<WakeJoin<(StatusCode, Json<serde_json::Value>)>>,
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
        .with_state(state)
}

fn authorized(headers: &HeaderMap, deps: &RouterDeps) -> bool {
    match &deps.api_key {
        None => true, // no key configured: local-only default (SPEC §15.2)
        Some(expected) => headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|k| k == expected)
            .unwrap_or(false),
    }
}

fn err_json(code: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        match code {
            "unauthorized" => StatusCode::UNAUTHORIZED,
            "unknown_model" => StatusCode::NOT_FOUND,
            "queue_full" => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        Json(serde_json::json!({ "code": code, "message": message })),
    )
}

/// `GET /v1/models` — lists enabled public ids; NEVER wakes a deployment.
async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !authorized(&headers, &state.deps) {
        return Err(err_json("unauthorized", "missing or invalid api key"));
    }
    let store = state.deps.store.lock().unwrap();
    let ids = store.list_enabled_route_ids().map_err(|e| {
        err_json("internal", &format!("store: {e}"))
    })?;
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
    body: axum::body::Bytes,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    if !authorized(&headers, &state.deps) {
        return Err(err_json("unauthorized", "missing or valid api key required"));
    }
    // Bound the buffered body before parsing (T19: bounded queues).
    if body.len() > state.deps.limits.max_buffered_bytes_total {
        return Err(err_json("queue_full", "request exceeds buffered-bytes bound"));
    }
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        err_json("invalid_request", &format!("bad json: {e}"))
    })?;
    let Some(model) = v["model"].as_str() else {
        return Err(err_json("invalid_request", "model is required"));
    };
    if v["stream"].as_bool() == Some(true) {
        // Streaming path: resolve + activate, then bridge the engine's SSE.
        let (deployment_id, kind) = chat::resolve(&state.deps, model).await?;
        let forward = state
            .deps
            .forwards
            .get(&kind)
            .cloned()
            .ok_or_else(|| {
                err_json("unsupported", &format!("no forwarder for profile {kind}"))
            })?;
        // In-flight bound enforced atomically BEFORE the response is built
        // (T19): accounting is registered synchronously; the stream guard
        // releases when the backend stream ends.
        let guard = state
            .deps
            .inflight
            .try_guard_arc(
                &deployment_id,
                state.deps.limits.max_requests_per_deployment,
            )
            .ok_or_else(|| err_json("queue_full", "deployment in-flight bound reached"))?;
        let sse = stream::stream_response(forward, v, guard);
        return Ok(sse.into_response());
    }
    let json = chat::dispatch(&state.deps, model, &v).await?;
    Ok(Json(json).into_response())}
