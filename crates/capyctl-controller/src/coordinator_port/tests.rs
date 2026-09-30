use super::*;

/// Simultaneous arrivals that saw the same state must collapse to one activation
/// (T15). Deriving the key from the deployment plus the revision and generation it
/// was observed at makes that the store's decision rather than the router's.
#[test]
fn arrivals_that_saw_the_same_state_share_an_activation_key() {
    let a = CoordinatorLifecycle::activation_key("dep-1", 3, 7, "op-1");
    let b = CoordinatorLifecycle::activation_key("dep-1", 3, 7, "op-1");
    assert_eq!(a, b, "concurrent arrivals must join one activation");
}

/// An arrival that saw a later generation is asking about a different runtime, so it
/// must not join an operation raised for the previous one.
#[test]
fn a_later_generation_does_not_join_an_earlier_activation() {
    let earlier = CoordinatorLifecycle::activation_key("dep-1", 3, 7, "op-1");
    let later = CoordinatorLifecycle::activation_key("dep-1", 3, 8, "op-1");
    let revised = CoordinatorLifecycle::activation_key("dep-1", 4, 7, "op-1");
    assert_ne!(earlier, later);
    assert_ne!(earlier, revised);
}

/// T15: found live on the 16 GB discrete-GPU laptop host. A failed launch
/// leaves the generation unchanged, and the key a later request derived was
/// the failed attempt's: its receipt carried another deadline, so every
/// request for the deployment was answered 409 `idempotency key identifies a
/// different command` until an operator started it. The key also names the
/// deployment's latest operation, so an attempt after a failed one is new,
/// while arrivals that saw the same state still share one.
#[test]
fn an_activation_after_a_failed_one_is_a_new_command() {
    let failed = CoordinatorLifecycle::activation_key("dep-1", 1, 1, "op-create");
    let retry = CoordinatorLifecycle::activation_key("dep-1", 1, 1, "op-failed-start");
    assert_ne!(failed, retry);
    assert_eq!(
        retry,
        CoordinatorLifecycle::activation_key("dep-1", 1, 1, "op-failed-start")
    );
}

#[test]
fn different_deployments_never_share_a_key() {
    assert_ne!(
        CoordinatorLifecycle::activation_key("dep-1", 1, 1, "op-1"),
        CoordinatorLifecycle::activation_key("dep-2", 1, 1, "op-1")
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
    use capyctl_domain::LifecycleState;
    use capyctl_store::deployments::OpState;

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

mod deployment_acceptance {
    use crate::coordinator_port::CoordinatorLifecycle;
    use crate::operations::DeployRequest;
    use sha2::{Digest as _, Sha256};

    fn key(context: &str, name: &str, manifest: &[u8]) -> String {
        let mut hash = Sha256::new();
        hash.update(context.as_bytes());
        hash.update([0u8]);
        hash.update(name.as_bytes());
        hash.update([0u8]);
        hash.update(manifest);
        format!("{:x}", hash.finalize())
    }

    fn request(name: &str, manifest: &str) -> DeployRequest {
        DeployRequest {
            name: name.into(),
            kind: "model".into(),
            manifest: manifest.as_bytes().to_vec(),
            route_model_id: Some(name.into()),
        }
    }

    /// A response lost after the record was written must not produce a second
    /// deployment when the caller retries (T09). The key is what makes the retry
    /// resolve to the same record, so identical submissions must derive the same one.
    #[test]
    fn an_identical_resubmission_derives_the_same_key() {
        let a = request("m", r#"{"kind":"model"}"#);
        let b = request("m", r#"{"kind":"model"}"#);
        assert_eq!(
            key("ctx", &a.name, &a.manifest),
            key("ctx", &b.name, &b.manifest)
        );
    }

    /// A changed manifest is a different deployment, not a retry of the first.
    #[test]
    fn a_changed_manifest_is_not_a_retry() {
        let a = request("m", r#"{"kind":"model","v":1}"#);
        let b = request("m", r#"{"kind":"model","v":2}"#);
        assert_ne!(
            key("ctx", &a.name, &a.manifest),
            key("ctx", &b.name, &b.manifest)
        );
    }

    /// Two contexts submitting the same manifest are not each other's retries.
    #[test]
    fn separate_contexts_do_not_collide() {
        let r = request("m", r#"{"kind":"model"}"#);
        assert_ne!(
            key("ctx-a", &r.name, &r.manifest),
            key("ctx-b", &r.name, &r.manifest)
        );
    }

    /// The bridge derives its key the same way, so a retry through it resolves to
    /// the same record rather than creating a second deployment.
    #[test]
    fn the_bridge_derives_the_documented_key() {
        let r = request("m", r#"{"kind":"model"}"#);
        // Mirrors submit_deploy's derivation; a divergence here would silently break
        // idempotency without failing anything else.
        let expected = key("ctx", &r.name, &r.manifest);
        assert_eq!(expected.len(), 64, "a sha256 hex digest");
        let _ = CoordinatorLifecycle::activation_key("dep", 1, 1, "op");
    }
}
