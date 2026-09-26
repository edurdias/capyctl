//! Design §9: the inference listener's effective bind and authentication, as
//! the role bound it, so `mllm status` can repeat the start warning
//! (`inference: unauthenticated on <addr>`). The role sets the view once it
//! has decided the bind and authentication for the run; until then it is
//! `null`.
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

/// The inference listener's view (`{bind, authenticated}`), set by the role.
#[derive(Default)]
pub struct InferenceListenerView(RwLock<Option<serde_json::Value>>);

impl InferenceListenerView {
    /// Record the listener the role serves for this run.
    pub fn set(&self, view: serde_json::Value) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(view);
    }

    fn get(&self) -> serde_json::Value {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or(serde_json::Value::Null)
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
    }))
    .into_response()
}
