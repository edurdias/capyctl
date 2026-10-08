//! SPEC §17 (owner decision 2026-09-23, M80): latency distributions on the
//! management listener.
//!
//! `GET /management/v1/metrics/latency[?deployment=<id>]` reports the router's
//! per-request phases, the host ingress's own timings and the engine
//! histograms hosts forward, each marked with its `source` (`capyctl` or
//! `engine`). The report is composed by the service (see
//! `capyctl_router::timing::latency_report`); this module only authenticates,
//! validates the one query parameter and serves it. Reads are in-memory and
//! bounded; nothing here touches an engine.
//!
//! SPEC §§10, 17 (owner decision 2026-10-08): `GET
//! /management/v1/metrics/load[?deployment=<id>]` reports each deployment's
//! live conditions (composed by `capyctl_router::capacity::capacity_report`):
//! the router's in-flight and waiting requests, and per instance the latest
//! host-reported engine load and the running limit with its source. A
//! deployment ID that names none is `404 not_found`. It reads the store, so
//! reads are bounded like the snapshot's: two at a time, the rest refused.
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
use tokio::sync::Semaphore;

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

/// The routes of this module share one authentication.
trait Authenticated: Send + Sync + 'static {
    fn credentials(&self) -> &ManagementCredentials;
}
impl Authenticated for MetricsState {
    fn credentials(&self) -> &ManagementCredentials {
        &self.credentials
    }
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate::<MetricsState>,
        ))
        .with_state(state)
}

async fn authenticate<S: Authenticated>(
    State(state): State<Arc<S>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if !state.credentials().accepts(&request) {
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

/// SPEC §§10, 17 (owner decision 2026-10-08): the outcome of one load read.
pub enum LoadRead {
    Report(serde_json::Value),
    /// The `deployment` filter names no deployment.
    UnknownDeployment,
    /// The read failed; no detail crosses this boundary.
    Unavailable,
}

/// Composes the load report, optionally for one deployment. Bounded reads
/// only, never activation or an engine call.
pub trait LoadSource: Send + Sync + 'static {
    fn load(&self, deployment: Option<&str>) -> LoadRead;
}

impl<F> LoadSource for F
where
    F: Fn(Option<&str>) -> LoadRead + Send + Sync + 'static,
{
    fn load(&self, deployment: Option<&str>) -> LoadRead {
        self(deployment)
    }
}

struct LoadState {
    credentials: ManagementCredentials,
    source: Arc<dyn LoadSource>,
    /// At most two blocking reads at once, as the snapshot route.
    reads: Arc<Semaphore>,
}
impl Authenticated for LoadState {
    fn credentials(&self) -> &ManagementCredentials {
        &self.credentials
    }
}

pub fn load_router(credentials: ManagementCredentials, source: Arc<dyn LoadSource>) -> Router {
    let state = Arc::new(LoadState {
        credentials,
        source,
        reads: Arc::new(Semaphore::new(2)),
    });
    Router::new()
        .route("/management/v1/metrics/load", get(load))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate::<LoadState>,
        ))
        .with_state(state)
}

async fn load(State(state): State<Arc<LoadState>>, request: Request) -> Response {
    let Ok(deployment) = deployment_filter(request.uri().query()) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    };
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let source = state.source.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        source.load(deployment.as_deref())
    })
    .await
    {
        Ok(LoadRead::Report(report)) => Json(report).into_response(),
        Ok(LoadRead::UnknownDeployment) => (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({"api_version":"1","error":{"code":"not_found",
                "message":"Deployment not found","retryable":false,"operation_id":null,
                "details":{}}}),
            ),
        )
            .into_response(),
        Ok(LoadRead::Unavailable) | Err(_) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false)
        }
    }
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
