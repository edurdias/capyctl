//! ADR 0008 (owner decision 2026-09-23): the standalone role's embedded engine
//! installations (ADR 0018 §5: one per executable), as registered at boot and as its launches found it since
//! (`measured`, `unmeasured` or `drifted`). A remote host's installations are
//! reported through `/management/v1/hosts` instead.
use crate::error;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use capyctl_controller::installation_gate::EmbeddedInstallations;
use std::sync::Arc;

struct InstallationState {
    installations: Arc<EmbeddedInstallations>,
}

pub fn installation_router(installations: Arc<EmbeddedInstallations>) -> Router {
    let state = Arc::new(InstallationState { installations });
    Router::new()
        .route("/management/v1/installation", get(view))
        .layer(middleware::from_fn(authenticate))
        .with_state(state)
}

/// ADR 0040 §2: the principal is the composition's.
async fn authenticate(request: Request, next: Next) -> Response {
    let mut response = if !crate::authorized(&request) {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
    } else if request.uri().query().is_some() {
        error(StatusCode::BAD_REQUEST, "invalid_request", false)
    } else {
        next.run(request).await
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

async fn view(State(state): State<Arc<InstallationState>>) -> Response {
    // ADR 0018 §5: every installation; `installation` stays the first one, as
    // the single-installation view always named it.
    let views = state.installations.views();
    Json(serde_json::json!({
        "api_version": "1",
        "installation": views.first().cloned().unwrap_or(serde_json::Value::Null),
        "installations": views,
    }))
    .into_response()
}
