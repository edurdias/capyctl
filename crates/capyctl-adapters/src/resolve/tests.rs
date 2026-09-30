use super::*;
use crate::traits::{MemberRef, RuntimeAction, RuntimeCommand};
use capyctl_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};

fn vllm() -> AdapterSpec {
    AdapterSpec::Vllm {
        endpoint: "http://127.0.0.1:8000".parse().unwrap(),
        api_key: None,
        fingerprint: "fp".into(),
        policy: ParkPolicy::Disabled,
        model_id: "m".into(),
        launch: None,
        engine_key: None,
        admin_key: None,
        residency: capyctl_config::effective::Residency::Deep,
    }
}

#[test]
fn every_family_maps_to_its_own_spec() {
    assert_eq!(vllm().engine(), Engine::Vllm);
}

/// A profile's identity — build fingerprint, reserved-flag policy, verification
/// evidence — is only meaningful for the engine it names. Resolving a spec for a
/// different family would attach that identity to the wrong control contract.
#[test]
fn a_spec_for_another_family_is_rejected() {
    let declared = Engine::Sglang;
    assert!(
        matches!(
            resolve(declared, vllm(), None),
            Err(RuntimeError::Unsupported)
        ),
        "a vLLM spec must not resolve as {declared:?}"
    );
}

#[test]
fn a_matching_family_resolves() {
    assert!(resolve(Engine::Vllm, vllm(), None).is_ok());
}

/// vLLM still refuses the persisted control path, which the default trait method
/// provides. Resolution must not appear to grant a capability the engine lacks.
#[tokio::test]
async fn resolution_does_not_invent_a_persisted_control_path() {
    let adapter = resolve(Engine::Vllm, vllm(), None).unwrap();
    let command = RuntimeCommand {
        action: RuntimeAction::Park,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d".into(),
                revision: 1,
                generation: 1,
                operation_id: "o".into(),
                step_id: "s".into(),
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
    let adapter = resolve(Engine::Vllm, vllm(), None).unwrap();
    let member = MemberRef {
        deployment_id: "d".into(),
        member_id: "m".into(),
    };
    // vLLM reports what it can prove about work; a placeholder would not answer.
    assert!(adapter.observe_work(&member).await.is_ok());
}
