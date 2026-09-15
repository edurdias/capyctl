//! Authenticated command submission through the existing application-owned worker.
use crate::{
    candidates::CandidateSource,
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
    candidate_creation::CandidateCreationReceipt,
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
    fn candidate_inference(
        &self,
        run: &str,
        key: &str,
        expected_revision: i64,
        request: &str,
    ) -> Result<
        mllm_store::candidate_creation::progression::CandidateInferenceReceipt,
        ConfigurationFailure,
    >;
    fn initialize_candidate(
        &self,
        run: &str,
        key: &str,
        expected_revision: i64,
        deadline_ms: i64,
    ) -> Result<
        mllm_store::candidate_creation::progression::CandidateActionReceipt,
        ConfigurationFailure,
    >;
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
    fn candidate_inference(
        &self,
        run: &str,
        key: &str,
        expected_revision: i64,
        request: &str,
    ) -> Result<
        mllm_store::candidate_creation::progression::CandidateInferenceReceipt,
        ConfigurationFailure,
    > {
        self.commands
            .candidate_inference(
                self.configuration.principal(),
                run,
                expected_revision,
                key,
                request,
            )
            .map_err(command_failure)
    }
    fn initialize_candidate(
        &self,
        run: &str,
        key: &str,
        expected_revision: i64,
        deadline_ms: i64,
    ) -> Result<
        mllm_store::candidate_creation::progression::CandidateActionReceipt,
        ConfigurationFailure,
    > {
        self.commands
            .initialize_candidate(
                self.configuration.principal(),
                run,
                expected_revision,
                key,
                deadline_ms,
            )
            .map_err(command_failure)
    }
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCommand {
    expected_revision: i64,
    action: CandidateAction,
    deadline_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateInferenceCommand {
    expected_revision: i64,
    request: serde_json::Value,
}
pub(crate) async fn accept_candidate_inference(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Response {
    match accept_candidate_inference_inner(state,request).await {
        Ok(r)=>(StatusCode::ACCEPTED,Json(serde_json::json!({"api_version":"1","operation_id":r.operation_id,"deployment_id":r.deployment_id,"qualification_run_id":r.run_id,"joined":false,"revision":r.revision.to_string()}))).into_response(),
        Err(error)=>error.response(),
    }
}
async fn accept_candidate_inference_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<
    mllm_store::candidate_creation::progression::CandidateInferenceReceipt,
    ConfigurationFailure,
> {
    use ConfigurationFailure::*;
    let run = request
        .uri()
        .path()
        .strip_prefix("/management/v1/qualification-runs/")
        .and_then(|s| s.strip_suffix("/inference"))
        .ok_or(InvalidRequest)?;
    if !run
        .parse::<ulid::Ulid>()
        .is_ok_and(|id| id.to_string() == run)
    {
        return Err(InvalidRequest);
    }
    let run = run.to_owned();
    let (key, body, permit) = configuration::read_command(&state, request).await?;
    let command: CandidateInferenceCommand =
        serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
    if command.expected_revision < 1 || !command.request.is_object() {
        return Err(InvalidRequest);
    }
    let source = state.actions.clone().ok_or(Unsupported)?;
    let result = configuration::accept_blocking(permit, move || {
        source.candidate_inference(
            &run,
            &key,
            command.expected_revision,
            &command.request.to_string(),
        )
    })
    .await?;
    if result.revision < 1
        || ![&result.operation_id, &result.deployment_id, &result.run_id]
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
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CandidateAction {
    Initialize,
    Park,
    Restore,
    Finish,
    Abort,
    Cleanup,
}

pub(crate) async fn accept_candidate(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Response {
    match accept_candidate_inner(state, request).await {
        Ok((run, receipt)) => (StatusCode::ACCEPTED, Json(serde_json::json!({"api_version":"1","qualification_run_id":run,"operation_id":receipt.operation_id(),"step_id":receipt.step_id(),"deployment_id":receipt.deployment_id(),"revision":receipt.revision().to_string(),"joined":false}))).into_response(),
        Err(error) => error.response(),
    }
}
async fn accept_candidate_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<
    (
        String,
        mllm_store::candidate_creation::progression::CandidateActionReceipt,
    ),
    ConfigurationFailure,
> {
    use ConfigurationFailure::*;
    let run = request
        .uri()
        .path()
        .strip_prefix("/management/v1/qualification-runs/")
        .and_then(|s| s.strip_suffix("/actions"))
        .ok_or(InvalidRequest)?;
    if !run
        .parse::<ulid::Ulid>()
        .is_ok_and(|id| id.to_string() == run)
    {
        return Err(InvalidRequest);
    }
    let run = run.to_owned();
    let (key, body, permit) = configuration::read_command(&state, request).await?;
    let command: CandidateCommand = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
    if command.expected_revision < 1 || command.deadline_ms < 1 {
        return Err(InvalidRequest);
    }
    if !matches!(command.action, CandidateAction::Initialize) {
        return Err(Unsupported);
    }
    let source = state.actions.clone().ok_or(Unsupported)?;
    let target = run.clone();
    let receipt = configuration::accept_blocking(permit, move || {
        source.initialize_candidate(
            &target,
            &key,
            command.expected_revision,
            command.deadline_ms,
        )
    })
    .await?;
    if receipt.revision() < 1
        || ![
            receipt.operation_id(),
            receipt.step_id(),
            receipt.deployment_id(),
        ]
        .iter()
        .all(|id| {
            id.parse::<ulid::Ulid>()
                .is_ok_and(|parsed| parsed.to_string() == *id)
        })
    {
        return Err(Internal);
    }
    Ok((run, receipt))
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
impl CandidateSource for OwnedActionSource {
    fn create_candidate(
        &self,
        key: &str,
        command_json: &str,
    ) -> Result<CandidateCreationReceipt, ConfigurationFailure> {
        self.configuration.create_candidate(key, command_json)
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
