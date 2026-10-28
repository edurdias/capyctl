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
        api: capyctl_config::effective::DeploymentApi::Chat,
        lmcache: None,
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

fn tensorfold() -> AdapterSpec {
    AdapterSpec::Tensorfold {
        endpoint: "http://127.0.0.1:8000".parse().unwrap(),
        fingerprint: "0.6.0".into(),
        model_id: "m".into(),
        line: capyctl_config::tensorfold_native::TensorfoldLine::Python,
        launch: None,
        extensions_built: false,
    }
}

// T41: TensorFold resolves for its own family only.
#[test]
fn a_tensorfold_spec_resolves_only_as_tensorfold() {
    assert_eq!(tensorfold().engine(), Engine::Tensorfold);
    assert!(resolve(Engine::Tensorfold, tensorfold(), None).is_ok());
    assert!(matches!(
        resolve(Engine::Vllm, tensorfold(), None),
        Err(RuntimeError::Unsupported)
    ));
}

// T41 (ADR 0039 §1): a TensorFold spec resolves its line; a launch plan of
// the other line than the spec's is not this binding's launch.
#[test]
fn a_tensorfold_spec_and_its_plan_share_one_line() {
    use capyctl_config::tensorfold_native::TensorfoldLine;
    let with = |line: TensorfoldLine, plan_line: TensorfoldLine| AdapterSpec::Tensorfold {
        endpoint: "http://127.0.0.1:8000".parse().unwrap(),
        fingerprint: "1.0.0".into(),
        model_id: "m".into(),
        line,
        launch: Some(crate::tensorfold::PlanInputTensorfold {
            line: plan_line,
            ..Default::default()
        }),
        extensions_built: false,
    };
    assert!(resolve(
        Engine::Tensorfold,
        with(TensorfoldLine::Native, TensorfoldLine::Native),
        None
    )
    .is_ok());
    assert!(matches!(
        resolve(
            Engine::Tensorfold,
            with(TensorfoldLine::Native, TensorfoldLine::Python),
            None
        ),
        Err(RuntimeError::Unsupported)
    ));
}

fn llamacpp() -> AdapterSpec {
    AdapterSpec::Llamacpp {
        endpoint: "http://127.0.0.1:8000".parse().unwrap(),
        fingerprint: "0.6.0+d812350".into(),
        model_id: "m".into(),
        launch: None,
        api: capyctl_config::effective::DeploymentApi::Chat,
    }
}

// T42: llama.cpp resolves for its own family only.
#[test]
fn a_llamacpp_spec_resolves_only_as_llamacpp() {
    assert_eq!(llamacpp().engine(), Engine::Llamacpp);
    assert!(resolve(Engine::Llamacpp, llamacpp(), None).is_ok());
    assert!(matches!(
        resolve(Engine::Tensorfold, llamacpp(), None),
        Err(RuntimeError::Unsupported)
    ));
}
