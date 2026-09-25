//! Preparatory management boundaries. Not the complete A3 API.
//!
//! Optional mutations accept stopped configurations and owned Start/Stop
//! commands. No listener or inference routes are composed here.
//! The trusted service must resolve independent credentials and
//! mount this router ONLY on its separate loopback (or TLS) management listener.
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use mllm_store::{snapshot::Snapshot, Store};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
pub mod actions;
pub mod configuration;
mod credentials;
pub mod drain;
pub mod enrollment;
pub mod events;
pub mod hosts;
// ADR 0008: the standalone role's embedded installation.
pub mod installation;
// SPEC §17 (M80): router, ingress and engine latency distributions.
pub mod metrics;

/// Hashes only; intentionally neither Debug nor Serialize. This validates token
/// syntax/distinctness, not randomness or provenance. Inputs MUST come from the
/// trusted service's protected resolver, never deployment/HTTP configuration.
pub struct ManagementCredentials {
    digest: [u8; 32],
}
impl ManagementCredentials {
    pub fn from_trusted_resolver(management: &str, inference: &str) -> Result<Self, &'static str> {
        if !valid_token(management) || !valid_token(inference) {
            return Err("invalid management credentials");
        }
        let digest: [u8; 32] = Sha256::digest(management.as_bytes()).into();
        let other: [u8; 32] = Sha256::digest(inference.as_bytes()).into();
        if bool::from(digest.ct_eq(&other)) {
            return Err("invalid management credentials");
        }
        Ok(Self { digest })
    }
    fn accepts(&self, request: &Request) -> bool {
        let mut headers = request.headers().get_all(header::AUTHORIZATION).iter();
        let Some(value) = headers.next() else {
            return false;
        };
        if headers.next().is_some() {
            return false;
        }
        let Some(token) = value.to_str().ok().and_then(|s| s.strip_prefix("Bearer ")) else {
            return false;
        };
        if !valid_token(token) {
            return false;
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        bool::from(self.digest.ct_eq(&digest))
    }
}
fn valid_token(value: &str) -> bool {
    (32..=256).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
}

/// No raw source errors cross this boundary.
#[derive(Debug)]
pub struct SnapshotUnavailable;
/// Service-owned provider; must perform only bounded snapshot reads, never
/// activation, freshness refresh, engine callbacks or credential resolution.
pub trait SnapshotSource: Send + Sync + 'static {
    fn snapshot(&self) -> Result<Snapshot, SnapshotUnavailable>;
}
pub struct StoreSnapshotSource {
    store: Mutex<Store>,
}
impl StoreSnapshotSource {
    pub fn new(store: Store) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }
}
impl SnapshotSource for StoreSnapshotSource {
    fn snapshot(&self) -> Result<Snapshot, SnapshotUnavailable> {
        self.store
            .lock()
            .map_err(|_| SnapshotUnavailable)?
            .snapshot()
            .map_err(|_| SnapshotUnavailable)
    }
}
struct AppState {
    credentials: ManagementCredentials,
    source: Arc<dyn SnapshotSource>,
    reads: Arc<Semaphore>,
    events: Option<Arc<dyn events::EventSource>>,
    streams: Arc<Semaphore>,
    event_options: events::EventStreamOptions,
    configuration: Option<Arc<dyn configuration::ConfigurationSource>>,
    actions: Option<Arc<dyn actions::ActionSource>>,
    commands_in_flight: Arc<Semaphore>,
    /// Owner decision 2026-09-23: evicting starts in flight. Each runs as its
    /// own task past its command slot (it may drain for the whole switch
    /// bound), so this bounds those tasks instead.
    evictions: Arc<Semaphore>,
}

/// At most two queued/running blocking reads per router. Cancellation retains a
/// permit inside its worker until the actual read completes. No unbounded queue.
pub fn snapshot_router(
    credentials: ManagementCredentials,
    source: Arc<dyn SnapshotSource>,
) -> Router {
    let state = Arc::new(AppState {
        credentials,
        source,
        reads: Arc::new(Semaphore::new(2)),
        events: None,
        streams: Arc::new(Semaphore::new(0)),
        event_options: events::EventStreamOptions::default(),
        configuration: None,
        actions: None,
        commands_in_flight: Arc::new(Semaphore::new(0)),
        evictions: Arc::new(Semaphore::new(actions::MAX_EVICTING_STARTS)),
    });
    routes(state, false)
}

/// Combined historical snapshot and durable SSE provider. Mount once on the
/// separately secured management listener; this does not start a listener.
pub fn read_only_router<T: SnapshotSource + events::EventSource>(
    credentials: ManagementCredentials,
    source: Arc<T>,
) -> Router {
    read_only_router_with_event_options(credentials, source, events::EventStreamOptions::default())
        .expect("default event options are bounded")
}

/// Configurable service limits may only tighten the built-in upper bounds.
pub fn read_only_router_with_event_options<T: SnapshotSource + events::EventSource>(
    credentials: ManagementCredentials,
    source: Arc<T>,
    options: events::EventStreamOptions,
) -> Result<Router, &'static str> {
    options.validate()?;
    let state = Arc::new(AppState {
        credentials,
        source: source.clone(),
        reads: Arc::new(Semaphore::new(2)),
        events: Some(source),
        streams: Arc::new(Semaphore::new(options.max_streams)),
        event_options: options,
        configuration: None,
        actions: None,
        commands_in_flight: Arc::new(Semaphore::new(0)),
        evictions: Arc::new(Semaphore::new(actions::MAX_EVICTING_STARTS)),
    });
    Ok(routes(state, true))
}

/// Adds only stopped managed configuration commands to snapshot/SSE. Activation,
/// lifecycle actions, listener startup and full A3 composition remain unavailable.
pub fn configuration_router<
    T: SnapshotSource + events::EventSource + configuration::ConfigurationSource,
>(
    credentials: ManagementCredentials,
    source: Arc<T>,
) -> Router {
    let options = events::EventStreamOptions::default();
    let state = Arc::new(AppState {
        credentials,
        source: source.clone(),
        reads: Arc::new(Semaphore::new(2)),
        events: Some(source.clone()),
        streams: Arc::new(Semaphore::new(options.max_streams)),
        event_options: options,
        configuration: Some(source),
        actions: None,
        commands_in_flight: Arc::new(Semaphore::new(2)),
        evictions: Arc::new(Semaphore::new(actions::MAX_EVICTING_STARTS)),
    });
    routes(state, true)
}

/// Compose Start and Stop with the same owned snapshot and configuration
/// authority. Retain OwnedCoordinator outside this router.
pub fn lifecycle_router(
    credentials: ManagementCredentials,
    source: Arc<actions::OwnedActionSource>,
) -> Router {
    let options = events::EventStreamOptions::default();
    routes(
        Arc::new(AppState {
            credentials,
            source: source.clone(),
            reads: Arc::new(Semaphore::new(2)),
            events: Some(source.clone()),
            streams: Arc::new(Semaphore::new(options.max_streams)),
            event_options: options,
            configuration: Some(source.clone()),
            actions: Some(source),
            commands_in_flight: Arc::new(Semaphore::new(2)),
            evictions: Arc::new(Semaphore::new(actions::MAX_EVICTING_STARTS)),
        }),
        true,
    )
}

fn routes(state: Arc<AppState>, include_events: bool) -> Router {
    let router = Router::new().route("/management/v1/snapshot", get(snapshot).head(method_denied));
    let router = if state.actions.is_some() {
        router
            .route(
                "/management/v1/deployments/{id}/actions",
                axum::routing::post(actions::accept),
            )
            // Owner decision Q7: per-instance `start` and `stop`.
            .route(
                "/management/v1/deployments/{id}/instances/{index}/actions",
                axum::routing::post(actions::accept_instance),
            )
    } else {
        router
    };
    let router = if include_events {
        router.route(
            "/management/v1/events",
            get(events::subscribe).head(method_denied),
        )
    } else {
        router
    };
    let router = if state.configuration.is_some() {
        router
            .route(
                "/management/v1/deployments",
                axum::routing::post(configuration::accept),
            )
            .route(
                "/management/v1/deployments/{id}",
                axum::routing::put(configuration::accept),
            )
            // SPEC §8.2: `inspect deployment --effective-config`.
            .route(
                "/management/v1/deployments/{id}/effective-config",
                get(configuration::effective_config),
            )
            // SPEC §6.3, ADR 0008: the store keys deployments still reference,
            // for the host-side `mllm prune sources`.
            .route(
                "/management/v1/model-sources",
                get(configuration::model_sources).head(method_denied),
            )
    } else {
        router
    };
    router
        .fallback(not_found)
        .method_not_allowed_fallback(method_denied)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}
async fn authenticate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if state.credentials.accepts(&request) {
        next.run(request).await
    } else {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
async fn snapshot(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if request.uri().query().is_some() {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    }
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let source = state.source.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        source.snapshot()
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}
async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", false)
}
async fn method_denied() -> Response {
    error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", false)
}
fn error(status: StatusCode, code: &'static str, retryable: bool) -> Response {
    let message = match code {
        "unauthenticated" => "Management authentication required",
        "invalid_request" => "Invalid request",
        "unsupported_media_type" => "JSON content type required",
        "identity_conflict" => "Host identity request conflicts with current state",
        "queue_full" => "Read capacity exhausted",
        "not_found" => "Route not found",
        "method_not_allowed" => "Method not allowed",
        _ => "Management read failed",
    };
    let details = if code == "cursor_expired" {
        serde_json::json!({"resnapshot_required":true})
    } else {
        serde_json::json!({})
    };
    (status, Json(serde_json::json!({"api_version":"1","error":{"code":code,"message":message,"retryable":retryable,"operation_id":null,"details":details}}))).into_response()
}
