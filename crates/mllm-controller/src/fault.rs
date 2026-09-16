//! The lifecycle port's own failure vocabulary.
//!
//! The port previously returned the store's and the controller's error types, which
//! only worked while `Controller` was its single implementation. A second authority
//! exposes the leak: the coordinator's failures are not store failures, and a
//! poisoned ownership mutex has no honest `StoreError` variant at all.
//!
//! The distinction this exists to protect is uncertain versus failed. An effect that
//! may or may not have reached an engine is not a failure, and collapsing it into one
//! invites the caller to retry a non-idempotent control or to release accounting that
//! is still owed. `Uncertain` is therefore a variant of its own, and every conversion
//! into this type is an exhaustive match: adding a variant upstream is a compile
//! error here rather than a silent reclassification into the nearest neighbour.

use mllm_adapters::traits::AdapterError;
use mllm_store::StoreError;

use crate::operations::ControllerError;

#[derive(Debug, thiserror::Error)]
pub enum LifecycleFault {
    /// The named deployment or route does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Admission was refused: capacity, policy, suspension, or an illegal
    /// transition. The request did not take effect and may be re-made.
    #[error("blocked: {0}")]
    Blocked(String),

    /// A revision, generation or idempotency precondition did not hold. The caller
    /// is acting on a view of the world that has moved.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The effect may or may not have occurred, and no observation settles it.
    ///
    /// Never retry on this. Accounting stays charged and ownership stays held until
    /// evidence resolves it, which is the rule the whole lifecycle is built around.
    #[error("uncertain, reconciliation required: {0}")]
    Uncertain(String),

    /// A definite failure that did not take effect.
    #[error("failed: {0}")]
    Failed(String),

    /// The authority itself could not answer — its lock is poisoned, its worker has
    /// stopped, or its store is unreadable. Distinct from a failed request, because
    /// nothing about the request was decided.
    #[error("authority unavailable: {0}")]
    Unavailable(String),
}

impl From<StoreError> for LifecycleFault {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Conflict => Self::Conflict(error.to_string()),
            StoreError::IdempotencyConflict => Self::Conflict(error.to_string()),
            StoreError::StaleGeneration => Self::Conflict(error.to_string()),
            // A storage or I/O failure decided nothing about the request, so it is
            // the authority being unavailable rather than the request failing.
            StoreError::Sql(_) => Self::Unavailable(error.to_string()),
            StoreError::Io(_) => Self::Unavailable(error.to_string()),
        }
    }
}

impl From<ControllerError> for LifecycleFault {
    fn from(error: ControllerError) -> Self {
        match error {
            ControllerError::UnknownDeployment(ref id) => Self::NotFound(id.clone()),
            ControllerError::Blocked(_) => Self::Blocked(error.to_string()),
            ControllerError::IllegalTransition { .. } => Self::Blocked(error.to_string()),
            ControllerError::NoSafeEstimate(_) => Self::Blocked(error.to_string()),
            ControllerError::StaleGeneration(_) => Self::Conflict(error.to_string()),
            // A timeout waiting for a terminal state says nothing about whether the
            // operation took effect; it is still running. Reporting it as failure
            // would invite a caller to act as though nothing happened.
            ControllerError::Timeout => Self::Uncertain(error.to_string()),
            ControllerError::OperationFailed { .. } => Self::Failed(error.to_string()),
            ControllerError::Store(inner) => inner.into(),
        }
    }
}

impl From<AdapterError> for LifecycleFault {
    fn from(error: AdapterError) -> Self {
        let text = format!("{error:?}");
        match error {
            // The adapter's own uncertainty is the same condition, and must survive
            // the crossing rather than flattening into a failure.
            AdapterError::Uncertain(message) => Self::Uncertain(message),
            AdapterError::PolicyDenied => Self::Blocked(text),
            AdapterError::UnsupportedCapability => Self::Blocked(text),
            AdapterError::UnsupportedCombination => Self::Blocked(text),
            // A crash is a definite outcome about the engine, not about whether a
            // request was accepted, so it is a failure rather than uncertainty.
            AdapterError::Crash(_) => Self::Failed(text),
        }
    }
}

#[cfg(test)]
mod tests;
