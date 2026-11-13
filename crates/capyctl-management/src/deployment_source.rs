//! ADR 0040 §9 / C2: Edit uses the accepted source, never an effective revision.
use crate::{
    configuration::{accept_blocking, redact_effective, ConfigurationFailure},
    AppState,
};
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) async fn get(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(source) = state.configuration.clone() else {
        return ConfigurationFailure::Unsupported.response();
    };
    if id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return ConfigurationFailure::InvalidRequest.response();
    }
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return ConfigurationFailure::QueueFull.response();
    };
    match accept_blocking(permit, move || source.deployment_source(&id)).await {
        Ok(Some(mut view)) => {
            redact_effective(&mut view["config"]);
            view["api_version"] = Value::String("1".into());
            view["yaml"] = json!(capyctl_config::yaml_emit::to_block_yaml(&view["config"]));
            Json(view).into_response()
        }
        Ok(None) => crate::error(
            axum::http::StatusCode::NOT_FOUND,
            "deployment_not_found",
            false,
        ),
        Err(error) => error.response(),
    }
}
