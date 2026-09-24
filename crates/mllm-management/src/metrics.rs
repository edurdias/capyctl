//! SPEC §17 (owner decision 2026-09-23, M80): latency distributions on the
//! management listener.
//!
//! `GET /management/v1/metrics/latency[?deployment=<id>]` reports the router's
//! per-request phases, the host ingress's own timings and the engine
//! histograms hosts forward, each marked with its `source` (`mllm` or
//! `engine`). The report is composed by the service (see
//! `mllm_router::timing::latency_report`); this module only authenticates,
//! validates the one query parameter and serves it. Reads are in-memory and
//! bounded; nothing here touches an engine.
use crate::{error, ManagementCredentials};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;

/// Composes the latency report, optionally for one deployment.
pub trait LatencySource: Send + Sync + 'static {
    fn latency(&self, deployment: Option<&str>) -> serde_json::Value;
}

impl<F> LatencySource for F
where
    F: Fn(Option<&str>) -> serde_json::Value + Send + Sync + 'static,
{
    fn latency(&self, deployment: Option<&str>) -> serde_json::Value {
        self(deployment)
    }
}

struct MetricsState {
    credentials: ManagementCredentials,
    source: Arc<dyn LatencySource>,
}

pub fn latency_router(
    credentials: ManagementCredentials,
    source: Arc<dyn LatencySource>,
) -> Router {
    let state = Arc::new(MetricsState {
        credentials,
        source,
    });
    Router::new()
        .route("/management/v1/metrics/latency", get(latency))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

async fn authenticate(
    State(state): State<Arc<MetricsState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if !state.credentials.accepts(&request) {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
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

/// The one accepted query: `deployment=<id>`, a bounded identifier.
fn deployment_filter(query: Option<&str>) -> Result<Option<String>, ()> {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Ok(None);
    };
    let value = query.strip_prefix("deployment=").ok_or(())?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(());
    }
    Ok(Some(value.to_owned()))
}

async fn latency(State(state): State<Arc<MetricsState>>, request: Request) -> Response {
    let Ok(deployment) = deployment_filter(request.uri().query()) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    };
    Json(state.source.latency(deployment.as_deref())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bounded_deployment_filter_is_accepted() {
        assert_eq!(deployment_filter(None), Ok(None));
        assert_eq!(
            deployment_filter(Some("deployment=dep_1-a")),
            Ok(Some("dep_1-a".into()))
        );
        assert!(deployment_filter(Some("deployment=")).is_err());
        assert!(deployment_filter(Some("deployment=a&x=1")).is_err());
        assert!(deployment_filter(Some("other=1")).is_err());
    }
}
