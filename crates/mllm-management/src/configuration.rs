//! Stopped configuration commands only; no activation or engine callbacks.
use crate::{events::EventSource, AppState, SnapshotSource, SnapshotUnavailable};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use mllm_controller::OwnedCoordinatorState;
use mllm_store::{
    events::{EventPage, EventReadError},
    managed_configuration::{ManagedConfigurationError, ManagedConfigurationReceipt},
    snapshot::Snapshot,
};
use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use std::{
    error::Error,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod json_shape;
const MAX_BODY: usize = 1 << 20;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Closed public categories; never carries raw SQL, input or provider errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationFailure {
    InvalidRequest,
    InvalidConfig,
    BodyTooLarge,
    QueueFull,
    DeadlineExceeded,
    Unsupported,
    NotFound,
    RevisionConflict,
    IdempotencyConflict,
    RouteConflict,
    RuntimeRetained,
    ReconciliationRequired,
    HostPolicyDenied,
    CapacityBlocked,
    Internal,
}
impl ConfigurationFailure {
    pub(crate) fn response(self) -> Response {
        use ConfigurationFailure::*;
        let (status, code, message, retryable) = match self {
            InvalidRequest => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid request",
                false,
            ),
            InvalidConfig => (
                StatusCode::BAD_REQUEST,
                "invalid_config",
                "Invalid deployment configuration",
                false,
            ),
            BodyTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Command body exceeds limit",
                false,
            ),
            QueueFull => (
                StatusCode::TOO_MANY_REQUESTS,
                "queue_full",
                "Command capacity exhausted",
                true,
            ),
            DeadlineExceeded => (
                StatusCode::GATEWAY_TIMEOUT,
                "deadline_exceeded",
                "Command response deadline exceeded; retry with the same idempotency key",
                true,
            ),
            Unsupported => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unsupported_capability",
                "Activation is not available on this configuration boundary",
                false,
            ),
            NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Deployment not found",
                false,
            ),
            RevisionConflict => (
                StatusCode::CONFLICT,
                "revision_conflict",
                "Expected revision does not match",
                false,
            ),
            IdempotencyConflict => (
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "Idempotency key identifies a different command",
                false,
            ),
            RouteConflict => (
                StatusCode::CONFLICT,
                "route_conflict",
                "Deployment name or route is already owned",
                false,
            ),
            RuntimeRetained => (
                StatusCode::CONFLICT,
                "runtime_retained",
                "Runtime ownership requires verified cleanup",
                false,
            ),
            ReconciliationRequired => (
                StatusCode::SERVICE_UNAVAILABLE,
                "reconciliation_required",
                "Current owned state or resource policy is unavailable",
                true,
            ),
            Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Management command failed",
                false,
            ),
            HostPolicyDenied => (
                StatusCode::FORBIDDEN,
                "host_policy_denied",
                "Host policy denies candidate qualification",
                false,
            ),
            CapacityBlocked => (
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity_blocked",
                "Candidate endpoint capacity is unavailable",
                true,
            ),
        };
        (status, Json(serde_json::json!({"api_version":"1","error":{"code":code,"message":message,"retryable":retryable,"operation_id":null,"details":{}}}))).into_response()
    }
}
impl From<ManagedConfigurationError> for ConfigurationFailure {
    fn from(value: ManagedConfigurationError) -> Self {
        use ManagedConfigurationError::*;
        match value {
            Invalid => Self::InvalidConfig,
            StaleSession | PolicyConflict => Self::ReconciliationRequired,
            IdempotencyConflict => Self::IdempotencyConflict,
            RevisionConflict => Self::RevisionConflict,
            RouteConflict => Self::RouteConflict,
            RuntimeRetained => Self::RuntimeRetained,
            CorruptStoredData | Sql(_) => Self::Internal,
        }
    }
}

/// Request data only. No command variant authorizes activation or cleanup.
pub enum ConfigurationCommand {
    Create {
        config_json: String,
    },
    Replace {
        deployment_id: String,
        expected_revision: i64,
        config_json: String,
    },
}
pub trait ConfigurationSource: Send + Sync + 'static {
    /// Persist before returning a receipt. Request cancellation must not cancel
    /// a started call; callers retain the original idempotency key for retries.
    fn accept(
        &self,
        key: &str,
        command: ConfigurationCommand,
    ) -> Result<ManagedConfigurationReceipt, ConfigurationFailure>;
}

/// Uses the worker's exact owned Store/session and lifetime process lock. It does
/// not open SQLite, begin sessions, import policy or construct runtime drivers.
pub struct SharedConfigurationSource {
    state: Arc<Mutex<OwnedCoordinatorState>>,
    trusted_host: Value,
    host_id: String,
    principal: String,
}
impl SharedConfigurationSource {
    pub fn new(
        state: Arc<Mutex<OwnedCoordinatorState>>,
        trusted_host: Value,
        principal: &str,
    ) -> Result<Self, ConfigurationFailure> {
        if !identifier(principal) || trusted_host.to_string().len() > MAX_BODY {
            return Err(ConfigurationFailure::Internal);
        }
        let host_id = trusted_host
            .get("name")
            .and_then(Value::as_str)
            .filter(|id| identifier(id))
            .ok_or(ConfigurationFailure::Internal)?
            .to_owned();
        Ok(Self {
            state,
            trusted_host,
            host_id,
            principal: principal.into(),
        })
    }
}
impl SnapshotSource for SharedConfigurationSource {
    fn snapshot(&self) -> Result<Snapshot, SnapshotUnavailable> {
        self.state
            .lock()
            .map_err(|_| SnapshotUnavailable)?
            .store()
            .snapshot()
            .map_err(|_| SnapshotUnavailable)
    }
}
impl crate::candidates::CandidateSource for SharedConfigurationSource {
    fn create_candidate(
        &self,
        key: &str,
        command_json: &str,
    ) -> Result<mllm_store::candidate_creation::CandidateCreationReceipt, ConfigurationFailure>
    {
        let state = self
            .state
            .lock()
            .map_err(|_| ConfigurationFailure::Internal)?;
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ConfigurationFailure::Internal)?
                .as_millis(),
        )
        .map_err(|_| ConfigurationFailure::Internal)?;
        // Store composes current policy only after checking historical receipts.
        // No startup policy import or fresh coordinator session is permitted here.
        state
            .store()
            .create_candidate_run(
                state.session(),
                &self.principal,
                key,
                command_json,
                &self.trusted_host,
                now,
            )
            .map_err(Into::into)
    }
}
impl EventSource for SharedConfigurationSource {
    fn events_after(&self, after: Option<&str>, limit: usize) -> Result<EventPage, EventReadError> {
        self.state
            .lock()
            .map_err(|_| EventReadError::InvalidLimit)?
            .store()
            .events_after(after, limit)
    }
}
impl ConfigurationSource for SharedConfigurationSource {
    fn accept(
        &self,
        key: &str,
        command: ConfigurationCommand,
    ) -> Result<ManagedConfigurationReceipt, ConfigurationFailure> {
        let state = self
            .state
            .lock()
            .map_err(|_| ConfigurationFailure::Internal)?;
        let store = state.store();
        let policy = store
            .resource_policy(&self.host_id)
            .map_err(|_| ConfigurationFailure::Internal)?
            .ok_or(ConfigurationFailure::ReconciliationRequired)?;
        let host = mllm_config::effective::compose_current_resource_controls(
            &self.trusted_host,
            &policy.context,
            &policy.controls,
        )
        .map_err(|_| ConfigurationFailure::ReconciliationRequired)?;
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ConfigurationFailure::Internal)?
                .as_millis(),
        )
        .map_err(|_| ConfigurationFailure::Internal)?;
        match command {
            ConfigurationCommand::Create { config_json } => store
                .create_stopped_managed_configuration(
                    state.session(),
                    &self.principal,
                    key,
                    &format!("{{\"config\":{config_json}}}"),
                    &host,
                    now,
                ),
            ConfigurationCommand::Replace {
                deployment_id,
                expected_revision,
                config_json,
            } => {
                if store
                    .get_deployment(&deployment_id)
                    .map_err(|_| ConfigurationFailure::Internal)?
                    .is_none()
                {
                    return Err(ConfigurationFailure::NotFound);
                }
                store.replace_stopped_managed_configuration(
                    state.session(),
                    &self.principal,
                    key,
                    &deployment_id,
                    &format!(
                        "{{\"config\":{config_json},\"expected_revision\":{expected_revision}}}"
                    ),
                    &host,
                    now,
                )
            }
        }
        .map_err(Into::into)
    }
}

fn identifier(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn single_header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    let mut values = request.headers().get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
    activate: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replace<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
    expected_revision: i64,
}

pub(crate) async fn accept(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match accept_inner(state, request).await {
        Ok(receipt) => (StatusCode::ACCEPTED, Json(serde_json::json!({"api_version":"1", "operation_id":receipt.operation_id,"deployment_id":receipt.deployment_id,"revision":receipt.revision.to_string(),"joined":false}))).into_response(),
        Err(error) => error.response(),
    }
}

async fn accept_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<ManagedConfigurationReceipt, ConfigurationFailure> {
    use ConfigurationFailure::*;
    let target = if request.method() == Method::PUT {
        let id = request
            .uri()
            .path()
            .strip_prefix("/management/v1/deployments/")
            .ok_or(InvalidRequest)?;
        let parsed = id.parse::<ulid::Ulid>().map_err(|_| InvalidRequest)?;
        if parsed.to_string() != id {
            return Err(InvalidRequest);
        }
        Some(id.to_owned())
    } else {
        None
    };
    let (key, body, permit) = read_command(&state, request).await?;
    let command = if let Some(deployment_id) = target {
        let input: Replace<'_> = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
        if input.expected_revision < 1 {
            return Err(InvalidRequest);
        }
        ConfigurationCommand::Replace {
            deployment_id,
            expected_revision: input.expected_revision,
            config_json: input.config.get().into(),
        }
    } else {
        let input: Create<'_> = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
        if input.activate {
            return Err(Unsupported);
        }
        ConfigurationCommand::Create {
            config_json: input.config.get().into(),
        }
    };
    let source = state.configuration.clone().ok_or(Unsupported)?;
    let receipt = accept_blocking(permit, move || source.accept(&key, command)).await?;
    if receipt.version != 1
        || receipt.revision < 1
        || receipt.generation < 1
        || receipt.resource_policy_revision < 1
        || receipt.accepted_at_ms < 0
        || ![&receipt.operation_id, &receipt.deployment_id]
            .iter()
            .all(|id| {
                id.parse::<ulid::Ulid>()
                    .is_ok_and(|parsed| parsed.to_string() == **id)
            })
    {
        return Err(Internal);
    }
    Ok(receipt)
}

/// Shared command budget, wire validation and body deadline for every mutation.
pub(crate) async fn read_command(
    state: &Arc<AppState>,
    request: Request,
) -> Result<(String, axum::body::Bytes, tokio::sync::OwnedSemaphorePermit), ConfigurationFailure> {
    use ConfigurationFailure::*;
    if request.uri().query().is_some() {
        return Err(InvalidRequest);
    }
    let key = single_header(&request, "idempotency-key")
        .filter(|key| identifier(key))
        .ok_or(InvalidRequest)?
        .to_owned();
    let content_type = single_header(&request, "content-type").ok_or(InvalidRequest)?;
    if !matches!(
        content_type.to_ascii_lowercase().as_str(),
        "application/json" | "application/json; charset=utf-8"
    ) {
        return Err(InvalidRequest);
    }
    if request.headers().contains_key("content-encoding") {
        return Err(InvalidRequest);
    }
    let permit = state
        .commands_in_flight
        .clone()
        .try_acquire_owned()
        .map_err(|_| QueueFull)?;
    let body = tokio::time::timeout(BODY_TIMEOUT, to_bytes(request.into_body(), MAX_BODY))
        .await
        .map_err(|_| DeadlineExceeded)?
        .map_err(|error| {
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                BodyTooLarge
            } else {
                InvalidRequest
            }
        })?;
    json_shape::validate(&body).map_err(|_| InvalidRequest)?;
    Ok((key, body, permit))
}

pub(crate) async fn accept_blocking<T: Send + 'static>(
    permit: tokio::sync::OwnedSemaphorePermit,
    work: impl FnOnce() -> Result<T, ConfigurationFailure> + Send + 'static,
) -> Result<T, ConfigurationFailure> {
    use ConfigurationFailure::*;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    });
    // Timeout/disconnect abandons observation, not an already-started transaction.
    tokio::time::timeout(ACCEPT_TIMEOUT, result)
        .await
        .map_err(|_| DeadlineExceeded)?
        .map_err(|_| Internal)?
}
