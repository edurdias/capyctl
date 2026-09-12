//! Chat dispatch: resolve the alias to an explicit deployment, admit, and
//! forward through the deployment's adapter (F1 design §5). Admission
//! joins one activation operation when the deployment is not READY
//! (simultaneous requests join one wake — T15).

use async_trait::async_trait;

use axum::http::StatusCode;
use axum::Json;

use crate::RouterDeps;

/// Resolve the alias to an explicit deployment (no model-name guessing —
/// SPEC §10) and ensure READY, joining the single activation operation.
/// Returns (deployment id, profile kind).
pub async fn resolve(
    deps: &RouterDeps,
    model: &str,
) -> Result<(String, String), (StatusCode, Json<serde_json::Value>)> {
    let (deployment_id, kind, observed) = {
        let store = deps.store.lock().unwrap();
        let row = store
            .find_deployment_by_route(model)
            .map_err(|e| err("internal", &format!("store: {e}")))?
            .ok_or_else(|| {
                err(
                    "unknown_model",
                    &format!("no deployment serves model id {model}"),
                )
            })?;
        (row.id.clone(), row.kind.clone(), row.observed_state)
    };

    // Join the deployment's single activation operation when not READY
    // (T15: simultaneous requests join one wake; no duplicate processes).
    if observed != mllm_domain::LifecycleState::Ready {
        // Auto-activation: wakes on-demand deployments but never undoes an
        // administrative stop (T10; SPEC §6.3).
        let op = deps
            .controller
            .auto_activate(&deployment_id)
            .await
            .map_err(map_controller)?;
        deps.controller
            .wait_terminal(&op)
            .await
            .map_err(map_controller)?;
    }

    Ok((deployment_id, kind))
}

/// Non-streaming dispatch: resolve + forward through the deployment's
/// adapter; the in-flight guard releases on completion (accounting is
/// conservative on every other path).
pub async fn dispatch(
    deps: &RouterDeps,
    model: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let (deployment_id, kind) = resolve(deps, model).await?;

    // Admission accounting: per-deployment in-flight bound (T19).
    if deps.inflight.current(&deployment_id) >= deps.limits.max_requests_per_deployment {
        return Err(err("queue_full", "deployment in-flight bound reached"));
    }
    let forward = deps
        .forwards
        .get(&kind)
        .ok_or_else(|| err("unsupported", &format!("no forwarder for profile {kind}")))?
        .clone();
    let guard = deps.inflight.guard(&deployment_id);
    let resp = forward
        .forward_chat(body)
        .await
        .map_err(|e| err("engine_error", &format!("engine: {e:?}")))?;
    guard.release();
    Ok(resp)
}

fn err(code: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        match code {
            "unknown_model" => StatusCode::NOT_FOUND,
            "queue_full" => StatusCode::PAYLOAD_TOO_LARGE,
            "unsupported" => StatusCode::NOT_IMPLEMENTED,
            "insufficient_resources" => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        Json(serde_json::json!({ "code": code, "message": message })),
    )
}

fn map_controller(e: mllm_controller::ControllerError) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        mllm_controller::ControllerError::Blocked(b) => {
            err("insufficient_resources", &format!("admission blocked: {b:?}"))
        }
        mllm_controller::ControllerError::UnknownDeployment(d) => {
            err("unknown_model", &format!("deployment {d} vanished"))
        }
        other => err("activation_failed", &other.to_string()),
    }
}

/// Placeholder forwarder for attached deployments: an attached service is
/// routed through its endpoint URL at dispatch time (F3+ wiring); in F1 the
/// attached chat path reports unsupported rather than improvising.
pub struct NoForward;

#[async_trait]
impl mllm_adapters::traits::ChatForward for NoForward {
    async fn forward_chat(
        &self,
        _body: &serde_json::Value,
    ) -> Result<serde_json::Value, mllm_adapters::traits::AdapterError> {
        Err(mllm_adapters::traits::AdapterError::UnsupportedCapability)
    }
}