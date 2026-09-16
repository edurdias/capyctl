use super::*;

/// Simultaneous arrivals that saw the same state must collapse to one activation
/// (T15). Deriving the key from the deployment plus the revision and generation it
/// was observed at makes that the store's decision rather than the router's.
#[test]
fn arrivals_that_saw_the_same_state_share_an_activation_key() {
    let a = CoordinatorLifecycle::activation_key("dep-1", 3, 7);
    let b = CoordinatorLifecycle::activation_key("dep-1", 3, 7);
    assert_eq!(a, b, "concurrent arrivals must join one activation");
}

/// An arrival that saw a later generation is asking about a different runtime, so it
/// must not join an operation raised for the previous one.
#[test]
fn a_later_generation_does_not_join_an_earlier_activation() {
    let earlier = CoordinatorLifecycle::activation_key("dep-1", 3, 7);
    let later = CoordinatorLifecycle::activation_key("dep-1", 3, 8);
    let revised = CoordinatorLifecycle::activation_key("dep-1", 4, 7);
    assert_ne!(earlier, later);
    assert_ne!(earlier, revised);
}

#[test]
fn different_deployments_never_share_a_key() {
    assert_ne!(
        CoordinatorLifecycle::activation_key("dep-1", 1, 1),
        CoordinatorLifecycle::activation_key("dep-2", 1, 1)
    );
}

/// A refusal must say what is missing. A bare error at a call site is how a caller
/// ends up substituting a neighbouring operation for the one it wanted.
#[test]
fn refusals_name_the_missing_capability() {
    match CoordinatorLifecycle::unsupported("observe engine work") {
        LifecycleFault::Blocked(message) => {
            assert!(message.contains("observe engine work"), "{message}");
            assert!(message.contains("refusing"), "{message}");
        }
        other => panic!("a missing capability is blocked, not {other:?}"),
    }
}

mod wait_terminal {
    use super::*;
    use mllm_domain::LifecycleState;
    use mllm_store::deployments::OpState;

    fn classify(
        state: OpState,
        error_code: Option<&str>,
        observed: Option<LifecycleState>,
        expired: bool,
    ) -> Option<Result<LifecycleState, LifecycleFault>> {
        CoordinatorLifecycle::classify(
            "op-1",
            state,
            error_code.map(str::to_string),
            observed,
            expired,
        )
    }

    /// Exhausting the caller's wait is uncertainty, never failure. The operation is
    /// still running and the coordinator reconciles it; reporting failure would tell
    /// a caller the activation did not happen while it still might.
    #[test]
    fn an_expired_wait_on_a_running_operation_is_uncertain() {
        for state in [OpState::Pending, OpState::Running] {
            match classify(state, None, Some(LifecycleState::Starting), true) {
                Some(Err(LifecycleFault::Uncertain(message))) => {
                    assert!(message.contains("still"), "{message}");
                }
                other => panic!("expected uncertainty for {state:?}, got {other:?}"),
            }
        }
    }

    /// Before the wait expires, a running operation is not an outcome at all: the
    /// caller keeps waiting rather than receiving a verdict.
    #[test]
    fn a_running_operation_yields_no_outcome_yet() {
        for state in [OpState::Pending, OpState::Running] {
            assert!(
                classify(state, None, Some(LifecycleState::Starting), false).is_none(),
                "{state:?} must keep waiting"
            );
        }
    }

    #[test]
    fn a_succeeded_operation_reports_the_observed_state() {
        match classify(OpState::Succeeded, None, Some(LifecycleState::Ready), false) {
            Some(Ok(state)) => assert_eq!(state, LifecycleState::Ready),
            other => panic!("expected the observed state, got {other:?}"),
        }
    }

    /// Success with no readable observed state is a record that cannot be read, not
    /// a state to report.
    #[test]
    fn success_without_an_observed_state_is_unavailable() {
        assert!(matches!(
            classify(OpState::Succeeded, None, None, false),
            Some(Err(LifecycleFault::Unavailable(_)))
        ));
    }

    #[test]
    fn a_failed_operation_carries_its_code() {
        match classify(OpState::Failed, Some("denied"), None, false) {
            Some(Err(LifecycleFault::Failed(message))) => {
                assert!(message.contains("denied"), "{message}")
            }
            other => panic!("expected a failure carrying its code, got {other:?}"),
        }
        match classify(OpState::Failed, None, None, false) {
            Some(Err(LifecycleFault::Failed(message))) => {
                assert!(message.contains("unknown"), "{message}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// A terminal outcome does not depend on the caller's patience.
    #[test]
    fn expiry_does_not_change_a_terminal_outcome() {
        assert!(matches!(
            classify(OpState::Succeeded, None, Some(LifecycleState::Ready), true),
            Some(Ok(LifecycleState::Ready))
        ));
        assert!(matches!(
            classify(OpState::Failed, Some("denied"), None, true),
            Some(Err(LifecycleFault::Failed(_)))
        ));
    }
}
