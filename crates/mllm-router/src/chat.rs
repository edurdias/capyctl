//! Chat dispatch: resolve the alias to an explicit deployment, admit, and
//! forward through the deployment's adapter (F1 design §5). Admission
//! joins one activation operation when the deployment is not READY
//! (T15 groundwork; full switching lives in `switch.rs`).

use mllm_domain::LifecycleState;

use crate::admission;
use crate::RouterDeps;

pub async fn dispatch(
    deps: &RouterDeps,
    model: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, (String, String)> {
    // Resolve alias → explicit deployment (no model-name guessing).
    let (deployment_id, kind, observed) = {
        let store = deps.store.lock().unwrap();
        let row = store
            .find_deployment_by_route(model)
            .map_err(|e| ("internal".to_string(), format!("store: {e}")))?
            .ok_or_else(|| {
                (
                    "unknown_model".to_string(),
                    format!("no deployment serves model id {model}"),
                )
            })?;
        // admission_enabled/suspended live in the F3 remote schema; F1's
        // in-process graph treats accepted deployments as enabled (SPEC
        // §6.3: an on-demand STOPPED deployment is a successful accept).
        (row.id.clone(), row.kind.clone(), row.observed_state)
    };

    // Join the deployment's single activation operation when not READY
    // (simultaneous requests join one wake — T15).
    if observed != LifecycleState::Ready {
        let op = deps
            .controller
            .request_transition(&deployment_id, mllm_domain::LifecycleAction::Start)
            .await
            .map_err(map_controller)?;
        deps.controller
            .wait_terminal(&op)
            .await
            .map_err(map_controller)?;
    }

    // Admission accounting: per-deployment in-flight bound.
    admission::admit_request(deps, &deployment_id)?;

    let forward = deps
        .forwards
        .get(&kind)
        .ok_or_else(|| {
            (
                "unsupported".to_string(),
                format!("no forwarder for profile {kind}"),
            )
        })?
        .clone();
    let resp = forward
        .forward_chat(body)
        .await
        .map_err(|e| ("engine_error".to_string(), format!("engine: {e:?}")))?;
    Ok(resp)
}

fn map_controller(e: mllm_controller::ControllerError) -> (String, String) {
    match e {
        mllm_controller::ControllerError::Blocked(b) => {
            ("insufficient_resources".into(), format!("admission blocked: {b:?}"))
        }
        other => ("activation_failed".into(), other.to_string()),
    }
}
