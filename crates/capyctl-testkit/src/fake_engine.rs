//! A deterministic fake engine implementing [`capyctl_adapters::traits::EngineAdapter`].
//!
//! The fake is the executable spec of the behavioral contracts the real F1/F2
//! adapters must honor: slow startup (liveness is not readiness), park levels
//! with distinct memory-retention signatures, ambiguous outcomes (effect
//! applied, ack lost), the deep-park policy gate, and crash injection.
//!
//! ADR 0030: [`FakeEngine::with_decisions`] makes it a decision model that
//! answers `/v1/systemone` (as scripted: well formed, malformed, another
//! pick or other probabilities) and refuses chat, so CPU tests cover
//! readiness, re-proof and the wake canary of a decisions deployment.
//!
//! ADR 0036: [`FakeEngine::with_embeddings`] makes it an embedding model
//! that answers `/v1/embeddings` with set vectors, streamed to the sink in
//! chunks, well formed or broken as a test scripts it ([`FakeEmbeddings`]),
//! and refuses chat and decisions.

use async_trait::async_trait;
use capyctl_adapters::traits::*;
use capyctl_adapters::ParkPolicy;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// SPEC §10 (amended 2026-10-01): the engine-wide counters read busy.
    engine_busy: bool,
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
    /// ADR 0030: the `/v1/systemone` mode. `Some` is a decision model, whose
    /// every `/v1/systemone` request is answered with this engine response
    /// (well formed or not, as a test scripts it) and whose chat requests
    /// are refused; `None` is a chat model.
    decisions: Mutex<Option<serde_json::Value>>,
    /// Chat requests and `/v1/systemone` requests this engine was sent.
    chat_requests: AtomicU64,
    decision_requests: AtomicU64,
    /// ADR 0036: the `/v1/embeddings` mode. `Some` is an embedding model,
    /// answering every embeddings request as scripted and refusing chat and
    /// decisions; `None` has no such endpoint.
    embeddings: Mutex<Option<FakeEmbeddings>>,
    embedding_requests: AtomicU64,
}

/// ADR 0036 plan item 7: how a broken embeddings answer is broken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddingFault {
    /// One `data` entry more than the request had inputs.
    WrongCount,
    /// The last entry's vector is one value longer than the others (on a
    /// single input, that vector alone, so the answer still parses).
    Ragged,
    /// The answer stops halfway: the first half reaches the sink, then the
    /// connection ends.
    CutShort,
}

/// ADR 0036 plan item 7: the script of an embedding Fake.
#[derive(Clone, Debug, PartialEq)]
pub struct FakeEmbeddings {
    /// Every input's vector, before any `dimensions` truncation.
    pub vector: Vec<f32>,
    /// `model` written after `data` rather than before it.
    pub model_last: bool,
    /// The size of each chunk handed to the sink, in bytes.
    pub chunk_bytes: usize,
    pub fault: Option<EmbeddingFault>,
}

impl Default for FakeEmbeddings {
    fn default() -> Self {
        Self {
            vector: vec![0.6, 0.8, 0.0, 0.0],
            model_last: false,
            chunk_bytes: 16,
            fault: None,
        }
    }
}

impl FakeEmbeddings {
    /// The engine's whole answer to `body`, as bytes: `model` named back as
    /// asked, one entry per input in the asked encoding, broken as scripted.
    pub fn answer(&self, body: &serde_json::Value) -> Vec<u8> {
        let inputs = match &body["input"] {
            serde_json::Value::Array(items) if !items[0].is_number() => items.len(),
            _ => 1,
        };
        let entries = match self.fault {
            Some(EmbeddingFault::WrongCount) => inputs + 1,
            _ => inputs,
        };
        let base64 = body["encoding_format"] == "base64";
        let width = body["dimensions"]
            .as_u64()
            .map_or(self.vector.len(), |d| (d as usize).min(self.vector.len()));
        let model = serde_json::to_string(&body["model"]).unwrap();
        let mut data = Vec::with_capacity(entries);
        for index in 0..entries {
            let mut vector = self.vector[..width].to_vec();
            if self.fault == Some(EmbeddingFault::Ragged) && index + 1 == entries {
                vector.push(0.5);
            }
            let embedding = if base64 {
                let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
                format!("\"{}\"", base64_encode(&bytes))
            } else {
                serde_json::to_string(&vector).unwrap()
            };
            data.push(format!(
                r#"{{"object":"embedding","index":{index},"embedding":{embedding}}}"#
            ));
        }
        let data = format!(r#""data":[{}]"#, data.join(","));
        let tokens = 8 * inputs;
        let usage = format!(r#""usage":{{"prompt_tokens":{tokens},"total_tokens":{tokens}}}"#);
        let mut answer = if self.model_last {
            format!(r#"{{"object":"list",{data},"model":{model},{usage}}}"#)
        } else {
            format!(r#"{{"object":"list","model":{model},{data},{usage}}}"#)
        }
        .into_bytes();
        if self.fault == Some(EmbeddingFault::CutShort) {
            answer.truncate(answer.len() / 2);
        }
        answer
    }
}

/// Standard base64 with padding, as engines encode `base64` vectors.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let n = group
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= group.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
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
                engine_busy: false,
            }),
            states: Mutex::new(HashMap::new()),
            started_at: std::time::Instant::now(),
            decisions: Mutex::new(None),
            chat_requests: AtomicU64::new(0),
            decision_requests: AtomicU64::new(0),
            embeddings: Mutex::new(None),
            embedding_requests: AtomicU64::new(0),
        }
    }

    /// ADR 0030: a decision model. Every `/v1/systemone` request is answered
    /// with a well-formed answer to the fixed probe question (billing picked
    /// at 0.95); chat requests are refused, so readiness, the wake canary
    /// and re-proofs can only pass on decisions.
    pub fn with_decisions(self) -> Self {
        self.set_decision(Self::decision_answer("billing", [0.95, 0.02, 0.03]));
        self
    }

    /// ADR 0030: the response the next `/v1/systemone` requests get, raw as
    /// an engine sends it: [`Self::decision_answer`], or anything malformed.
    pub fn set_decision(&self, response: serde_json::Value) {
        *self.decisions.lock().unwrap() = Some(response);
    }

    /// An engine's answer to the fixed decision probe
    /// (`capyctl_adapters::decision_probe`) picking `choice`, with the
    /// billing, shipping and technical probabilities in that order.
    pub fn decision_answer(choice: &str, probabilities: [f64; 3]) -> serde_json::Value {
        use capyctl_adapters::decision_probe::{OPTIONS, QUESTION};
        let probabilities: serde_json::Map<String, serde_json::Value> = OPTIONS
            .iter()
            .zip(probabilities)
            .map(|((option, _), p)| ((*option).to_owned(), serde_json::json!(p)))
            .collect();
        serde_json::json!({
            "model": "fake",
            "answers": {QUESTION: {"type": "choice", "choice": choice,
                "probabilities": probabilities, "confidence": 0.9}},
            "usage": {"input_tokens": 32, "output_tokens": 0}
        })
    }

    /// How many chat completions this engine was sent.
    pub fn chat_requests(&self) -> u64 {
        self.chat_requests.load(Ordering::SeqCst)
    }

    /// How many `/v1/systemone` requests this engine was sent.
    pub fn decision_requests(&self) -> u64 {
        self.decision_requests.load(Ordering::SeqCst)
    }

    /// ADR 0036: an embedding model answering with
    /// [`FakeEmbeddings::default`]; chat and decisions are refused, so
    /// readiness, the wake canary and re-proofs can only pass on embeddings.
    pub fn with_embeddings(self) -> Self {
        self.set_embeddings(FakeEmbeddings::default());
        self
    }

    /// ADR 0036: the script the next `/v1/embeddings` requests are answered by.
    pub fn set_embeddings(&self, script: FakeEmbeddings) {
        *self.embeddings.lock().unwrap() = Some(script);
    }

    /// How many `/v1/embeddings` requests (probes included) this engine was sent.
    pub fn embedding_requests(&self) -> u64 {
        self.embedding_requests.load(Ordering::SeqCst)
    }

    /// ADR 0036: the engine's whole answer to one embeddings request, counted.
    /// A model in another mode has no such endpoint.
    fn embedding_response(&self, body: &serde_json::Value) -> Result<Vec<u8>, AdapterError> {
        self.embedding_requests.fetch_add(1, Ordering::SeqCst);
        let script = self.embeddings.lock().unwrap().clone();
        Ok(script
            .ok_or(AdapterError::UnsupportedCapability)?
            .answer(body))
    }

    /// ADR 0030: the engine's answer to one `/v1/systemone` request, `model`
    /// named back as asked. A chat model has no such endpoint.
    pub fn decision_response(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        self.decision_requests.fetch_add(1, Ordering::SeqCst);
        let mut response = self
            .decisions
            .lock()
            .unwrap()
            .clone()
            .ok_or(AdapterError::UnsupportedCapability)?;
        if response.get("model").is_some() {
            response["model"] = body["model"].clone();
        }
        Ok(response)
    }

    /// A chat request: counted, and refused by a decision or embedding model.
    fn chat(&self) -> Result<(), AdapterError> {
        self.chat_requests.fetch_add(1, Ordering::SeqCst);
        if self.decisions.lock().unwrap().is_some() || self.embeddings.lock().unwrap().is_some() {
            return Err(AdapterError::PolicyDenied);
        }
        Ok(())
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
    pub fn with_members(self, members: Vec<capyctl_domain::completion::ProcessIdentity>) -> Self {
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
        context: &capyctl_domain::completion::StepExecutionContext,
    ) -> Result<Vec<capyctl_domain::completion::ProcessIdentity>, RuntimeError> {
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
        identities: &[capyctl_domain::completion::ProcessIdentity],
        terminate: bool,
        observed_at_ms: i64,
    ) -> Result<capyctl_domain::completion::CleanupEvidence, RuntimeError> {
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
        identities: &[capyctl_domain::completion::ProcessIdentity],
    ) -> Result<capyctl_domain::completion::CleanupEvidence, RuntimeError> {
        self.lifecycle_cleanup_mode_observed(binding, incarnation, identities, true)
    }
    /// Preserve the persisted cleanup mode and sample time after the gone check.
    pub fn lifecycle_cleanup_mode_observed(
        &self,
        binding: &str,
        incarnation: &str,
        identities: &[capyctl_domain::completion::ProcessIdentity],
        terminate: bool,
    ) -> Result<capyctl_domain::completion::CleanupEvidence, RuntimeError> {
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
        context: &capyctl_domain::completion::StepExecutionContext,
    ) -> Result<capyctl_domain::completion::ParkedStatusObservation, RuntimeError> {
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

    /// Whether the engine's own counters read work (default: idle), as
    /// [`EngineAdapter::engine_quiescent`] reports it.
    pub fn set_engine_busy(&self, busy: bool) {
        self.knobs.lock().unwrap().engine_busy = busy;
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
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        let decisions = self.decisions.lock().unwrap().clone();
        // ADR 0036 §5: an embedding model's readiness is the fixed embedding
        // probe; its answer is fixed by the script, so it is drawn here.
        let embeddings = self
            .embeddings
            .lock()
            .unwrap()
            .clone()
            .map(|script| script.answer(&capyctl_adapters::embedding_probe::request("fake")));
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let state = lifecycle.as_mut().ok_or(RuntimeError::Unsupported)?;
        // ADR 0030 §3: a decision model's readiness is its fixed decision.
        state.decisions = decisions;
        state.embeddings = embeddings;
        let result = state.execute_with_clock(command, self.lifecycle_clock.as_deref());
        self.decision_requests
            .fetch_add(std::mem::take(&mut state.decision_probes), Ordering::SeqCst);
        self.embedding_requests.fetch_add(
            std::mem::take(&mut state.embedding_probes),
            Ordering::SeqCst,
        );
        result
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
                "CAPYCTL_PARK_LEVEL".to_string(),
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
        Ok(Quiescence {
            quiescent: true,
            unproven: None,
        })
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

    async fn engine_quiescent(&self, _member: &MemberRef, _after_ms: i64) -> bool {
        !self.knobs.lock().unwrap().engine_busy
    }
    /// ADR 0030 §4: a decision model's wake canary is its fixed decision
    /// probe; ADR 0036 §6: an embedding model's is its fixed embedding probe;
    /// a chat Fake has no canary.
    async fn wake_canary(
        &self,
        context: &capyctl_domain::completion::StepExecutionContext,
        _max_tokens: u32,
        bound: Duration,
    ) -> Result<capyctl_adapters::completion_probe::CanaryAnswer, RuntimeError> {
        use capyctl_adapters::completion_probe::CanaryEvidence;
        let answer = if self.embeddings.lock().unwrap().is_some() {
            CanaryEvidence::Embedding(
                ChatForward::embedding_probe(self, "fake", bound)
                    .await
                    .map_err(|e| RuntimeError::Uncertain(format!("Fake embedding probe: {e}")))?,
            )
        } else if self.decisions.lock().unwrap().is_some() {
            CanaryEvidence::Decision(
                ChatForward::decision_probe(self, "fake", bound)
                    .await
                    .map_err(|e| RuntimeError::Uncertain(format!("Fake decision probe: {e}")))?,
            )
        } else {
            return Err(RuntimeError::Unsupported);
        };
        Ok(capyctl_adapters::completion_probe::CanaryAnswer {
            answer,
            observed_at_ms: match self.lifecycle_clock.as_deref() {
                Some(clock) => clock()?,
                None => context.issued_at_ms,
            },
        })
    }
}

#[async_trait]
impl capyctl_adapters::traits::ChatForward for FakeEngine {
    async fn forward_chat_stream_async(
        &self,
        body: &serde_json::Value,
        sink: &mut dyn capyctl_adapters::traits::ChatSink,
    ) -> Result<capyctl_adapters::traits::StreamEnded, AdapterError> {
        self.chat()?;
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
                return Ok(capyctl_adapters::traits::StreamEnded::Cancelled);
            }
        }
        // This ordinary fake generates no external work to reconcile.
        Ok(capyctl_adapters::traits::StreamEnded::Completed)
    }
    /// ADR 0030 §2: the public `/v1/systemone` route's forwarding, answered
    /// as the engine would (a chat model has no such endpoint).
    async fn forward_decision(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        self.decision_response(body)
    }
    async fn forward_chat(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        self.chat()?;
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
    ) -> Result<capyctl_adapters::traits::StreamEnded, AdapterError> {
        self.chat()?;
        if let Some(state) = self.lifecycle.lock().unwrap().as_mut() {
            return state.stream(body, on_chunk);
        }
        let model = body["model"].as_str().unwrap_or("fake").to_string();
        on_chunk(format!(
            r#"{{"id":"fake-stream","model":"{model}","choices":[{{"delta":{{"content":"hel"}}}}]}}"#
        ));
        on_chunk(r#"{"choices":[{"delta":{"content":"lo"}}]}"#.to_string());
        Ok(capyctl_adapters::traits::StreamEnded::Completed)
    }

    /// ADR 0030 §3: the fixed decision probe, judged by the same validator
    /// the real adapters use.
    async fn decision_probe(
        &self,
        served: &str,
        _bound: Duration,
    ) -> Result<capyctl_adapters::decision_probe::DecisionAnswer, AdapterError> {
        let response =
            self.decision_response(&capyctl_adapters::decision_probe::request(served))?;
        capyctl_adapters::decision_probe::answer(&response)
    }

    /// ADR 0036 §3-§4: the request is held to its closed contract, then the
    /// scripted answer is streamed to `sink` in chunks, all but its closing
    /// brace, and judged by the same checks the real forwarder applies. A
    /// client gone stops the relay, never the reading (owner decision
    /// 2026-10-27, question 7).
    async fn forward_embedding(
        &self,
        body: &serde_json::Value,
        sink: &mut dyn capyctl_adapters::traits::EmbeddingSink,
    ) -> Result<capyctl_adapters::traits::EmbeddingAnswered, AdapterError> {
        capyctl_adapters::embedding_request::validate_embedding_request(body)?;
        let chunk_bytes = match self.embeddings.lock().unwrap().as_ref() {
            Some(script) => script.chunk_bytes.max(1),
            None => return Err(AdapterError::UnsupportedCapability),
        };
        let answer = self.embedding_response(body)?;
        let relayed = answer.strip_suffix(b"}").unwrap_or(&answer);
        let mut client_gone = false;
        for chunk in relayed.chunks(chunk_bytes) {
            sink.progressed();
            if !client_gone && sink.send(chunk.to_vec()).await.is_err() {
                client_gone = true;
            }
        }
        let mut answered =
            capyctl_adapters::embedding_request::embedding_answered(body, &answer)
                .ok_or_else(|| AdapterError::Uncertain("embeddings answer unverified".into()))?;
        answered.client_gone = client_gone;
        Ok(answered)
    }

    /// ADR 0036 §5: the fixed embedding probe, judged by the same validator
    /// the real adapters use.
    async fn embedding_probe(
        &self,
        served: &str,
        _bound: Duration,
    ) -> Result<capyctl_adapters::embedding_probe::EmbeddingAnswer, AdapterError> {
        let answer =
            self.embedding_response(&capyctl_adapters::embedding_probe::request(served))?;
        let response = serde_json::from_slice(&answer).map_err(|_| {
            AdapterError::Uncertain("the embedding probe answered no complete JSON".into())
        })?;
        capyctl_adapters::embedding_probe::answer(&response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_adapters::completion_probe::CanaryEvidence;
    use capyctl_adapters::traits::{ChatForward, DeliveryFailed, EmbeddingSink};

    /// A sink that keeps every chunk, or refuses them all once `gone`.
    #[derive(Default)]
    struct Kept {
        chunks: Vec<Vec<u8>>,
        progressed: usize,
        gone: bool,
    }

    #[async_trait]
    impl EmbeddingSink for Kept {
        async fn send(&mut self, bytes: Vec<u8>) -> Result<(), DeliveryFailed> {
            if self.gone {
                return Err(DeliveryFailed);
            }
            self.chunks.push(bytes);
            Ok(())
        }
        fn progressed(&mut self) {
            self.progressed += 1;
        }
    }

    fn request(extra: serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({"model": "public", "input": ["one", "two"]});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    }

    /// ADR 0036 plan item 7 (T47): a well-formed answer streams to the sink
    /// in chunks, all but its closing brace, `model` before or after `data`.
    #[tokio::test]
    async fn an_embedding_fake_streams_its_set_vectors_in_chunks() {
        for model_last in [false, true] {
            let engine = FakeEngine::new();
            engine.set_embeddings(FakeEmbeddings {
                model_last,
                ..Default::default()
            });
            let mut sink = Kept::default();
            let answered = engine
                .forward_embedding(&request(serde_json::json!({})), &mut sink)
                .await
                .unwrap();
            assert_eq!(answered.inputs, 2);
            assert_eq!(answered.prompt_tokens, Some(16));
            assert!(!answered.client_gone);
            assert!(sink.chunks.len() > 1, "streamed in chunks");
            assert_eq!(sink.progressed, sink.chunks.len());
            let mut whole = sink.chunks.concat();
            whole.push(b'}');
            let text = String::from_utf8(whole.clone()).unwrap();
            assert_eq!(
                text.find("\"model\"") > text.find("\"data\""),
                model_last,
                "{text}"
            );
            let whole: serde_json::Value = serde_json::from_slice(&whole).unwrap();
            assert_eq!(whole["model"], "public");
            assert_eq!(
                whole["data"][1]["embedding"],
                serde_json::json!([0.6, 0.8, 0.0, 0.0])
            );
        }
        let engine = FakeEngine::new().with_embeddings();
        for extra in [
            serde_json::json!({"dimensions": 2}),
            serde_json::json!({"encoding_format": "base64"}),
            serde_json::json!({"input": [1, 2, 3]}),
        ] {
            engine
                .forward_embedding(&request(extra.clone()), &mut Kept::default())
                .await
                .unwrap_or_else(|e| panic!("{extra}: {e:?}"));
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    /// ADR 0036 §4 (T47): a broken answer is uncertain, whatever reached
    /// the sink before it was found broken.
    #[tokio::test]
    async fn a_broken_embedding_answer_is_uncertain() {
        for fault in [
            EmbeddingFault::WrongCount,
            EmbeddingFault::Ragged,
            EmbeddingFault::CutShort,
        ] {
            let engine = FakeEngine::new();
            engine.set_embeddings(FakeEmbeddings {
                fault: Some(fault),
                ..Default::default()
            });
            let mut sink = Kept::default();
            let result = engine
                .forward_embedding(&request(serde_json::json!({})), &mut sink)
                .await;
            assert!(
                matches!(result, Err(AdapterError::Uncertain(_))),
                "{fault:?}: {result:?}"
            );
            assert!(!sink.chunks.is_empty(), "{fault:?}");
        }
    }

    /// ADR 0036 §4 (owner decision 2026-10-27, question 7, T47): a client
    /// gone stops the relay, never the reading or the checks.
    #[tokio::test]
    async fn a_client_gone_still_reads_the_answer_to_its_end() {
        let engine = FakeEngine::new().with_embeddings();
        let mut sink = Kept {
            gone: true,
            ..Default::default()
        };
        let answered = engine
            .forward_embedding(&request(serde_json::json!({})), &mut sink)
            .await
            .unwrap();
        assert!(answered.client_gone);
        assert!(sink.chunks.is_empty());
        assert!(sink.progressed > 1);
    }

    /// ADR 0036 §3, §5 (T47): an embedding Fake refuses chat, decisions and
    /// a request outside the closed contract (sending nothing); a chat Fake
    /// has no embeddings endpoint.
    #[tokio::test]
    async fn an_embedding_fake_answers_embeddings_alone() {
        let engine = FakeEngine::new().with_embeddings();
        let chat = serde_json::json!({"model": "public", "messages": []});
        assert!(matches!(
            engine.forward_chat(&chat).await,
            Err(AdapterError::PolicyDenied)
        ));
        assert!(engine.forward_decision(&chat).await.is_err());
        assert!(engine
            .decision_probe("fake", Duration::from_secs(1))
            .await
            .is_err());
        assert!(engine
            .forward_embedding(
                &request(serde_json::json!({"stream": true})),
                &mut Kept::default()
            )
            .await
            .is_err());
        assert_eq!(engine.embedding_requests(), 0);
        let plain = FakeEngine::new();
        assert!(matches!(
            plain
                .forward_embedding(&request(serde_json::json!({})), &mut Kept::default())
                .await,
            Err(AdapterError::UnsupportedCapability)
        ));
        assert!(plain
            .embedding_probe("fake", Duration::from_secs(1))
            .await
            .is_err());
    }

    /// ADR 0036 §5-§6 (T47): the fixed probe and the wake canary answer one
    /// vector of the native dimension; a broken probe answer is uncertain.
    #[tokio::test]
    async fn the_embedding_probe_and_wake_canary_carry_the_set_vector() {
        let engine = FakeEngine::new().with_embeddings();
        let answer = engine
            .embedding_probe("served", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(answer.dimension(), 4);
        let context = capyctl_domain::completion::StepExecutionContext {
            token: capyctl_domain::completion::TransitionToken {
                deployment_id: "deployment".into(),
                revision: 1,
                generation: 1,
                operation_id: "operation".into(),
                step_id: "wake".into(),
            },
            binding_id: "binding".into(),
            incarnation: "incarnation".into(),
            issued_at_ms: 1200,
            deadline_ms: 400000,
            identities: capyctl_domain::completion::ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: None,
            launch_settings: None,
        };
        let canary = engine
            .wake_canary(&context, 16, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(canary.answer, CanaryEvidence::Embedding(answer));
        assert_eq!(engine.embedding_requests(), 2);
        assert_eq!(engine.chat_requests(), 0);
        for fault in [EmbeddingFault::WrongCount, EmbeddingFault::CutShort] {
            engine.set_embeddings(FakeEmbeddings {
                fault: Some(fault),
                ..Default::default()
            });
            assert!(matches!(
                engine
                    .embedding_probe("served", Duration::from_secs(1))
                    .await,
                Err(AdapterError::Uncertain(_))
            ));
            assert!(matches!(
                engine
                    .wake_canary(&context, 16, Duration::from_secs(1))
                    .await,
                Err(RuntimeError::Uncertain(_))
            ));
        }
    }
}
