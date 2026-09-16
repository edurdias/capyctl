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
        let row = deps
            .controller
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

    // Join the deployment's single activation when not READY (T15:
    // simultaneous requests join one wake; no duplicate processes, no
    // double auto_activate). The wake re-checks readiness inside the join:
    // a concurrent activation may have completed already (activating a
    // now-READY deployment is illegal, not idempotent).
    if observed != mllm_domain::LifecycleState::Ready {
        deps.activation_join
            .join(&deployment_id, || async {
                let now_ready = deps
                    .controller
                    .get_deployment(&deployment_id)
                    .ok()
                    .flatten()
                    .map(|r| r.observed_state == mllm_domain::LifecycleState::Ready)
                    .unwrap_or(true);
                if now_ready {
                    return Ok(0);
                }
                // Auto-activation: wakes on-demand deployments but never
                // undoes an administrative stop (T10; SPEC §6.3).
                let op = deps
                    .controller
                    .auto_activate(&deployment_id)
                    .await
                    .map_err(map_controller)?;
                deps.controller
                    .wait_terminal(&op)
                    .await
                    .map_err(map_controller)?;
                Ok(0)
            })
            .await?;
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

    let forward = deps
        .forwards
        .get(&kind)
        .ok_or_else(|| err("unsupported", &format!("no forwarder for profile {kind}")))?
        .clone();
    // Admission accounting: per-deployment in-flight bound (T19), enforced
    // atomically (check + increment share the lock — no over-admission).
    let guard = deps
        .inflight
        .try_guard(&deployment_id, deps.limits.max_requests_per_deployment)
        .ok_or_else(|| err("queue_full", "deployment in-flight bound reached"))?
        .abandon();
    // From the first poll onward the engine may have accepted work. Dropping
    // this request or receiving an uncertain transport error cannot free it.
    let resp = forward
        .forward_chat(body)
        .await
        .map_err(|_| err("engine_error", "backend completion unverified"))?;
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
            "conflict" => StatusCode::CONFLICT,
            // Still in progress as far as anyone can tell: not a failure the client
            // should read as "nothing happened".
            "activation_uncertain" => StatusCode::SERVICE_UNAVAILABLE,
            "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        Json(serde_json::json!({ "code": code, "message": message })),
    )
}

/// Exhaustive on purpose. The previous catch-all reported every unclassified
/// outcome as an activation failure, which told a client that nothing happened even
/// when the activation was still running. A new fault variant must be a compile
/// error here, not silently absorbed into that claim.
fn map_controller(e: mllm_controller::LifecycleFault) -> (StatusCode, Json<serde_json::Value>) {
    use mllm_controller::LifecycleFault as F;
    match e {
        F::NotFound(d) => err("unknown_model", &format!("deployment {d} vanished")),
        F::Blocked(m) => err("insufficient_resources", &format!("admission blocked: {m}")),
        F::Conflict(m) => err("conflict", &m),
        // The activation may still be running. Saying it failed would invite a
        // client to treat the deployment as untouched.
        F::Uncertain(m) => err("activation_uncertain", &m),
        F::Failed(m) => err("activation_failed", &m),
        F::Unavailable(m) => err("unavailable", &m),
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
