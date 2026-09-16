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
