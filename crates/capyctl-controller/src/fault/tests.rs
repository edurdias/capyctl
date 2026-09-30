use super::*;
use capyctl_domain::LifecycleState;

/// The distinction the type exists to protect. A timeout waiting for a terminal
/// state does not mean the operation did not happen — it means nobody knows yet.
/// Classifying it as a failure invites a caller to act as though nothing occurred,
/// which for a non-idempotent engine control is the exact mistake the lifecycle is
/// built to prevent.
#[test]
fn a_timeout_is_uncertain_not_failed() {
    assert!(matches!(
        LifecycleFault::from(ControllerError::Timeout),
        LifecycleFault::Uncertain(_)
    ));
}

/// A definite failure carries its code and stays a failure.
#[test]
fn a_reported_operation_failure_stays_failed() {
    let fault = LifecycleFault::from(ControllerError::OperationFailed {
        op: "park".into(),
        code: "denied".into(),
    });
    match fault {
        LifecycleFault::Failed(message) => {
            assert!(message.contains("park"), "{message}");
            assert!(message.contains("denied"), "{message}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_missing_deployment_is_not_found_and_keeps_its_identity() {
    match LifecycleFault::from(ControllerError::UnknownDeployment("dep-1".into())) {
        LifecycleFault::NotFound(id) => assert_eq!(id, "dep-1"),
        other => panic!("expected not found, got {other:?}"),
    }
}

/// Refusals that decided nothing and may be re-made are blocked, not failed.
#[test]
fn refusals_that_took_no_effect_are_blocked() {
    for error in [
        ControllerError::IllegalTransition {
            from: LifecycleState::Ready,
            to: LifecycleState::Ready,
        },
        ControllerError::NoSafeEstimate("no observation".into()),
    ] {
        assert!(
            matches!(LifecycleFault::from(error), LifecycleFault::Blocked(_)),
            "a refusal that took no effect must be blocked"
        );
    }
}

/// Precondition failures are conflicts: the caller is acting on a stale view.
#[test]
fn precondition_failures_are_conflicts() {
    assert!(matches!(
        LifecycleFault::from(ControllerError::StaleGeneration("dep-1".into())),
        LifecycleFault::Conflict(_)
    ));
    for error in [
        StoreError::Conflict,
        StoreError::IdempotencyConflict,
        StoreError::StaleGeneration,
    ] {
        assert!(matches!(
            LifecycleFault::from(error),
            LifecycleFault::Conflict(_)
        ));
    }
}

/// A storage failure decided nothing about the request, so the authority is
/// unavailable rather than the request having failed. The difference matters: a
/// failed request may be re-made, an unavailable authority must not be assumed to
/// have done nothing.
#[test]
fn storage_failures_make_the_authority_unavailable() {
    let sql = StoreError::Sql(rusqlite::Error::QueryReturnedNoRows);
    assert!(matches!(
        LifecycleFault::from(sql),
        LifecycleFault::Unavailable(_)
    ));
    let io = StoreError::Io(std::io::Error::other("disk gone"));
    assert!(matches!(
        LifecycleFault::from(io),
        LifecycleFault::Unavailable(_)
    ));
}

/// A store error reaching the port through the controller keeps its classification
/// rather than flattening into a generic controller failure.
#[test]
fn a_nested_store_error_keeps_its_classification() {
    assert!(matches!(
        LifecycleFault::from(ControllerError::Store(StoreError::StaleGeneration)),
        LifecycleFault::Conflict(_)
    ));
    assert!(matches!(
        LifecycleFault::from(ControllerError::Store(StoreError::Sql(
            rusqlite::Error::QueryReturnedNoRows
        ))),
        LifecycleFault::Unavailable(_)
    ));
}

/// The adapter's uncertainty is the same condition and must survive the crossing.
/// Flattening it here would lose the distinction at exactly the boundary where the
/// engine is the only thing that knows what happened.
#[test]
fn adapter_uncertainty_survives_the_crossing() {
    use capyctl_adapters::traits::AdapterError;
    match LifecycleFault::from(AdapterError::Uncertain("sleep may have landed".into())) {
        LifecycleFault::Uncertain(message) => assert_eq!(message, "sleep may have landed"),
        other => panic!("expected uncertainty, got {other:?}"),
    }
}

#[test]
fn adapter_refusals_are_blocked_and_a_crash_is_a_failure() {
    use capyctl_adapters::traits::AdapterError;
    for error in [
        AdapterError::PolicyDenied,
        AdapterError::UnsupportedCapability,
        AdapterError::UnsupportedCombination,
    ] {
        assert!(matches!(
            LifecycleFault::from(error),
            LifecycleFault::Blocked(_)
        ));
    }
    assert!(matches!(
        LifecycleFault::from(AdapterError::Crash(capyctl_adapters::Phase::Startup)),
        LifecycleFault::Failed(_)
    ));
}

/// A caller that stopped waiting has not established that the coordinator stopped
/// working. The command may already have been accepted, so this is uncertainty
/// rather than failure — the same reasoning as a controller timeout.
#[test]
fn a_caller_timeout_is_uncertain() {
    use crate::coordinator::CoordinatorError;
    assert!(matches!(
        LifecycleFault::from(CoordinatorError::CallerTimeout),
        LifecycleFault::Uncertain(_)
    ));
}

/// A command refused before admission decided nothing and may be re-made; a stopped
/// or failed coordinator is the authority being unavailable.
#[test]
fn coordinator_refusals_and_outages_are_distinguished() {
    use crate::coordinator::CoordinatorError;
    assert!(matches!(
        LifecycleFault::from(CoordinatorError::Busy),
        LifecycleFault::Blocked(_)
    ));
    assert!(matches!(
        LifecycleFault::from(CoordinatorError::Stopped("draining".into())),
        LifecycleFault::Unavailable(_)
    ));
    assert!(matches!(
        LifecycleFault::from(CoordinatorError::Service("poisoned".into())),
        LifecycleFault::Unavailable(_)
    ));
}

/// Corrupt or unreadable lifecycle state says nothing about the request.
#[test]
fn unreadable_lifecycle_state_is_unavailable_not_blocked() {
    use capyctl_store::lifecycle::LifecycleError;
    for error in [
        LifecycleError::CorruptStoredData,
        LifecycleError::ReconciliationRequired,
    ] {
        assert!(matches!(
            LifecycleFault::from(error),
            LifecycleFault::Unavailable(_)
        ));
    }
    assert!(matches!(
        LifecycleFault::from(LifecycleError::NotFound),
        LifecycleFault::NotFound(_)
    ));
    assert!(matches!(
        LifecycleFault::from(LifecycleError::RevisionConflict),
        LifecycleFault::Conflict(_)
    ));
}
