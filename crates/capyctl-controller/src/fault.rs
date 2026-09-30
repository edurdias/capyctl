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

use capyctl_adapters::traits::AdapterError;
use capyctl_store::lifecycle::LifecycleError;
use capyctl_store::managed_configuration::ManagedConfigurationError;
use capyctl_store::StoreError;

use crate::coordinator::{CoordinatorCommandError, CoordinatorError};

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

    /// SPEC §6.3: an operator stopped the deployment (or every instance of it),
    /// and inference must not undo that. Not a capacity refusal: nothing is
    /// queued and waiting does not help; the operator starts it again.
    #[error("stopped: {0}")]
    Stopped(String),

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

/// SPEC §6.3, §10: what a request for a deployment an operator stopped is
/// told: that the operator stopped it and how to start it again. `instances`
/// is true when the operator stopped every instance rather than the
/// deployment.
pub fn operator_stopped(deployment: &str, instances: bool) -> LifecycleFault {
    LifecycleFault::Stopped(if instances {
        format!(
            "every instance of deployment {deployment} was stopped by an operator; inference does not start it; start it with `capyctl start deployment {deployment}` (or `capyctl start instance {deployment}/<index>`)"
        )
    } else {
        format!(
            "deployment {deployment} was stopped by an operator; inference does not start it; start it with `capyctl start deployment {deployment}`"
        )
    })
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
            StoreError::FromNewerVersion { .. } => Self::Unavailable(error.to_string()),
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
            // Nothing reached the engine; the refusal is retryable, not a failure.
            AdapterError::NotAccepted(_) => Self::Unavailable(text),
            // The engine answered the request as invalid: deterministic.
            AdapterError::Rejected { .. } => Self::Blocked(text),
        }
    }
}

impl From<LifecycleError> for LifecycleFault {
    fn from(error: LifecycleError) -> Self {
        let text = error.to_string();
        match error {
            LifecycleError::NotFound => Self::NotFound(text),
            LifecycleError::RevisionConflict
            | LifecycleError::IdempotencyConflict
            | LifecycleError::Stale
            | LifecycleError::Conflict => Self::Conflict(text),
            // Refusals that decided nothing and may be re-made once the condition
            // clears.
            LifecycleError::RuntimeRetained
            | LifecycleError::HostPolicyDenied
            | LifecycleError::CapacityBlocked
            | LifecycleError::StartupRequiresEmptyHost
            | LifecycleError::HostIneligible
            | LifecycleError::QueueFull
            | LifecycleError::Disabled
            | LifecycleError::Unsupported
            | LifecycleError::Invalid
            | LifecycleError::Rejected(_) => Self::Blocked(text),
            // ADR 0014 §7 (WE3): a digest still being measured clears on its
            // own; a checkpoint known not to match needs the operator.
            LifecycleError::CheckpointDigestPending => Self::Unavailable(text),
            LifecycleError::CheckpointMismatch | LifecycleError::CheckpointUnusable(_) => {
                Self::Blocked(text)
            }
            // ADR 0008: a source still materializing clears on its own; one
            // that failed terminally needs a new revision.
            LifecycleError::ModelSourcePending => Self::Unavailable(text),
            LifecycleError::ModelSourceFailed => Self::Blocked(text),
            // The store cannot say what the current state is, so nothing about the
            // request was decided.
            LifecycleError::ReconciliationRequired
            | LifecycleError::CorruptStoredData
            | LifecycleError::Sql(_) => Self::Unavailable(text),
        }
    }
}

impl From<CoordinatorError> for LifecycleFault {
    fn from(error: CoordinatorError) -> Self {
        let text = error.to_string();
        match error {
            // The command was never admitted, so it decided nothing and may be
            // re-made once the coordinator has capacity again.
            CoordinatorError::Busy => Self::Blocked(text),
            CoordinatorError::Stopped(_) => Self::Unavailable(text),
            CoordinatorError::Service(_) => Self::Unavailable(text),
            CoordinatorError::Invalid => Self::Blocked(text),
            // W5: deferred without effect; it proceeds once capacity is back.
            CoordinatorError::Deferred(_) => Self::Unavailable(text),
            // The caller stopped waiting; the coordinator did not stop working. The
            // command may well have been accepted, so this is never a failure.
            CoordinatorError::CallerTimeout => Self::Uncertain(text),
        }
    }
}

impl From<CoordinatorCommandError> for LifecycleFault {
    fn from(error: CoordinatorCommandError) -> Self {
        match error {
            CoordinatorCommandError::Coordinator(inner) => inner.into(),
            CoordinatorCommandError::Lifecycle(inner) => inner.into(),
        }
    }
}

impl From<ManagedConfigurationError> for LifecycleFault {
    fn from(error: ManagedConfigurationError) -> Self {
        let text = error.to_string();
        match error {
            ManagedConfigurationError::Invalid | ManagedConfigurationError::Rejected(_) => {
                Self::Blocked(text)
            }
            ManagedConfigurationError::RuntimeRetained => Self::Blocked(text),
            ManagedConfigurationError::StaleSession
            | ManagedConfigurationError::IdempotencyConflict
            | ManagedConfigurationError::RevisionConflict
            | ManagedConfigurationError::RouteConflict
            | ManagedConfigurationError::PolicyConflict => Self::Conflict(text),
            // The stored configuration cannot be read, so nothing about this request
            // was decided.
            ManagedConfigurationError::CorruptStoredData | ManagedConfigurationError::Sql(_) => {
                Self::Unavailable(text)
            }
        }
    }
}

#[cfg(test)]
mod tests;
