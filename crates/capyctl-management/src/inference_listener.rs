//! Design §9: the inference listener's effective bind and authentication, as
//! the role bound it, so `capyctl status` can repeat the start warning
//! (`inference: unauthenticated on <addr>`). The role sets the view once it
//! has decided the bind and authentication for the run; until then it is
//! `null`. ADR 0037 §13: the same read carries the role's tracing status
//! (`tracing: off` or on with its endpoint, spans dropped and the last export
//! error), read live at each request.
use crate::{error, ManagementCredentials};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::{Arc, RwLock};
/// A live reading of the role's tracing status (ADR 0037 §13).
pub type TracingStatus = Arc<dyn Fn() -> serde_json::Value + Send + Sync>;

/// The inference listener's view (`{bind, authenticated}`), set by the role,
/// and the role's tracing status.
#[derive(Default)]
pub struct InferenceListenerView {
    listener: RwLock<Option<serde_json::Value>>,
    tracing: RwLock<Option<TracingStatus>>,
}

impl InferenceListenerView {
    /// Record the listener the role serves for this run.
    pub fn set(&self, view: serde_json::Value) {
        *self
            .listener
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(view);
    }

    /// ADR 0037 §13: record where the role's tracing status is read.
    pub fn set_tracing(&self, status: TracingStatus) {
        *self
            .tracing
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(status);
    }

    fn get(&self) -> serde_json::Value {
        self.listener
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or(serde_json::Value::Null)
    }

    fn tracing(&self) -> serde_json::Value {
        let status = self
            .tracing
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        status.map_or(serde_json::Value::Null, |status| status())
    }
}

struct ListenerState {
    credentials: ManagementCredentials,
    view: Arc<InferenceListenerView>,
}

/// `GET /management/v1/inference-listener`, served only to the management
/// credential (SPEC §16.5).
pub fn inference_listener_router(
    credentials: ManagementCredentials,
    view: Arc<InferenceListenerView>,
) -> Router {
    let state = Arc::new(ListenerState { credentials, view });
    Router::new()
        .route("/management/v1/inference-listener", get(current))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

async fn authenticate(
    State(state): State<Arc<ListenerState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if !state.credentials.accepts(&request) {
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

async fn current(State(state): State<Arc<ListenerState>>) -> Response {
    Json(serde_json::json!({
        "api_version": "1",
        "inference_listener": state.view.get(),
        "tracing": state.view.tracing(),
    }))
    .into_response()
}
