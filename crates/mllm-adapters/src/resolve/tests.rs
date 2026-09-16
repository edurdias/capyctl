use super::*;
use crate::traits::{MemberRef, RuntimeAction, RuntimeCommand};
use mllm_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};

fn fake() -> AdapterSpec {
    AdapterSpec::Fake {
        clock: Arc::new(|| Ok(1_700_000_000_000)),
    }
}

fn vllm() -> AdapterSpec {
    AdapterSpec::Vllm {
        endpoint: "http://127.0.0.1:8000".parse().unwrap(),
        api_key: None,
        fingerprint: "fp".into(),
        policy: ParkPolicy::Denied,
        model_id: "m".into(),
    }
}

#[test]
fn every_family_maps_to_its_own_spec() {
    assert_eq!(vllm().engine(), Engine::Vllm);
    assert_eq!(fake().engine(), Engine::Fake);
}

/// A profile's identity — build fingerprint, reserved-flag policy, qualification
/// evidence — is only meaningful for the engine it names. Resolving a spec for a
/// different family would attach that identity to the wrong control contract.
#[test]
fn a_spec_for_another_family_is_rejected() {
    for declared in [Engine::Sglang, Engine::Fake] {
        assert!(
            matches!(resolve(declared, vllm()), Err(RuntimeError::Unsupported)),
            "a vLLM spec must not resolve as {declared:?}"
        );
    }
    for declared in [Engine::Vllm, Engine::Sglang] {
        assert!(matches!(
            resolve(declared, fake()),
            Err(RuntimeError::Unsupported)
        ));
    }
}

#[test]
fn a_matching_family_resolves() {
    assert!(resolve(Engine::Vllm, vllm()).is_ok());
    assert!(resolve(Engine::Fake, fake()).is_ok());
}

/// vLLM still refuses the persisted control path, which the default trait method
/// provides. Resolution must not appear to grant a capability the engine lacks.
#[tokio::test]
async fn resolution_does_not_invent_a_persisted_control_path() {
    let adapter = resolve(Engine::Vllm, vllm()).unwrap();
    let command = RuntimeCommand {
        action: RuntimeAction::Park,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d".into(),
                revision: 1,
                generation: 1,
                operation_id: "o".into(),
                step_id: "s".into(),
                qualification_id: "q".into(),
            },
            binding_id: "b".into(),
            incarnation: "i".into(),
            issued_at_ms: 0,
            deadline_ms: i64::MAX,
            identities: ExecutionIdentities::Retained(Vec::new()),
            completion_target: None,
            grant_id: None,
            launch_settings: None,
        },
    };
    assert!(matches!(
        adapter.execute_persisted(&command).await,
        Err(RuntimeError::Unsupported)
    ));
}

/// The resolved adapter must be the real engine implementation, not a placeholder.
#[tokio::test]
async fn the_resolved_adapter_is_the_engine_implementation() {
    let adapter = resolve(Engine::Vllm, vllm()).unwrap();
    let member = MemberRef {
        deployment_id: "d".into(),
        member_id: "m".into(),
    };
    // vLLM reports what it can prove about work; a placeholder would not answer.
    assert!(adapter.observe_work(&member).await.is_ok());
}

/// The Fake family's whole purpose is the persisted control path: it is what the
/// coordinator drives an ordinary Initialize through. Resolving it to a bare
/// `FakeEngine` produced an adapter that answered `Unsupported` to the only call
/// the lifecycle makes, so every activation on this host stalled after arming.
#[tokio::test]
async fn resolving_the_fake_family_grants_the_persisted_control_path() {
    let adapter = resolve(Engine::Fake, fake()).unwrap();
    let command = RuntimeCommand {
        action: RuntimeAction::Initialize,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d".into(),
                revision: 1,
                generation: 1,
                operation_id: "o".into(),
                step_id: "s".into(),
                qualification_id: "q".into(),
            },
            binding_id: "b".into(),
            incarnation: "i".into(),
            issued_at_ms: 0,
            deadline_ms: i64::MAX,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: Some("g".into()),
            launch_settings: Some(mllm_domain::launch::ProfileLaunchSettings::Fake(
                mllm_domain::launch::FakeLaunchSettings,
            )),
        },
    };
    assert!(
        !matches!(
            adapter.execute_persisted(&command).await,
            Err(RuntimeError::Unsupported)
        ),
        "the Fake family must answer the persisted control path"
    );
}
