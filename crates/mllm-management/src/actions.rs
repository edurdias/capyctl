//! Authenticated command submission through the existing application-owned worker.
use crate::{
    configuration::{
        self, ConfigurationCommand, ConfigurationFailure, ConfigurationSource,
        SharedConfigurationSource,
    },
    events::EventSource,
    AppState, SnapshotSource, SnapshotUnavailable,
};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use mllm_controller::coordinator::{
    CoordinatorCommandError, CoordinatorCommands, CoordinatorError,
};
use mllm_store::{
    events::{EventPage, EventReadError},
    lifecycle::LifecycleError,
    managed_configuration::ManagedConfigurationReceipt,
    snapshot::Snapshot,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionCommand {
    pub expected_revision: i64,
    pub action: Action,
    pub deadline_ms: i64,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Stop,
    Park,
    Suspend,
    Resume,
    Undeploy,
}

pub struct ActionReceipt {
    pub operation_id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub joined: bool,
}
pub trait ActionSource: Send + Sync + 'static {
    fn accept_action(
        &self,
        deployment: &str,
        key: &str,
        command: ActionCommand,
    ) -> Result<ActionReceipt, ConfigurationFailure>;
}

/// Construction rejects mismatched state before a router receives authority.
/// This handle does not own the worker shutdown task or create any new session.
pub struct OwnedActionSource {
    configuration: Arc<SharedConfigurationSource>,
    commands: CoordinatorCommands,
}
impl OwnedActionSource {
    pub fn new(
        configuration: Arc<SharedConfigurationSource>,
        commands: CoordinatorCommands,
    ) -> Result<Self, ConfigurationFailure> {
        if !commands.shares_state(configuration.owned_state()) {
            return Err(ConfigurationFailure::Internal);
        }
        Ok(Self {
            configuration,
            commands,
        })
    }
}
impl ActionSource for OwnedActionSource {
    fn accept_action(
        &self,
        deployment: &str,
        key: &str,
        command: ActionCommand,
    ) -> Result<ActionReceipt, ConfigurationFailure> {
        let principal = self.configuration.principal();
        match command.action {
            Action::Start => {
                let receipt = self
                    .commands
                    .start(
                        principal,
                        deployment,
                        command.expected_revision,
                        key,
                        command.deadline_ms,
                    )
                    .map_err(command_failure)?;
                Ok(ActionReceipt {
                    operation_id: receipt.operation_id().into(),
                    deployment_id: receipt.deployment_id().into(),
                    revision: receipt.revision(),
                    joined: receipt.joined(),
                })
            }
            Action::Stop => {
                let receipt = self
                    .commands
                    .stop(
                        principal,
                        deployment,
                        command.expected_revision,
                        key,
                        command.deadline_ms,
                    )
                    .map_err(command_failure)?;
                Ok(ActionReceipt {
                    operation_id: receipt.operation_id().into(),
                    deployment_id: deployment.into(),
                    revision: receipt.revision(),
                    joined: false,
                })
            }
            _ => Err(ConfigurationFailure::Unsupported),
        }
    }
}

fn command_failure(error: CoordinatorCommandError) -> ConfigurationFailure {
    use ConfigurationFailure as F;
    match error {
        CoordinatorCommandError::Coordinator(CoordinatorError::Busy) => F::QueueFull,
        CoordinatorCommandError::Coordinator(CoordinatorError::Stopped(_)) => {
            F::ReconciliationRequired
        }
        CoordinatorCommandError::Coordinator(CoordinatorError::CallerTimeout) => {
            F::DeadlineExceeded
        }
        CoordinatorCommandError::Coordinator(_) => F::Internal,
        CoordinatorCommandError::Lifecycle(error) => match error {
            LifecycleError::Invalid => F::InvalidRequest,
            LifecycleError::NotFound => F::NotFound,
            LifecycleError::RevisionConflict => F::RevisionConflict,
            LifecycleError::IdempotencyConflict => F::IdempotencyConflict,
            LifecycleError::Conflict => F::LifecycleConflict,
            LifecycleError::RuntimeRetained => F::RuntimeRetained,
            LifecycleError::Unsupported => F::Unsupported,
            LifecycleError::Disabled | LifecycleError::HostPolicyDenied => F::HostPolicyDenied,
            LifecycleError::CapacityBlocked => F::CapacityBlocked,
            LifecycleError::QueueFull => F::QueueFull,
            LifecycleError::Stale | LifecycleError::ReconciliationRequired => {
                F::ReconciliationRequired
            }
            LifecycleError::Sql(_)
            | LifecycleError::CorruptStoredData
            | LifecycleError::Rejected(_) => F::Internal,
        },
    }
}
impl SnapshotSource for OwnedActionSource {
    fn snapshot(&self) -> Result<Snapshot, SnapshotUnavailable> {
        self.configuration.snapshot()
    }
}
impl EventSource for OwnedActionSource {
    fn events_after(&self, after: Option<&str>, limit: usize) -> Result<EventPage, EventReadError> {
        self.configuration.events_after(after, limit)
    }
}
impl ConfigurationSource for OwnedActionSource {
    fn accept(
        &self,
        key: &str,
        command: ConfigurationCommand,
    ) -> Result<ManagedConfigurationReceipt, ConfigurationFailure> {
        self.configuration.accept(key, command)
    }
}

pub(crate) async fn accept(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match accept_inner(state, request).await {
        Ok(r) => (StatusCode::ACCEPTED, Json(serde_json::json!({"api_version":"1","operation_id":r.operation_id,"deployment_id":r.deployment_id,"joined":r.joined,"revision":r.revision.to_string()}))).into_response(),
        Err(error) => error.response(),
    }
}
async fn accept_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<ActionReceipt, ConfigurationFailure> {
    use ConfigurationFailure::*;
    let id = request
        .uri()
        .path()
        .strip_prefix("/management/v1/deployments/")
        .and_then(|s| s.strip_suffix("/actions"))
        .ok_or(InvalidRequest)?;
    if !id
        .parse::<ulid::Ulid>()
        .is_ok_and(|parsed| parsed.to_string() == id)
    {
        return Err(InvalidRequest);
    }
    let id = id.to_owned();
    let (key, body, permit) = configuration::read_command(&state, request).await?;
    let command: ActionCommand = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
    if command.expected_revision < 1 || command.deadline_ms < 1 {
        return Err(InvalidRequest);
    }
    if !matches!(command.action, Action::Start | Action::Stop) {
        return Err(Unsupported);
    }
    let source = state.actions.clone().ok_or(Unsupported)?;
    let result =
        configuration::accept_blocking(permit, move || source.accept_action(&id, &key, command))
            .await?;
    if result.revision < 1
        || ![&result.operation_id, &result.deployment_id]
            .iter()
            .all(|id| {
                id.parse::<ulid::Ulid>()
                    .is_ok_and(|parsed| parsed.to_string() == **id)
            })
    {
        return Err(Internal);
    }
    Ok(result)
}
