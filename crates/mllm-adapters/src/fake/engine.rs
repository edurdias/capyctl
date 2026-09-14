//! A deterministic fake engine implementing [`crate::traits::EngineAdapter`].
//!
//! The fake is the executable spec of the behavioral contracts the real F1/F2
//! adapters must honor: slow startup (liveness is not readiness), park levels
//! with distinct memory-retention signatures, ambiguous outcomes (effect
//! applied, ack lost), the deep-park policy gate, and crash injection.

use crate::traits::*;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Resident bytes with everything loaded (weights + KV + buffers).
pub const FULL_RESIDENT_BYTES: i64 = 4096;
/// Retained bytes after a level-1 park: KV cache dropped, CPU weight backup kept.
pub const LEVEL1_RETAINED_BYTES: i64 = 1024;
/// Retained bytes after a level-2 park: weights and KV discarded, buffers kept.
pub const BUFFER_RESIDUE: i64 = 256;

/// Stable build fingerprint the fake reports through `EngineState`
/// (parked-state observability contract, F1 design §3).
pub const FAKE_BUILD_FINGERPRINT: &str = "fake-engine-1";

/// The deep-park security gate: experimental level-2 operations are denied
/// unless the host explicitly opts in (design §9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParkPolicy {
    /// Default: level-2 park and weight reload are deterministically denied.
    #[default]
    Denied,
    /// Host explicitly allows the experimental deep-park paths.
    ExperimentalAllowed,
}

#[derive(Debug, Clone)]
struct MemberState {
    phase: Phase,
    retained_bytes: i64,
    reload_count: u64,
}

#[derive(Debug)]
struct Knobs {
    policy: ParkPolicy,
    startup_delay: Option<Duration>,
    fail_at: Option<Phase>,
    ambiguous_park: bool,
}

/// Deterministic engine simulator: one state per member (deployments on
/// the same engine share the adapter but hold independent member states —
/// A parked must not make B unready).
#[derive(Debug)]
pub struct FakeEngine {
    qualification: Mutex<Option<super::qualification::QualificationState>>,
    knobs: Mutex<Knobs>,
    states: Mutex<HashMap<String, MemberState>>,
    started_at: std::time::Instant,
}

impl FakeEngine {
    pub fn new() -> Self {
        Self {
            qualification: Mutex::new(None),
            knobs: Mutex::new(Knobs {
                policy: ParkPolicy::default(),
                startup_delay: None,
                fail_at: None,
                ambiguous_park: false,
            }),
            states: Mutex::new(HashMap::new()),
            started_at: std::time::Instant::now(),
        }
    }

    /// Opt in to the separate persisted qualification state machine.
    pub fn for_qualification() -> Self {
        let engine = Self::new();
        *engine.qualification.lock().unwrap() =
            Some(super::qualification::QualificationState::default());
        engine
    }

    /// Fault injection for the opt-in qualification runtime.
    pub fn with_qualification_fault(self, fault: super::QualificationFault) -> Self {
        if let Some(state) = self.qualification.lock().unwrap().as_mut() {
            state.fault = Some(fault);
        }
        self
    }

    /// Read-only collector check against actual opt-in Fake membership.
    pub fn qualification_members(
        &self,
        context: &mllm_domain::completion::StepExecutionContext,
    ) -> Result<Vec<mllm_domain::completion::ProcessIdentity>, RuntimeError> {
        self.qualification
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .members(context)
    }
    /// Executes one authorized Fake cleanup or inspects already terminated owned members.
    pub fn qualification_cleanup(
        &self,
        binding: &str,
        incarnation: &str,
        identities: &[mllm_domain::completion::ProcessIdentity],
        terminate: bool,
        observed_at_ms: i64,
    ) -> Result<mllm_domain::completion::CleanupEvidence, RuntimeError> {
        self.qualification
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(RuntimeError::Unsupported)?
            .cleanup(binding, incarnation, identities, terminate, observed_at_ms)
    }
    pub fn qualification_security_control(
        &self,
        command: &RuntimeCommand,
    ) -> Result<mllm_domain::qualification::CandidateSecurityControlObservation, RuntimeError> {
        self.qualification
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(RuntimeError::Unsupported)?
            .security_control(command)
    }
    pub fn qualification_parked_status(
        &self,
        context: &mllm_domain::completion::StepExecutionContext,
    ) -> Result<mllm_domain::qualification::CandidateParkedStatusObservation, RuntimeError> {
        self.qualification
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .parked_status(context)
    }
    /// Read-only opt-in Fake activity: inference sends, control sends, work started.
    pub fn qualification_activity(&self) -> Result<(u64, u64, u64), RuntimeError> {
        Ok(self
            .qualification
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .activity())
    }
    pub fn qualification_security_request(
        &self,
        context: &mllm_domain::completion::StepExecutionContext,
        endpoint: mllm_domain::qualification::CandidateSecurityEndpoint,
        body: &serde_json::Value,
    ) -> Result<
        (
            mllm_domain::qualification::CandidateTerminal,
            mllm_domain::qualification::CandidateResponseObservation,
        ),
        RuntimeError,
    > {
        self.qualification
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(RuntimeError::Unsupported)?
            .security_request(context, endpoint, body)
    }

    pub fn with_startup_delay(self, d: Duration) -> Self {
        self.knobs.lock().unwrap().startup_delay = Some(d);
        self
    }

    /// Crash injection: the operation belonging to `phase` fails with
    /// [`AdapterError::Crash`].
    pub fn fail_at(self, phase: Phase) -> Self {
        self.knobs.lock().unwrap().fail_at = Some(phase);
        self
    }

    /// Ambiguous-outcome injection: park effects are applied but the ack is
    /// lost, so the adapter must report [`AdapterError::Uncertain`].
    pub fn ambiguous_park(self) -> Self {
        self.knobs.lock().unwrap().ambiguous_park = true;
        self
    }

    /// Mark subsequent parks ambiguous: the effect applies but the ack is
    /// lost (qualification/ambiguity injection).
    pub fn set_ambiguous_park(&self) {
        self.knobs.lock().unwrap().ambiguous_park = true;
    }

    /// Sets the deep-park policy gate (default: [`ParkPolicy::Denied`]).
    pub fn with_policy(self, p: ParkPolicy) -> Self {
        self.knobs.lock().unwrap().policy = p;
        self
    }

    /// How many times weights were reloaded (must be exactly once per reload).
    pub fn reload_weights_count(&self) -> u64 {
        self.states
            .lock()
            .unwrap()
            .values()
            .map(|s| s.reload_count)
            .sum()
    }
}

impl Default for FakeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeEngine {
    fn member(&self, m: &MemberRef) -> MemberState {
        self.states
            .lock()
            .unwrap()
            .entry(m.member_id.clone())
            .or_insert_with(|| MemberState {
                phase: Phase::Startup,
                retained_bytes: FULL_RESIDENT_BYTES,
                reload_count: 0,
            })
            .clone()
    }

    fn with_member<R>(&self, m: &MemberRef, f: impl FnOnce(&mut MemberState) -> R) -> R {
        let mut guard = self.states.lock().unwrap();
        let st = guard
            .entry(m.member_id.clone())
            .or_insert_with(|| MemberState {
                phase: Phase::Startup,
                retained_bytes: FULL_RESIDENT_BYTES,
                reload_count: 0,
            });
        f(st)
    }
}

fn park_retained_bytes(level: ParkLevel) -> i64 {
    match level {
        ParkLevel::One => LEVEL1_RETAINED_BYTES,
        ParkLevel::Two => BUFFER_RESIDUE,
    }
}

#[async_trait]
impl EngineAdapter for FakeEngine {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<mllm_domain::qualification::EffectObservation, RuntimeError> {
        self.qualification
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(RuntimeError::Unsupported)?
            .execute(command)
    }
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError> {
        let st = self.member(member);
        Ok(EngineState {
            phase: st.phase,
            retained_bytes: st.retained_bytes,
            build_fingerprint: Some(FAKE_BUILD_FINGERPRINT.into()),
        })
    }

    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        let mut argv = vec![
            "fake-engine".to_string(),
            "--deployment".to_string(),
            plan.deployment_id.clone(),
            "--member".to_string(),
            plan.member_id.clone(),
        ];
        let mut env = std::collections::BTreeMap::new();
        if let Some(level) = plan.park_level {
            argv.push("--park-level".to_string());
            argv.push(match level {
                ParkLevel::One => "1".to_string(),
                ParkLevel::Two => "2".to_string(),
            });
            env.insert(
                "MLLM_PARK_LEVEL".to_string(),
                match level {
                    ParkLevel::One => "1".to_string(),
                    ParkLevel::Two => "2".to_string(),
                },
            );
        }
        Ok(RenderedCommand { argv, env })
    }

    async fn check_readiness(&self, _member: &MemberRef) -> Result<Readiness, AdapterError> {
        let knobs = self.knobs.lock().unwrap();
        if knobs.fail_at == Some(Phase::Startup) {
            return Err(AdapterError::Crash(Phase::Startup));
        }
        // A parked engine is not ready: parked-state observability (F1 design
        // §3) forbids reading a parked member as Ready.
        if self.member(_member).phase == Phase::Parked {
            return Ok(Readiness::Initializing);
        }
        if let Some(delay) = knobs.startup_delay {
            if self.started_at.elapsed() < delay {
                return Ok(Readiness::Initializing);
            }
        }
        let member = _member.clone();
        self.with_member(&member, |st| st.phase = Phase::Ready);
        Ok(Readiness::Ready)
    }

    async fn prepare_park(&self, _member: &MemberRef) -> Result<Quiescence, AdapterError> {
        if self.knobs.lock().unwrap().fail_at == Some(Phase::Parking) {
            return Err(AdapterError::Crash(Phase::Parking));
        }
        Ok(Quiescence { quiescent: true })
    }

    async fn park(
        &self,
        _member: &MemberRef,
        level: ParkLevel,
    ) -> Result<ParkOutcome, AdapterError> {
        let (policy, ambiguous, fail_at) = {
            let knobs = self.knobs.lock().unwrap();
            (knobs.policy, knobs.ambiguous_park, knobs.fail_at)
        };
        if fail_at == Some(Phase::Parking) {
            return Err(AdapterError::Crash(Phase::Parking));
        }
        // Deep-park security gate: level 2 is experimental and denied by default.
        if level == ParkLevel::Two && policy == ParkPolicy::Denied {
            return Err(AdapterError::PolicyDenied);
        }
        let retained = park_retained_bytes(level);
        {
            let member = _member.clone();
            self.with_member(&member, |st| {
                st.phase = Phase::Parked;
                st.retained_bytes = retained;
            });
        }
        if ambiguous {
            // The effect was applied above, but the ack is lost: the caller
            // must reconcile; the adapter never fabricates a success.
            return Err(AdapterError::Uncertain("park applied, ack lost".into()));
        }
        Ok(ParkOutcome::Parked {
            retained_bytes: retained,
        })
    }

    async fn restore(&self, _member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        if self.knobs.lock().unwrap().fail_at == Some(Phase::Restore) {
            return Err(AdapterError::Crash(Phase::Restore));
        }
        let member = _member.clone();
        self.with_member(&member, |st| {
            st.phase = Phase::Ready;
            st.retained_bytes = FULL_RESIDENT_BYTES;
        });
        Ok(RestoreOutcome::Restored)
    }

    async fn reload_weights(&self, _member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        let knobs = self.knobs.lock().unwrap();
        if knobs.fail_at == Some(Phase::Restore) {
            return Err(AdapterError::Crash(Phase::Restore));
        }
        // Reloading weights after a deep park is part of the experimental
        // level-2 path: gated by the same policy as the park itself.
        if knobs.policy == ParkPolicy::Denied {
            return Err(AdapterError::PolicyDenied);
        }
        drop(knobs);
        let member = _member.clone();
        self.with_member(&member, |st| {
            st.reload_count += 1;
            st.retained_bytes = FULL_RESIDENT_BYTES;
        });
        Ok(ReloadOutcome::Reloaded)
    }

    async fn observe_work(&self, _member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Ok(WorkObservation::Idle)
    }

    async fn cancel_work(
        &self,
        _member: &MemberRef,
        _req: &RequestRef,
        require_ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        // Without an ack the outcome is unknown: never report success.
        if require_ack {
            Ok(CancellationOutcome::Acknowledged)
        } else {
            Ok(CancellationOutcome::Uncertain)
        }
    }
}

#[async_trait]
impl crate::traits::ChatForward for FakeEngine {
    async fn forward_chat(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        if let Some(state) = self.qualification.lock().unwrap().as_mut() {
            return state.forward(body);
        }
        let model = body["model"].as_str().unwrap_or("fake").to_string();
        Ok(serde_json::json!({
            "id": "fake-completion",
            "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}]
        }))
    }

    async fn forward_chat_stream(
        &self,
        body: &serde_json::Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<crate::traits::StreamEnded, AdapterError> {
        if let Some(state) = self.qualification.lock().unwrap().as_mut() {
            return state.stream(body, on_chunk);
        }
        let model = body["model"].as_str().unwrap_or("fake").to_string();
        on_chunk(format!(
            r#"{{"id":"fake-stream","model":"{model}","choices":[{{"delta":{{"content":"hel"}}}}]}}"#
        ));
        on_chunk(r#"{"choices":[{"delta":{"content":"lo"}}]}"#.to_string());
        Ok(crate::traits::StreamEnded::Completed)
    }
}
