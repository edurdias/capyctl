//! A deterministic fake engine implementing [`mllm_adapters::traits::EngineAdapter`].
//!
//! The fake is the executable spec of the behavioral contracts the real F1/F2
//! adapters must honor: slow startup (liveness is not readiness), park levels
//! with distinct memory-retention signatures, ambiguous outcomes (effect
//! applied, ack lost), the deep-park policy gate, and crash injection.

use async_trait::async_trait;
use mllm_adapters::traits::*;
use mllm_adapters::ParkPolicy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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
pub struct FakeEngine {
    lifecycle_clock: Option<Arc<dyn Fn() -> Result<i64, RuntimeError> + Send + Sync>>,
    lifecycle: Mutex<Option<crate::lifecycle::LifecycleState>>,
    knobs: Mutex<Knobs>,
    states: Mutex<HashMap<String, MemberState>>,
    started_at: std::time::Instant,
}

impl std::fmt::Debug for FakeEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeEngine").finish_non_exhaustive()
    }
}

impl FakeEngine {
    pub fn new() -> Self {
        Self {
            lifecycle_clock: None,
            lifecycle: Mutex::new(None),
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

    /// Opt in to the separate persisted lifecycle state machine.
    pub fn with_lifecycle() -> Self {
        let engine = Self::new();
        *engine.lifecycle.lock().unwrap() = Some(crate::lifecycle::LifecycleState::default());
        engine
    }

    /// Service composition may timestamp actual Fake milestones using a trusted
    /// clock. The deterministic fixture constructor preserves arm-time samples.
    pub fn with_lifecycle_clock(
        clock: Arc<dyn Fn() -> Result<i64, RuntimeError> + Send + Sync>,
    ) -> Self {
        let mut engine = Self::with_lifecycle();
        engine.lifecycle_clock = Some(clock);
        engine
    }

    /// Report `members` as the launched group instead of the fabricated pair.
    ///
    /// The coordinator checks that an embedded group is still exactly the
    /// recorded processes after a park or restore (SPEC §13.2), which only a
    /// real, live process can satisfy. Nothing is launched or signalled: the
    /// test owns these processes. Such a Fake also follows the embedded vLLM
    /// residency contract (Park with no prior Drain, a readiness Probe after a
    /// wake); a default Fake keeps refusing a Park the coordinator sends.
    pub fn with_members(self, members: Vec<mllm_domain::completion::ProcessIdentity>) -> Self {
        if let Some(state) = self.lifecycle.lock().unwrap().as_mut() {
            state.members_override = Some(members);
        }
        self
    }

    /// Fault injection for the opt-in lifecycle runtime.
    pub fn with_fault(self, fault: crate::FakeFault) -> Self {
        if let Some(state) = self.lifecycle.lock().unwrap().as_mut() {
            state.fault = Some(fault);
        }
        self
    }

    /// Read-only collector check against actual opt-in Fake membership.
    pub fn lifecycle_members(
        &self,
        context: &mllm_domain::completion::StepExecutionContext,
    ) -> Result<Vec<mllm_domain::completion::ProcessIdentity>, RuntimeError> {
        self.lifecycle
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .members(context)
    }
    /// Executes one authorized Fake cleanup or inspects already terminated owned members.
    pub fn lifecycle_cleanup(
        &self,
        binding: &str,
        incarnation: &str,
        identities: &[mllm_domain::completion::ProcessIdentity],
        terminate: bool,
        observed_at_ms: i64,
    ) -> Result<mllm_domain::completion::CleanupEvidence, RuntimeError> {
        self.lifecycle
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(RuntimeError::Unsupported)?
            .cleanup(binding, incarnation, identities, terminate, observed_at_ms)
    }
    /// Service-owned cleanup samples the configured clock at the actual gone
    /// observation boundary. A clock failure after control remains uncertain.
    pub fn lifecycle_cleanup_observed(
        &self,
        binding: &str,
        incarnation: &str,
        identities: &[mllm_domain::completion::ProcessIdentity],
    ) -> Result<mllm_domain::completion::CleanupEvidence, RuntimeError> {
        self.lifecycle_cleanup_mode_observed(binding, incarnation, identities, true)
    }
    /// Preserve the persisted cleanup mode and sample time after the gone check.
    pub fn lifecycle_cleanup_mode_observed(
        &self,
        binding: &str,
        incarnation: &str,
        identities: &[mllm_domain::completion::ProcessIdentity],
        terminate: bool,
    ) -> Result<mllm_domain::completion::CleanupEvidence, RuntimeError> {
        let clock = self
            .lifecycle_clock
            .as_deref()
            .ok_or(RuntimeError::Unsupported)?;
        self.lifecycle
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(RuntimeError::Unsupported)?
            .cleanup_with_clock(binding, incarnation, identities, terminate, clock)
    }
    pub fn parked_status(
        &self,
        context: &mllm_domain::completion::StepExecutionContext,
    ) -> Result<mllm_domain::completion::ParkedStatusObservation, RuntimeError> {
        self.lifecycle
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .parked_status(context)
    }
    /// Read-only opt-in Fake activity: inference sends, control sends, work started.
    pub fn lifecycle_activity(&self) -> Result<(u64, u64, u64), RuntimeError> {
        Ok(self
            .lifecycle
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .activity())
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
    /// lost (ambiguity injection).
    pub fn set_ambiguous_park(&self) {
        self.knobs.lock().unwrap().ambiguous_park = true;
    }

    /// Sets the deep-park policy gate (default: [`ParkPolicy::Enabled`]).
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
    ) -> Result<mllm_domain::completion::EffectObservation, RuntimeError> {
        self.lifecycle
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(RuntimeError::Unsupported)?
            .execute_with_clock(command, self.lifecycle_clock.as_deref())
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
        // Deep-park security gate: level 2 is refused when the host has disabled it.
        if level == ParkLevel::Two && policy == ParkPolicy::Disabled {
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
        if knobs.policy == ParkPolicy::Disabled {
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
impl mllm_adapters::traits::ChatForward for FakeEngine {
    async fn forward_chat_stream_async(
        &self,
        body: &serde_json::Value,
        sink: &mut dyn mllm_adapters::traits::ChatSink,
    ) -> Result<mllm_adapters::traits::StreamEnded, AdapterError> {
        // Lifecycle fault streams retain their explicit synchronous
        // collector contract. Never buffer that generator to fake async support.
        if self.lifecycle.lock().unwrap().is_some() {
            return Err(AdapterError::UnsupportedCapability);
        }
        let model = body["model"].as_str().unwrap_or("fake");
        for chunk in [
            serde_json::json!({"id":"fake-stream", "model":model,"choices":[{"delta":{"content":"hel"}}]}).to_string(),
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#.to_string(),
        ] {
            if !matches!(tokio::time::timeout(std::time::Duration::from_secs(10), sink.send(chunk)).await, Ok(Ok(()))) {
                break;
            }
        }
        // This ordinary fake generates no external work to reconcile.
        Ok(mllm_adapters::traits::StreamEnded::Completed)
    }
    async fn forward_chat(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        if let Some(state) = self.lifecycle.lock().unwrap().as_mut() {
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
    ) -> Result<mllm_adapters::traits::StreamEnded, AdapterError> {
        if let Some(state) = self.lifecycle.lock().unwrap().as_mut() {
            return state.stream(body, on_chunk);
        }
        let model = body["model"].as_str().unwrap_or("fake").to_string();
        on_chunk(format!(
            r#"{{"id":"fake-stream","model":"{model}","choices":[{{"delta":{{"content":"hel"}}}}]}}"#
        ));
        on_chunk(r#"{"choices":[{"delta":{"content":"lo"}}]}"#.to_string());
        Ok(mllm_adapters::traits::StreamEnded::Completed)
    }
}
