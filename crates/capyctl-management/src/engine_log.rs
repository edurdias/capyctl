//! SPEC §13.3 / T21: one instance's engine log on the management listener.
//!
//! `GET /management/v1/deployments/{id}/engine-log[?instance=<index>][&kib=<N>]`
//! returns the bounded, redacted end of the log the instance's current launch
//! writes. `kib` is 1..=256 (default 64); `instance` is the index status
//! shows, required when the deployment has more than one instance. A server
//! asks the instance's host (ADR 0017: only a host that declared
//! `engine_log_tail`); a standalone role reads its embedded host's log
//! in-process. Logs are redacted as they are written and again as they are
//! read; a raw development log (`--debug-engine-logs`) is never served, and
//! no answer names a file path.
use crate::ManagementCredentials;
use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use capyctl_controller::{
    agent_sessions::AgentSessions,
    engine_logs::{self, LaunchScope, ScopeError, Tail, TailFailure},
    ownership::SharedCoordinatorState,
};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

/// SPEC §13.3: the default and largest tail, in KiB.
pub const DEFAULT_KIB: u32 = 64;
pub const MAX_KIB: u32 = capyctl_protocol::execution::MAX_ENGINE_LOG_TAIL_BYTES / 1024;

/// One instance's tail, with the launch it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineLog {
    pub host_id: String,
    pub incarnation: String,
    pub tail: Tail,
}

/// Why no tail is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineLogFailure {
    /// No such deployment or instance.
    NotFound,
    /// The instance holds no running launch.
    NotRunning,
    /// The launch's log could not be served.
    Tail(TailFailure),
}

impl From<ScopeError> for EngineLogFailure {
    fn from(error: ScopeError) -> Self {
        match error {
            ScopeError::NotFound => Self::NotFound,
            ScopeError::NotRunning => Self::NotRunning,
            ScopeError::Unavailable => Self::Tail(TailFailure::Unavailable),
        }
    }
}

pub type InstancesFuture = Pin<Box<dyn Future<Output = Result<Vec<u32>, EngineLogFailure>> + Send>>;
pub type EngineLogFuture =
    Pin<Box<dyn Future<Output = Result<EngineLog, EngineLogFailure>> + Send>>;

/// Where an instance's engine log is read.
pub trait EngineLogSource: Send + Sync + 'static {
    /// The deployment's instance indexes; `NotFound` for an unknown one.
    fn instances(&self, deployment: &str) -> InstancesFuture;
    /// At most `max_bytes` of the instance's current launch's log.
    fn tail(&self, deployment: &str, instance: u32, max_bytes: usize) -> EngineLogFuture;
}

struct EngineLogState {
    credentials: ManagementCredentials,
    source: Arc<dyn EngineLogSource>,
    reads: Arc<tokio::sync::Semaphore>,
}

pub fn engine_log_router(
    credentials: ManagementCredentials,
    source: Arc<dyn EngineLogSource>,
) -> Router {
    let state = Arc::new(EngineLogState {
        credentials,
        source,
        // At most two reads at once, like every other bounded read.
        reads: Arc::new(tokio::sync::Semaphore::new(2)),
    });
    Router::new()
        .route(
            "/management/v1/deployments/{id}/engine-log",
            get(engine_log),
        )
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

async fn authenticate(
    State(state): State<Arc<EngineLogState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if !state.credentials.accepts(&request) {
        crate::error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
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

/// The accepted query: `instance=<index>` and `kib=<1..=256>`, each at most
/// once, digits only.
#[derive(Debug, PartialEq, Eq)]
struct Query {
    instance: Option<u32>,
    kib: u32,
}

fn query(raw: Option<&str>) -> Result<Query, ()> {
    let mut parsed = Query {
        instance: None,
        kib: DEFAULT_KIB,
    };
    let mut kib = None;
    for pair in raw.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or(())?;
        if value.is_empty() || value.len() > 9 || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        let number: u32 = value.parse().map_err(|_| ())?;
        let slot = match key {
            "instance" => &mut parsed.instance,
            "kib" => &mut kib,
            _ => return Err(()),
        };
        if slot.replace(number).is_some() {
            return Err(());
        }
    }
    if let Some(kib) = kib {
        if !(1..=MAX_KIB).contains(&kib) {
            return Err(());
        }
        parsed.kib = kib;
    }
    Ok(parsed)
}

/// A bounded identifier, as every management route takes one.
fn deployment_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// The SPEC §16 error envelope with this route's own message.
fn refusal(status: StatusCode, code: &str, message: &str, retryable: bool) -> Response {
    (
        status,
        Json(serde_json::json!({"api_version":"1","error":{
            "code":code,"message":message,"retryable":retryable,
            "operation_id":null,"details":{}}})),
    )
        .into_response()
}

fn failure(failure: EngineLogFailure, instance: Option<u32>) -> Response {
    let which = instance.map_or_else(|| "the instance".to_owned(), |i| format!("instance {i}"));
    match failure {
        EngineLogFailure::NotFound => refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            "Deployment or instance not found",
            false,
        ),
        EngineLogFailure::NotRunning => refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("{which} has no running launch, so it has no engine log to read"),
            false,
        ),
        EngineLogFailure::Tail(TailFailure::Missing) => refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("no engine log exists for the launch of {which}"),
            false,
        ),
        // SPEC §13.3: raw development logs never reach a management response.
        EngineLogFailure::Tail(TailFailure::Raw) => refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the engine log was written under --debug-engine-logs and is not served: raw development logs never leave the host",
            false,
        ),
        EngineLogFailure::Tail(TailFailure::Unreadable) => refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the engine log could not be read on its host",
            false,
        ),
        // ADR 0017: the typed refusal; nothing was sent to the host.
        EngineLogFailure::Tail(TailFailure::CapabilityMissing(reason)) => refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "unsupported_capability",
            &format!(
                "the host of {which} cannot serve its engine log ({reason}); upgrade the host"
            ),
            false,
        ),
        EngineLogFailure::Tail(TailFailure::HostOffline) => refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "observation_stale",
            &format!("the host of {which} is not connected; retry once it reconnects"),
            true,
        ),
        EngineLogFailure::Tail(TailFailure::DeadlineExceeded) => refusal(
            StatusCode::GATEWAY_TIMEOUT,
            "deadline_exceeded",
            &format!("the host of {which} did not answer in time; retry"),
            true,
        ),
        EngineLogFailure::Tail(TailFailure::Unavailable) => refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "snapshot_unavailable",
            "Current owned state is unavailable; retry",
            true,
        ),
    }
}

async fn engine_log(
    State(state): State<Arc<EngineLogState>>,
    Path(deployment): Path<String>,
    request: Request,
) -> Response {
    let invalid =
        |message: &str| refusal(StatusCode::BAD_REQUEST, "invalid_request", message, false);
    let Ok(query) = query(request.uri().query()) else {
        return invalid(&format!(
            "the query accepts instance=<index> and kib=<1..={MAX_KIB}>, each at most once"
        ));
    };
    if !deployment_ok(&deployment) {
        return invalid("Invalid deployment id");
    }
    let Ok(_permit) = state.reads.clone().try_acquire_owned() else {
        return crate::error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let instances = match state.source.instances(&deployment).await {
        Ok(instances) => instances,
        Err(error) => return failure(error, query.instance),
    };
    let instance = match query.instance {
        Some(index) if instances.contains(&index) => index,
        Some(index) => return failure(EngineLogFailure::NotFound, Some(index)),
        None => match instances.as_slice() {
            [only] => *only,
            _ => {
                return invalid(&format!(
                    "the deployment has {} instances; name one with instance=<index> (status shows each index)",
                    instances.len()
                ))
            }
        },
    };
    let max_bytes = query.kib as usize * 1024;
    match state.source.tail(&deployment, instance, max_bytes).await {
        Ok(log) => Json(serde_json::json!({
            "api_version": "1",
            "deployment_id": deployment,
            "instance": instance,
            "host_id": log.host_id,
            "incarnation": log.incarnation,
            "kib": query.kib,
            "truncated": log.tail.truncated,
            "text": log.tail.text,
            "read_at_ms": capyctl_protocol::now_unix_ms(),
        }))
        .into_response(),
        Err(error) => failure(error, Some(instance)),
    }
}

/// A store read under the owner, off the runtime.
async fn owned<T: Send + 'static>(
    owner: &SharedCoordinatorState,
    read: impl FnOnce(&capyctl_store::Store) -> Result<T, ScopeError> + Send + 'static,
) -> Result<T, EngineLogFailure> {
    let owner = owner.clone();
    tokio::task::spawn_blocking(move || {
        let owner = owner.lock().map_err(|_| ScopeError::Unavailable)?;
        read(owner.store())
    })
    .await
    .map_err(|_| EngineLogFailure::Tail(TailFailure::Unavailable))?
    .map_err(EngineLogFailure::from)
}

fn scope_of(
    owner: &SharedCoordinatorState,
    deployment: &str,
    instance: u32,
) -> impl Future<Output = Result<LaunchScope, EngineLogFailure>> + Send + 'static {
    let owner = owner.clone();
    let deployment = deployment.to_owned();
    async move {
        owned(&owner, move |store| {
            engine_logs::launch_scope(store, &deployment, instance)
        })
        .await
    }
}

fn instances_of(owner: &SharedCoordinatorState, deployment: &str) -> InstancesFuture {
    let owner = owner.clone();
    let deployment = deployment.to_owned();
    Box::pin(async move {
        owned(&owner, move |store| {
            engine_logs::instance_indexes(store, &deployment)
        })
        .await
    })
}

/// A server's source: the instance's host is asked through its control
/// session.
pub struct RemoteEngineLogs {
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
    controller_id: String,
}

impl RemoteEngineLogs {
    pub fn new(
        owner: SharedCoordinatorState,
        sessions: Arc<AgentSessions>,
        controller_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            sessions,
            controller_id,
        })
    }
}

impl EngineLogSource for RemoteEngineLogs {
    fn instances(&self, deployment: &str) -> InstancesFuture {
        instances_of(&self.owner, deployment)
    }
    fn tail(&self, deployment: &str, instance: u32, max_bytes: usize) -> EngineLogFuture {
        let scope = scope_of(&self.owner, deployment, instance);
        let sessions = self.sessions.clone();
        let controller_id = self.controller_id.clone();
        let deployment = deployment.to_owned();
        Box::pin(async move {
            let scope = scope.await?;
            let tail = engine_logs::remote_tail(
                &sessions,
                &controller_id,
                &deployment,
                instance,
                &scope,
                u32::try_from(max_bytes).unwrap_or(u32::MAX),
            )
            .await
            .map_err(EngineLogFailure::Tail)?;
            Ok(EngineLog {
                host_id: scope.host_id,
                incarnation: scope.incarnation,
                tail,
            })
        })
    }
}

/// A standalone role's source: its embedded host's log, read in-process
/// from `<state>/logs/<deployment id>/<incarnation>.log`.
pub struct StandaloneEngineLogs {
    owner: SharedCoordinatorState,
    log_dir: PathBuf,
}

impl StandaloneEngineLogs {
    pub fn new(owner: SharedCoordinatorState, log_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { owner, log_dir })
    }
}

impl EngineLogSource for StandaloneEngineLogs {
    fn instances(&self, deployment: &str) -> InstancesFuture {
        instances_of(&self.owner, deployment)
    }
    fn tail(&self, deployment: &str, instance: u32, max_bytes: usize) -> EngineLogFuture {
        let scope = scope_of(&self.owner, deployment, instance);
        let log_dir = self.log_dir.clone();
        let deployment = deployment.to_owned();
        Box::pin(async move {
            let scope = scope.await?;
            let incarnation = scope.incarnation.clone();
            let tail = tokio::task::spawn_blocking(move || {
                let log = engine_logs::standalone_log(&log_dir, &deployment, &incarnation)
                    .ok_or(TailFailure::Missing)?;
                engine_logs::local_tail(&log, max_bytes)
            })
            .await
            .map_err(|_| EngineLogFailure::Tail(TailFailure::Unreadable))?
            .map_err(EngineLogFailure::Tail)?;
            Ok(EngineLog {
                host_id: scope.host_id,
                incarnation: scope.incarnation,
                tail,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T21: the query takes two bounded integers, each once.
    #[test]
    fn only_bounded_integers_are_accepted() {
        assert_eq!(
            query(None),
            Ok(Query {
                instance: None,
                kib: DEFAULT_KIB
            })
        );
        assert_eq!(
            query(Some("instance=2&kib=256")),
            Ok(Query {
                instance: Some(2),
                kib: 256
            })
        );
        for bad in [
            "kib=0",
            "kib=257",
            "kib=+1",
            "kib=1e2",
            "kib=99999999999",
            "instance",
            "instance=1&instance=1",
            "x=1",
        ] {
            assert!(query(Some(bad)).is_err(), "{bad}");
        }
    }
}
