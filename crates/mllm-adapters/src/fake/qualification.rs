//! Opt-in deterministic allocation state for the persisted qualification path.
use crate::traits::{RuntimeAction, RuntimeCommand, RuntimeError};
use mllm_domain::completion::{
    EffectObservation, Milestone, ObservationTerminal, ProcessIdentity, ResponseObservation,
    SecurityControlObservation, SecurityEndpoint,
};

#[derive(Clone, Copy, Debug)]
pub enum QualificationFault {
    WrongProbeOutput,
    LostProbeReply,
    FailedProbe,
    MissingProbeFinish,
    StreamMissingDone,
    StreamAfterFinish,
    CrossMarkerOutput,
    UnauthorizedAdminExec,
    UnauthorizedInferenceExec,
    UnauthorizedHealthExec,
    LostSecurityInferenceReply,
    StreamDuplicateField,
    StreamOverflow,
}

#[derive(Debug, Default)]
pub(super) struct QualificationState {
    pub(super) fault: Option<QualificationFault>,
    binding: Option<(String, String)>,
    deployment: Option<String>,
    members: Vec<ProcessIdentity>,
    allocations: bool,
    weights: bool,
    cache: bool,
    quiesced: bool,
    unknown_work: bool,
    runtime_credential: Option<Credential>,
    admin_credential: Option<Credential>,
    work_sequence: u64,
    request_attempts: u64,
    control_attempts: u64,
    alive: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Credential {
    Runtime,
    Admin,
}
impl QualificationState {
    pub(super) fn cleanup(
        &mut self,
        binding: &str,
        incarnation: &str,
        identities: &[ProcessIdentity],
        terminate: bool,
        observed_at_ms: i64,
    ) -> Result<mllm_domain::completion::CleanupEvidence, RuntimeError> {
        if observed_at_ms < 0 {
            return Err(RuntimeError::StaleRevision);
        }
        self.cleanup_with_clock(binding, incarnation, identities, terminate, &|| {
            Ok(observed_at_ms)
        })
    }

    pub(super) fn cleanup_with_clock(
        &mut self,
        binding: &str,
        incarnation: &str,
        identities: &[ProcessIdentity],
        terminate: bool,
        clock: &(dyn Fn() -> Result<i64, RuntimeError> + Send + Sync),
    ) -> Result<mllm_domain::completion::CleanupEvidence, RuntimeError> {
        let mut expected = identities.to_vec();
        expected.sort();
        let mut actual = self.members.clone();
        actual.sort();
        if self
            .binding
            .as_ref()
            .is_none_or(|(b, i)| b != binding || i != incarnation)
            || actual.len() != 2
            || expected != actual
        {
            return Err(RuntimeError::StaleRevision);
        }
        if terminate {
            self.control_attempts += 1;
            self.alive = false;
            self.allocations = false;
            self.weights = false;
            self.cache = false;
            self.unknown_work = false;
        }
        if self.alive {
            return Err(RuntimeError::Uncertain(
                "owned Fake members remain alive".into(),
            ));
        }
        let observed_at_ms = clock()?;
        if observed_at_ms < 0 {
            return Err(RuntimeError::Uncertain(
                "cleanup observation clock is negative".into(),
            ));
        }
        Ok(mllm_domain::completion::CleanupEvidence {
            binding_id: binding.into(),
            incarnation: incarnation.into(),
            identities: actual,
            observed_at_ms,
            receipt: "qualification-fake-v1:verified-api-and-worker-gone".into(),
        })
    }
    pub(super) fn activity(&self) -> (u64, u64, u64) {
        (
            self.request_attempts,
            self.control_attempts,
            self.work_sequence,
        )
    }
    fn authorize(
        &self,
        endpoint: SecurityEndpoint,
        credential: Option<Credential>,
    ) -> Result<(), u16> {
        if matches!(
            (self.fault, endpoint),
            (
                Some(QualificationFault::UnauthorizedAdminExec),
                SecurityEndpoint::AdminControl
            ) | (
                Some(QualificationFault::UnauthorizedInferenceExec),
                SecurityEndpoint::Inference
            ) | (
                Some(QualificationFault::UnauthorizedHealthExec),
                SecurityEndpoint::HealthGeneration
            ) | (
                Some(QualificationFault::LostSecurityInferenceReply),
                SecurityEndpoint::Inference
            )
        ) {
            return Ok(());
        }
        if credential.is_none() {
            return Err(401);
        }
        match endpoint {
            SecurityEndpoint::AdminControl if credential == self.admin_credential => {
                Ok(())
            }
            SecurityEndpoint::Inference if credential == self.runtime_credential => Ok(()),
            _ => Err(403),
        }
    }
    fn negative_check(
        &mut self,
        endpoint: SecurityEndpoint,
    ) -> (ObservationTerminal, ResponseObservation) {
        if endpoint == SecurityEndpoint::AdminControl {
            self.control_attempts += 1;
        } else {
            self.request_attempts += 1;
        }
        let before = self.work_sequence;
        let presented = match endpoint {
            SecurityEndpoint::Inference => None,
            _ => self.runtime_credential,
        };
        let status = match self.authorize(endpoint, presented) {
            Err(status) => status,
            Ok(()) => {
                self.work_sequence += 1;
                match endpoint {
                    SecurityEndpoint::AdminControl => {
                        self.allocations = false;
                        self.weights = false;
                        self.cache = false;
                    }
                    _ => {
                        self.unknown_work = true;
                    }
                }
                200
            }
        };
        let no_work = before == self.work_sequence && !self.unknown_work;
        let status = if matches!(
            (self.fault, endpoint),
            (
                Some(QualificationFault::LostSecurityInferenceReply),
                SecurityEndpoint::Inference
            )
        ) {
            0
        } else {
            status
        };
        (
            if status >= 400 && no_work {
                ObservationTerminal::RejectedWithoutWork
            } else {
                ObservationTerminal::Uncertain
            },
            ResponseObservation::SecurityRejection {
                endpoint,
                status,
                no_work,
                separate_credentials: self.runtime_credential.is_some()
                    && self.admin_credential.is_some()
                    && self.runtime_credential != self.admin_credential,
            },
        )
    }
    pub(super) fn security_control(
        &mut self,
        command: &RuntimeCommand,
    ) -> Result<SecurityControlObservation, RuntimeError> {
        let identities = self.members(&command.context)?;
        if command.action != RuntimeAction::Park
            || command.context.completion_target.is_some()
            || command.context.grant_id.is_none()
        {
            return Err(RuntimeError::Unsupported);
        }
        let (terminal, response) = self.negative_check(SecurityEndpoint::AdminControl);
        Ok(SecurityControlObservation {
            effect: EffectObservation {
                token: command.context.token.clone(),
                binding_id: command.context.binding_id.clone(),
                incarnation: command.context.incarnation.clone(),
                identities,
                observed_at_ms: command.context.issued_at_ms,
                receipt: format!(
                    "qualification-fake-v1:negative-admin:{}",
                    command.context.token.step_id
                ),
                facts: vec![],
            },
            terminal,
            response,
        })
    }
    pub(super) fn security_request(
        &mut self,
        context: &mllm_domain::completion::StepExecutionContext,
        endpoint: SecurityEndpoint,
        body: &serde_json::Value,
    ) -> Result<(ObservationTerminal, ResponseObservation), RuntimeError> {
        self.members(context)?;
        let expected = serde_json::json!({"model":format!("candidate-{}",context.token.deployment_id),"messages":[{"role":"user","content":"Repeat exactly: MLLM_READY_13"}],"temperature":0,"max_tokens":16,"stream":false});
        if endpoint == SecurityEndpoint::AdminControl || body != &expected {
            return Err(RuntimeError::Unsupported);
        }
        Ok(self.negative_check(endpoint))
    }
    pub(super) fn members(
        &self,
        context: &mllm_domain::completion::StepExecutionContext,
    ) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        let mllm_domain::completion::ExecutionIdentities::Retained(expected) = &context.identities
        else {
            return Err(RuntimeError::Unsupported);
        };
        let mut expected = expected.clone();
        expected.sort();
        let mut actual = self.members.clone();
        actual.sort();
        if self.binding.as_ref() != Some(&(context.binding_id.clone(), context.incarnation.clone()))
            || !self.alive
            || self.deployment.as_deref() != Some(context.token.deployment_id.as_str())
            || expected != actual
            || actual.len() != 2
        {
            return Err(RuntimeError::StaleRevision);
        }
        Ok(actual)
    }
    pub(super) fn parked_status(
        &self,
        c: &mllm_domain::completion::StepExecutionContext,
    ) -> Result<mllm_domain::completion::ParkedStatusObservation, RuntimeError> {
        let before = self.activity();
        let identities = self.members(c)?;
        Ok(
            mllm_domain::completion::ParkedStatusObservation {
                token: c.token.clone(),
                binding_id: c.binding_id.clone(),
                incarnation: c.incarnation.clone(),
                identities,
                observed_at_ms: c.issued_at_ms,
                receipt: format!("qualification-fake-v1:parked-status:{}", c.token.step_id),
                allocations: self.allocations,
                weights: self.weights,
                cache: self.cache,
                quiesced: self.quiesced,
                unknown_work: self.unknown_work,
                activity_before: before,
                activity_after: self.activity(),
            },
        )
    }
    pub(super) fn forward(
        &mut self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::traits::AdapterError> {
        self.request_attempts += 1;
        if !self.allocations || !self.weights || !self.cache {
            return Err(crate::traits::AdapterError::PolicyDenied);
        }
        let model = format!(
            "candidate-{}",
            self.deployment
                .as_deref()
                .ok_or(crate::traits::AdapterError::UnsupportedCombination)?
        );
        let content = match body["messages"][0]["content"].as_str() {
            Some("Repeat exactly: MLLM_READY_13") => "MLLM_READY_13",
            Some("Repeat exactly: MLLM_ALPHA_71") => "MLLM_ALPHA_71",
            Some("Repeat exactly: MLLM_BETA_29") => "MLLM_BETA_29",
            _ => return Err(crate::traits::AdapterError::PolicyDenied),
        };
        let expected = serde_json::json!({"model":model,"messages":[{"role":"user","content":format!("Repeat exactly: {content}")}],"temperature":0,"max_tokens":16,"stream":false});
        if body != &expected {
            return Err(crate::traits::AdapterError::PolicyDenied);
        }
        self.quiesced = false;
        self.work_sequence += 1;
        match self.fault {
            Some(QualificationFault::LostProbeReply) => {
                self.unknown_work = true;
                return Err(crate::traits::AdapterError::Uncertain(
                    "qualification probe reply lost".into(),
                ));
            }
            Some(QualificationFault::FailedProbe) => {
                return Err(crate::traits::AdapterError::PolicyDenied);
            }
            _ => {}
        }
        let content = if matches!(self.fault, Some(QualificationFault::WrongProbeOutput)) {
            "wrong output"
        } else if matches!(self.fault, Some(QualificationFault::CrossMarkerOutput)) {
            "MLLM_ALPHA_71 MLLM_BETA_29"
        } else {
            content
        };
        let mut response = serde_json::json!({"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}]});
        if matches!(self.fault, Some(QualificationFault::MissingProbeFinish)) {
            response["choices"][0]
                .as_object_mut()
                .unwrap()
                .remove("finish_reason");
        }
        Ok(response)
    }
    pub(super) fn stream(
        &mut self,
        body: &serde_json::Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<crate::traits::StreamEnded, crate::traits::AdapterError> {
        if body["stream"] != true {
            return Err(crate::traits::AdapterError::PolicyDenied);
        }
        let mut nonstream = body.clone();
        nonstream["stream"] = serde_json::json!(false);
        let response = self.forward(&nonstream)?;
        let content = response["choices"][0]["message"]["content"]
            .as_str()
            .unwrap();
        let model = &response["model"];
        let (first, last) = content.split_at(content.len() / 2);
        if matches!(self.fault, Some(QualificationFault::StreamOverflow)) {
            for _ in 0..4097 {
                on_chunk(serde_json::json!({"model":model,"choices":[{"index":0,"delta":{"content":""},"finish_reason":null}]}).to_string());
            }
        }
        for text in [first, last] {
            let raw=serde_json::json!({"model":model,"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}).to_string();
            on_chunk(
                if matches!(self.fault, Some(QualificationFault::StreamDuplicateField)) {
                    raw.replacen("\"model\":", "\"model\":\"wrong\",\"model\":", 1)
                } else {
                    raw
                },
            );
        }
        on_chunk(serde_json::json!({"model":model,"choices":[{"index":0,"delta":{},"finish_reason":response["choices"][0]["finish_reason"]}]}).to_string());
        if matches!(self.fault, Some(QualificationFault::StreamAfterFinish)) {
            on_chunk(serde_json::json!({"model":model,"choices":[{"index":0,"delta":{"content":"contamination"},"finish_reason":null}]}).to_string());
        }
        if matches!(self.fault, Some(QualificationFault::StreamMissingDone)) {
            self.unknown_work = true;
            return Ok(crate::traits::StreamEnded::BackendClosed);
        }
        Ok(crate::traits::StreamEnded::Completed)
    }
    #[cfg(test)]
    pub(super) fn execute(
        &mut self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        self.execute_with_clock(command, None)
    }

    pub(super) fn execute_with_clock(
        &mut self,
        command: &RuntimeCommand,
        clock: Option<&(dyn Fn() -> Result<i64, RuntimeError> + Send + Sync)>,
    ) -> Result<EffectObservation, RuntimeError> {
        let c = &command.context;
        // Ordinary cold initialization explicitly includes a model-usability
        // probe. Candidate child effects retain their separate probe protocol.
        // This dispatch shape recognizes scope; catalog authority stays in Store.
        //
        // Scope is read from what the command is, not from how its qualification
        // id is spelled. A candidate child effect acts on identities it retains
        // and names no completion target; only an ordinary cold Initialize owns
        // the launch and is asked to land on Ready. Requiring a `qualified:` id
        // as well excluded every restart-only deployment, which carries a
        // declared identity and is still an ordinary initialize.
        let ordinary = command.action == RuntimeAction::Initialize
            && matches!(
                c.identities,
                mllm_domain::completion::ExecutionIdentities::OwnedLaunch
            )
            && c.completion_target.as_ref().is_some_and(|p| {
                p.phase == mllm_domain::resources::ResourcePhase::Ready
                    && mllm_domain::resources::validate_footprint(p).is_ok()
            });
        if (c.completion_target.is_some() && !ordinary)
            || c.grant_id.is_none()
            || c.issued_at_ms >= c.deadline_ms
        {
            return Err(RuntimeError::Unsupported);
        }
        if command.action == RuntimeAction::Probe {
            return Err(RuntimeError::Unsupported);
        }
        if command.action != RuntimeAction::Initialize {
            self.members(c)?;
        }
        self.control_attempts += 1;
        if let Some((binding, incarnation)) = &self.binding {
            if binding != &c.binding_id || incarnation != &c.incarnation {
                return Err(RuntimeError::StaleRevision);
            }
        } else if command.action != RuntimeAction::Initialize {
            return Err(RuntimeError::Missing);
        }
        let mut facts = match command.action {
            RuntimeAction::Initialize if self.binding.is_none() => {
                if !matches!(
                    c.launch_settings,
                    Some(mllm_domain::launch::ProfileLaunchSettings::Fake(_))
                ) {
                    return Err(RuntimeError::Unsupported);
                }
                self.binding = Some((c.binding_id.clone(), c.incarnation.clone()));
                self.deployment = Some(c.token.deployment_id.clone());
                self.members = vec![
                    ProcessIdentity {
                        role: "api".into(),
                        pid: 71,
                        boot_id: "qualification-fake-boot".into(),
                        start_ticks: 100,
                    },
                    ProcessIdentity {
                        role: "worker-0".into(),
                        pid: 72,
                        boot_id: "qualification-fake-boot".into(),
                        start_ticks: 101,
                    },
                ];
                self.allocations = true;
                self.alive = true;
                self.weights = true;
                self.cache = true;
                self.runtime_credential = Some(Credential::Runtime);
                self.admin_credential = Some(Credential::Admin);
                vec![
                    Milestone::AllocationsRestored,
                    Milestone::WeightsUsable,
                    Milestone::CacheValid,
                ]
            }
            RuntimeAction::Drain if self.allocations && !self.unknown_work => {
                self.quiesced = true;
                vec![Milestone::Quiesced]
            }
            RuntimeAction::Park if self.quiesced && self.allocations && !self.unknown_work => {
                self.allocations = false;
                self.weights = false;
                self.cache = false;
                vec![Milestone::MemoryReleased]
            }
            RuntimeAction::Restore if !self.allocations => {
                self.allocations = true;
                self.quiesced = false;
                vec![Milestone::AllocationsRestored]
            }
            RuntimeAction::ReloadWeights if self.allocations && !self.weights => {
                self.weights = true;
                vec![Milestone::WeightsUsable]
            }
            RuntimeAction::InvalidateCache if self.allocations && self.weights && !self.cache => {
                self.cache = true;
                vec![Milestone::CacheValid]
            }
            _ => return Err(RuntimeError::Unsupported),
        };
        if ordinary {
            let model = format!("candidate-{}", c.token.deployment_id);
            let body = serde_json::json!({"model":model,"messages":[{"role":"user","content":"Repeat exactly: MLLM_READY_13"}],"temperature":0,"max_tokens":16,"stream":false});
            let result = self.forward(&body).map_err(|_| {
                RuntimeError::Uncertain("ordinary Fake readiness probe failed".into())
            })?;
            if result["model"] != model
                || result["choices"][0]["message"]["content"] != "MLLM_READY_13"
                || result["choices"][0]["finish_reason"] != "stop"
            {
                return Err(RuntimeError::Uncertain(
                    "ordinary Fake readiness probe failed".into(),
                ));
            }
            facts.push(Milestone::ModelUsable);
        }
        let observed_at_ms = match clock {
            Some(clock) => clock()?,
            None => c.issued_at_ms,
        };
        if observed_at_ms < c.issued_at_ms || observed_at_ms >= c.deadline_ms {
            return Err(RuntimeError::Uncertain(
                "Fake observation clock outside accepted deadline".into(),
            ));
        }
        Ok(EffectObservation {
            token: c.token.clone(),
            binding_id: c.binding_id.clone(),
            incarnation: c.incarnation.clone(),
            identities: self.members.clone(),
            observed_at_ms,
            receipt: format!(
                "qualification-fake-v1:{:?}:{}",
                command.action, c.token.step_id
            ),
            facts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::RuntimeAction;
    use mllm_domain::completion::{
        ExecutionIdentities, Milestone, StepExecutionContext, TransitionToken,
    };
    fn command(action: RuntimeAction, step: &str) -> RuntimeCommand {
        RuntimeCommand {
            action,
            context: StepExecutionContext {
                token: TransitionToken {
                    deployment_id: "deployment".into(),
                    revision: 1,
                    generation: 1,
                    operation_id: "operation".into(),
                    step_id: step.into(),
                },
                binding_id: "binding".into(),
                incarnation: "incarnation".into(),
                issued_at_ms: 1200,
                deadline_ms: 400000,
                identities: ExecutionIdentities::OwnedLaunch,
                completion_target: None,
                grant_id: Some("grant".into()),
                launch_settings: Some(mllm_domain::launch::ProfileLaunchSettings::Fake(
                    mllm_domain::launch::FakeLaunchSettings,
                )),
            },
        }
    }
    #[test]
    fn qualified_initialize_proves_ready_with_real_fake_probe() {
        use mllm_domain::resources::{Allocation, PhaseFootprint, ResourcePhase};
        let mut c = command(RuntimeAction::Initialize, "ordinary");
        c.context.completion_target = Some(PhaseFootprint {
            phase: ResourcePhase::Ready,
            allocations: vec![Allocation {
                domain: "unified".into(),
                bytes: 8,
                host_kv_bytes: 1,
            }],
            devices: vec![],
        });
        let mut state = QualificationState::default();
        let result = state.execute(&c).unwrap();
        assert_eq!(
            result.facts,
            vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable
            ]
        );
        assert_eq!(state.activity(), (1, 1, 1));
        assert!(state.execute(&c).is_err());
        for fault in [
            QualificationFault::WrongProbeOutput,
            QualificationFault::LostProbeReply,
            QualificationFault::FailedProbe,
            QualificationFault::MissingProbeFinish,
        ] {
            let mut state = QualificationState {
                fault: Some(fault),
                ..Default::default()
            };
            assert!(state.execute(&c).is_err());
            assert!(state.allocations);
            assert!(state.alive);
        }
    }

    /// Scope is read from the command, not from any identity string. A candidate
    /// child effect acts on identities it retains and names no completion target,
    /// so it keeps its own probe protocol and never takes the ordinary path.
    #[test]
    fn a_child_effect_is_not_an_ordinary_initialize() {
        let c = command(RuntimeAction::Initialize, "child");
        let mut state = QualificationState::default();
        let result = state.execute(&c).unwrap();
        assert!(
            !result.facts.contains(&Milestone::ModelUsable),
            "no completion target means no ordinary readiness probe"
        );
    }

    #[tokio::test]
    async fn cleanup_samples_its_own_observation_clock_after_control() {
        use crate::{fake::FakeEngine, traits::EngineAdapter};
        use std::sync::{
            atomic::{AtomicI64, Ordering},
            Arc,
        };
        let now = Arc::new(AtomicI64::new(1300));
        let clock = now.clone();
        let engine = FakeEngine::for_qualification_with_clock(Arc::new(move || {
            Ok(clock.load(Ordering::SeqCst))
        }));
        let c = command(RuntimeAction::Initialize, "initialize");
        let initialized = engine.execute_persisted(&c).await.unwrap();
        let activity=engine.qualification_activity().unwrap();
        assert!(engine.qualification_cleanup_mode_observed(&initialized.binding_id,&initialized.incarnation,&initialized.identities,false).is_err(),"inspection must not terminate live Fake members");
        assert_eq!(engine.qualification_activity().unwrap(),activity);
        now.store(1700, Ordering::SeqCst);
        let gone = engine
            .qualification_cleanup_observed(
                &initialized.binding_id,
                &initialized.incarnation,
                &initialized.identities,
            )
            .unwrap();
        assert_eq!(gone.observed_at_ms, 1700);
        assert_eq!(gone.identities, initialized.identities);
        now.store(1800,Ordering::SeqCst);
        let inspected=engine.qualification_cleanup_mode_observed(&initialized.binding_id,&initialized.incarnation,&initialized.identities,false).unwrap();
        assert_eq!(inspected.observed_at_ms,1800);
        assert_eq!(inspected.identities,initialized.identities);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let failed = FakeEngine::for_qualification_with_clock(Arc::new(move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(1300)
            } else {
                Err(RuntimeError::Uncertain("cleanup clock unavailable".into()))
            }
        }));
        let identities = failed.execute_persisted(&c).await.unwrap().identities;
        assert!(failed
            .qualification_cleanup_observed(
                &c.context.binding_id,
                &c.context.incarnation,
                &identities
            )
            .is_err());
        // Failure to timestamp is after the control: read-only inspection sees
        // those exact members gone without another terminate operation.
        assert!(failed
            .qualification_cleanup(
                &c.context.binding_id,
                &c.context.incarnation,
                &identities,
                false,
                1700
            )
            .is_ok());
    }

    #[tokio::test]
    async fn service_clock_timestamps_actual_fake_effect_and_failure_is_uncertain() {
        use crate::{fake::FakeEngine, traits::EngineAdapter};
        use std::sync::Arc;
        let c = command(RuntimeAction::Initialize, "initialize");
        let engine = FakeEngine::for_qualification_with_clock(Arc::new(|| Ok(1300)));
        let result = engine.execute_persisted(&c).await.unwrap();
        assert_eq!(result.observed_at_ms, 1300);
        assert_ne!(result.observed_at_ms, c.context.issued_at_ms);
        assert!(!result.identities.is_empty());
        for now in [1199, 400000] {
            let engine = FakeEngine::for_qualification_with_clock(Arc::new(move || Ok(now)));
            assert!(matches!(
                engine.execute_persisted(&c).await,
                Err(RuntimeError::Uncertain(_))
            ));
            assert!(engine.execute_persisted(&c).await.is_err());
        }
        let engine = FakeEngine::for_qualification_with_clock(Arc::new(|| {
            Err(RuntimeError::Uncertain("clock unavailable".into()))
        }));
        assert!(matches!(
            engine.execute_persisted(&c).await,
            Err(RuntimeError::Uncertain(_))
        ));
    }

    #[test]
    fn persisted_fake_restore_does_not_reload_invalidate_or_probe() {
        let mut state = QualificationState::default();
        let initialized = state
            .execute(&command(RuntimeAction::Initialize, "initialize"))
            .unwrap();
        assert_eq!(
            initialized.facts,
            vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid
            ]
        );
        assert_eq!(initialized.identities.len(), 2);
        let warm_command = |action, step| {
            let mut c = command(action, step);
            c.context.identities = ExecutionIdentities::Retained(initialized.identities.clone());
            c.context.launch_settings = None;
            c
        };
        state
            .execute(&warm_command(RuntimeAction::Drain, "drain"))
            .unwrap();
        state.unknown_work = true;
        assert!(state
            .execute(&warm_command(RuntimeAction::Park, "unknown-work"))
            .is_err());
        state.unknown_work = false;
        state
            .execute(&warm_command(RuntimeAction::Park, "park"))
            .unwrap();
        let restored = state
            .execute(&warm_command(RuntimeAction::Restore, "restore"))
            .unwrap();
        assert_eq!(restored.facts, vec![Milestone::AllocationsRestored]);
        assert!(state
            .execute(&warm_command(
                RuntimeAction::InvalidateCache,
                "invalidate-too-early"
            ))
            .is_err());
        assert_eq!(
            state
                .execute(&warm_command(RuntimeAction::ReloadWeights, "reload"))
                .unwrap()
                .facts,
            vec![Milestone::WeightsUsable]
        );
        assert_eq!(
            state
                .execute(&warm_command(RuntimeAction::InvalidateCache, "invalidate"))
                .unwrap()
                .facts,
            vec![Milestone::CacheValid]
        );
        assert!(matches!(
            state.execute(&command(RuntimeAction::Probe, "probe")),
            Err(RuntimeError::Unsupported)
        ));
    }
}
