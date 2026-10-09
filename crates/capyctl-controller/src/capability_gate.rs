//! SPEC §§6.2, 9.1 / ADR 0010, embedded path: standalone's embedded host
//! refuses a launch, Park or Restore whose residency the engine cannot honor,
//! with the closed reason a host agent refuses it with. Both make the one
//! decision `EffectiveDeployment::deep_wake_refusal` (the agent in
//! `launch_capability` and `park_capability`).
//!
//! Found live 2026-10-06: standalone launched a `deep` SGLang deployment
//! declaring modelopt quantization and parked it; the wake's disk reload
//! failed and the instance was retained uncertain, while a host agent refuses
//! the same launch `capability_missing:deep_park`. A refusal is evidence that
//! nothing happened (SPEC §13, W4): the engine is never asked, so a refused
//! launch starts nothing and gives up at once, and a refused Park or Restore
//! leaves the launch as it was.
//!
//! Passing CPU tests are not qualification evidence (AGENTS.md).

use std::sync::Arc;

use capyctl_adapters::traits::*;
use capyctl_config::effective::EffectiveDeployment;
use capyctl_domain::completion::EffectObservation;

/// Wraps an embedded engine adapter whose frozen configuration the engine
/// cannot park and wake, refusing every Initialize, Park and Restore.
pub struct CapabilityGate {
    inner: Arc<dyn EngineAdapter>,
    refusal: &'static str,
}

impl CapabilityGate {
    /// `inner` itself when `effective` admits every residency action;
    /// otherwise `inner` behind a gate that refuses them.
    pub fn wrap(
        inner: Arc<dyn EngineAdapter>,
        effective: &EffectiveDeployment,
    ) -> Arc<dyn EngineAdapter> {
        match effective.deep_wake_refusal() {
            Some(refusal) => Arc::new(Self { inner, refusal }),
            None => inner,
        }
    }
}

#[async_trait::async_trait]
impl EngineAdapter for CapabilityGate {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        // ADR 0008: the host agent refuses the launch and both residency
        // moves of such a launch; a Stop and every read still reach it.
        if matches!(
            command.action,
            RuntimeAction::Initialize | RuntimeAction::Park | RuntimeAction::Restore
        ) {
            return Err(RuntimeError::Refused(self.refusal.into()));
        }
        self.inner.execute_persisted(command).await
    }
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError> {
        self.inner.inspect(member).await
    }
    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        self.inner.render_plan(plan).await
    }
    async fn check_readiness(&self, member: &MemberRef) -> Result<Readiness, AdapterError> {
        self.inner.check_readiness(member).await
    }
    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError> {
        self.inner.prepare_park(member).await
    }
    async fn park(
        &self,
        member: &MemberRef,
        level: ParkLevel,
    ) -> Result<ParkOutcome, AdapterError> {
        self.inner.park(member, level).await
    }
    async fn restore(&self, member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        self.inner.restore(member).await
    }
    async fn reload_weights(&self, member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        self.inner.reload_weights(member).await
    }
    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        self.inner.observe_work(member).await
    }
    async fn cancel_work(
        &self,
        member: &MemberRef,
        request: &RequestRef,
        require_ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        self.inner.cancel_work(member, request, require_ack).await
    }
    async fn idle_before_signal(
        &self,
        member: &MemberRef,
    ) -> Option<capyctl_adapters::traits::EngineWork> {
        self.inner.idle_before_signal(member).await
    }
    async fn engine_quiescent(&self, member: &MemberRef, after_ms: i64) -> bool {
        self.inner.engine_quiescent(member, after_ms).await
    }
    async fn wake_canary(
        &self,
        context: &capyctl_domain::completion::StepExecutionContext,
        max_tokens: u32,
        bound: std::time::Duration,
    ) -> Result<capyctl_adapters::completion_probe::ProbeAnswer, RuntimeError> {
        self.inner.wake_canary(context, max_tokens, bound).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_config::effective::resolve_effective;
    use capyctl_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts what reaches the engine and answers nothing.
    #[derive(Default)]
    struct Counted(AtomicUsize);
    #[async_trait::async_trait]
    impl EngineAdapter for Counted {
        async fn execute_persisted(
            &self,
            _: &RuntimeCommand,
        ) -> Result<EffectObservation, RuntimeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(RuntimeError::Unsupported)
        }
        async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn cancel_work(
            &self,
            _: &MemberRef,
            _: &RequestRef,
            _: bool,
        ) -> Result<CancellationOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
    }

    /// The SGLang golden configuration, edited.
    fn effective(edit: impl FnOnce(&mut Value)) -> EffectiveDeployment {
        let source: Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-sglang-golden.json"
        ))
        .unwrap();
        let mut deployment = source["input"]["deployment"].clone();
        edit(&mut deployment);
        resolve_effective(&deployment, &source["input"]["host"]).unwrap()
    }

    fn command(action: RuntimeAction) -> RuntimeCommand {
        RuntimeCommand {
            action,
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
                issued_at_ms: 1,
                deadline_ms: 2,
                identities: ExecutionIdentities::OwnedLaunch,
                completion_target: None,
                grant_id: None,
                launch_settings: None,
            },
        }
    }

    /// SPEC §§6.2, 9.1 / ADR 0010: a parking SGLang launch declaring modelopt
    /// quantization is refused `capability_missing:deep_park` on its launch,
    /// its Park and its Restore, as a host agent refuses them
    /// (`sglang_modelopt_quantization_refuses_deep_but_not_restart_only_or_vllm`);
    /// the engine is never asked, so a Park releases nothing. A Stop still
    /// reaches the engine. Without the quantization nothing is gated.
    // T22 T21
    #[tokio::test]
    async fn a_modelopt_sglang_launch_is_refused_its_launch_park_and_restore() {
        for quantization in ["modelopt_fp4", "modelopt", "MODELOPT_FP8", "nvfp4"] {
            let engine = Arc::new(Counted::default());
            let gate = CapabilityGate::wrap(
                engine.clone(),
                &effective(|d| d["engine_config"]["quantization"] = json!(quantization)),
            );
            for action in [
                RuntimeAction::Initialize,
                RuntimeAction::Park,
                RuntimeAction::Restore,
            ] {
                assert_eq!(
                    gate.execute_persisted(&command(action)).await.unwrap_err(),
                    RuntimeError::Refused("capability_missing:deep_park".into()),
                    "{quantization} {action:?}"
                );
            }
            assert_eq!(engine.0.load(Ordering::SeqCst), 0, "{quantization}");
            let _ = gate.execute_persisted(&command(RuntimeAction::Stop)).await;
            assert_eq!(engine.0.load(Ordering::SeqCst), 1, "{quantization}");
        }

        let engine: Arc<dyn EngineAdapter> = Arc::new(Counted::default());
        let gate = CapabilityGate::wrap(engine.clone(), &effective(|_| {}));
        assert!(Arc::ptr_eq(&gate, &engine), "nothing to refuse, no gate");
    }

    /// SPEC §§6.2, 9.1 / ADR 0010: standalone's embedded host refuses a
    /// parking launch of a gpt-oss checkpoint as a host agent does
    /// (`a_gpt_oss_checkpoint_refuses_deep_on_vllm_and_sglang_but_not_restart_only`):
    /// its launch, Park and Restore never reach the engine. Found on the
    /// recipe catalog 2026-10-07: the wake reload fails on SGLang 0.5.21 and
    /// on vLLM 0.30.0. `restart_only` on the same checkpoint is not gated.
    // T22 T21
    #[tokio::test]
    async fn a_gpt_oss_launch_is_refused_its_launch_park_and_restore() {
        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(
            checkpoint.path().join("config.json"),
            r#"{"architectures":["GptOssForCausalLM"],"model_type":"gpt_oss"}"#,
        )
        .unwrap();
        let on_checkpoint = |mut effective: EffectiveDeployment| {
            effective.model.resolved_path = Some(checkpoint.path().to_str().unwrap().into());
            effective
        };
        let engine = Arc::new(Counted::default());
        let gate = CapabilityGate::wrap(engine.clone(), &on_checkpoint(effective(|_| {})));
        for action in [
            RuntimeAction::Initialize,
            RuntimeAction::Park,
            RuntimeAction::Restore,
        ] {
            assert_eq!(
                gate.execute_persisted(&command(action)).await.unwrap_err(),
                RuntimeError::Refused("capability_missing:deep_park".into()),
                "{action:?}"
            );
        }
        assert_eq!(engine.0.load(Ordering::SeqCst), 0);

        let engine: Arc<dyn EngineAdapter> = Arc::new(Counted::default());
        let restart = on_checkpoint(effective(|d| d["residency"] = json!("restart_only")));
        let gate = CapabilityGate::wrap(engine.clone(), &restart);
        assert!(Arc::ptr_eq(&gate, &engine), "restart_only is not gated");
    }
}
