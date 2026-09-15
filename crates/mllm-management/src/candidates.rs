//! Candidate run creation only. No client evidence or runtime control capability.
use crate::{
    configuration::{accept_blocking, read_command, ConfigurationFailure},
    AppState,
};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use mllm_store::candidate_creation::{CandidateCreationError, CandidateCreationReceipt};
use std::sync::Arc;

pub trait CandidateSource: Send + Sync + 'static {
    /// Persist exact principal/host/run authority before returning a receipt.
    /// This call never launches a runtime or authorizes an execution callback.
    fn create_candidate(
        &self,
        key: &str,
        command_json: &str,
    ) -> Result<CandidateCreationReceipt, ConfigurationFailure>;
}

impl From<CandidateCreationError> for ConfigurationFailure {
    fn from(error: CandidateCreationError) -> Self {
        use CandidateCreationError::*;
        match error {
            InvalidCommand => Self::InvalidRequest,
            StaleSession => Self::ReconciliationRequired,
            IdempotencyConflict => Self::IdempotencyConflict,
            RevisionConflict => Self::RevisionConflict,
            QualificationDenied => Self::HostPolicyDenied,
            EndpointUnavailable => Self::CapacityBlocked,
            CorruptStoredData | Sql(_) => Self::Internal,
        }
    }
}

pub(crate) async fn accept(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match accept_inner(state, request).await {
        Ok(receipt) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "api_version":"1", "operation_id":receipt.operation_id(),
                "deployment_id":receipt.deployment_id(), "qualification_run_id":receipt.run_id(),
                "revision":receipt.revision().to_string(), "joined":false
            })),
        )
            .into_response(),
        Err(error) => error.response(),
    }
}

async fn accept_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<CandidateCreationReceipt, ConfigurationFailure> {
    let (key, body, permit) = read_command(&state, request).await?;
    let command = std::str::from_utf8(&body)
        .map_err(|_| ConfigurationFailure::InvalidRequest)?
        .to_owned();
    let source = state
        .candidates
        .clone()
        .ok_or(ConfigurationFailure::Unsupported)?;
    // Preserve reviewed JSON bytes. The Store's strict command decoder and
    // reviewed-manifest validator reject unknown fields and client evidence.
    accept_blocking(permit, move || source.create_candidate(&key, &command)).await
}
