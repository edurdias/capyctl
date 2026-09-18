//! The vLLM adapter: implements the F0 `EngineAdapter` contract over vLLM's
//! OpenAI-compatible HTTP API. Engine-specific endpoints and launch
//! parameters for vLLM live here and only here (SPEC §9).

use async_trait::async_trait;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::policy::ParkPolicy;
use crate::traits::{
    AdapterError, CancellationOutcome, EngineAdapter, EngineState, MemberRef, OwnedProcessLaunch,
    ParkLevel, ParkOutcome, Phase, PlanInput, Quiescence, Readiness, ReloadOutcome, RenderedCommand,
    RequestRef, RestoreOutcome, RuntimeAction, RuntimeCommand, RuntimeError, WorkObservation,
};
use crate::vllm::http::{EngineHttp, HttpError};

/// Bytes the adapter reports as retained after a level-2 park (buffers the
/// engine keeps resident). F1: the live value is replaced by observed
/// engine telemetry during Spark hardware verification (design §8 step 4).
pub const LEVEL2_RETAINED_BYTES: i64 = 256;

/// Deep-park level-2 residue mapping (documented; simulator-tier constant
/// until live hardware verification measures real retention).
pub const fn level2_residue() -> i64 {
    LEVEL2_RETAINED_BYTES
}

/// vLLM adapter for one managed member's API surface.
///
/// One instance is built for one incarnation (Spec §3): the coordinator resolves
/// an adapter per `InitializeWork`, and `claim_incarnation` refuses a second claim
/// on the strength of that. Nothing is shared between launches, and a launch's
/// port, key and plan belong to this instance alone.
///
/// Parked-state observability (F1 design §3): the adapter tracks the last
/// known park state locally per member (`parked` flags keyed by
/// `member.member_id`) and reports `Phase::Parked` from it; `/v1/models`
/// alone never establishes Ready after a park. The controller corroborates
/// via operation provenance. The park state is keyed per member because the
/// legacy F1 `Controller` drives several members through one adapter; member
/// A's park must never make member B report Parked or Initializing. That
/// keying retires with the legacy controller.
pub struct VllmAdapter {
    forward: crate::forward::ChatHttp,
    http: EngineHttp,
    endpoint: String,
    fingerprint: String,
    policy: ParkPolicy,
    model_id: String,
    parked: Mutex<HashMap<String, bool>>,
    /// Launch contract (F1 design §4): the concrete vLLM serve command
    /// this adapter renders for managed launches.
    launch: Option<crate::vllm::args::PlanInputVllm>,
    /// Process tools supplied by the director (Spec §3). The builder spawns,
    /// watches and enumerates through them and never learns where the
    /// identities it produces are recorded.
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    /// The launch this adapter instance owns: the binding and incarnation it
    /// claimed. The claim is taken before a process exists, so a step that failed
    /// mid-launch still holds it and a repeat cannot start a second engine over
    /// the first one's memory. Spec §5 has cleanup take identities from the
    /// execution context, never from adapter memory, so none is kept here.
    launched: Mutex<Option<(String, String)>>,
    /// The per-launch engine credential. It reaches the engine through the
    /// child's environment and appears in no argv, log or receipt (Spec §3).
    engine_key: Option<String>,
}

impl VllmAdapter {
    pub fn new(
        base: reqwest::Url,
        api_key: Option<String>,
        fingerprint: String,
        policy: ParkPolicy,
        model_id: String,
    ) -> Self {
        Self {
            forward: crate::forward::ChatHttp::new(base.clone(), model_id.clone(), api_key.clone()),
            endpoint: base.to_string(),
            http: EngineHttp::new(base, api_key),
            fingerprint,
            policy,
            model_id,
            parked: Mutex::new(HashMap::new()),
            launch: None,
            tools: None,
            launched: Mutex::new(None),
            engine_key: None,
        }
    }

    /// Attach the managed-launch contract (F1 design §4): the concrete
    /// serve command this adapter renders in `render_plan`.
    pub fn with_launch(mut self, launch: crate::vllm::args::PlanInputVllm) -> Self {
        self.launch = Some(launch);
        self
    }

    /// Attach the process tools the director supplies for an owned launch
    /// (Spec §3). Without them the adapter answers Initialize with
    /// `Unsupported`: it has no way to spawn anything it could prove it owns.
    pub fn with_tools(mut self, tools: Arc<dyn OwnedProcessLaunch>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Attach the per-launch engine credential (Spec §3). It is delivered to
    /// the child through the environment only, and it is the credential this
    /// adapter then presents on every request of its own: the engine guards
    /// `/v1` with exactly this key, so a readiness probe or an inference probe
    /// sent without it is refused, never merely slow (SPEC §6.1).
    pub fn with_engine_key(mut self, engine_key: String) -> Self {
        let base = reqwest::Url::parse(&self.endpoint)
            .expect("the endpoint this adapter was built from is a URL");
        self.forward = crate::forward::ChatHttp::new(
            base.clone(),
            self.model_id.clone(),
            Some(engine_key.clone()),
        );
        self.http = EngineHttp::new(base, Some(engine_key.clone()));
        self.engine_key = Some(engine_key);
        self
    }

    /// The engine's build fingerprint, for the receipt an Initialize records.
    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The engine endpoint this adapter talks to, for the same receipt.
    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The three things an owned launch needs. Any one missing makes the step
    /// unsupported rather than partly performed.
    pub(super) fn launch_parts(
        &self,
    ) -> Result<
        (
            crate::vllm::args::PlanInputVllm,
            Arc<dyn OwnedProcessLaunch>,
            String,
        ),
        RuntimeError,
    > {
        match (&self.launch, &self.tools, &self.engine_key) {
            (Some(launch), Some(tools), Some(key)) => {
                Ok((launch.clone(), tools.clone(), key.clone()))
            }
            _ => Err(RuntimeError::Unsupported),
        }
    }

    /// Claim this adapter's one launch. A second claim is refused: the adapter
    /// instance is built for a single incarnation, and a repeat would spawn a
    /// second engine while the first is still recorded as owned.
    pub(super) fn claim_incarnation(
        &self,
        binding_id: &str,
        incarnation: &str,
    ) -> Result<(), RuntimeError> {
        let mut launched = self
            .launched
            .lock()
            .map_err(|_| RuntimeError::Uncertain("launch record is poisoned".into()))?;
        if launched.is_some() {
            return Err(RuntimeError::Unsupported);
        }
        *launched = Some((binding_id.to_string(), incarnation.to_string()));
        Ok(())
    }

    fn require_policy(&self, _what: &str) -> Result<(), AdapterError> {
        // Deep-park security gate (SPEC §9.1 / T21): the profile-level
        // opt-in is required for every sleep/collective operation.
        match self.policy {
            ParkPolicy::Disabled => Err(AdapterError::PolicyDenied),
            ParkPolicy::Enabled => Ok(()),
        }
    }

    fn set_parked(&self, member: &MemberRef, v: bool) {
        self.parked
            .lock()
            .unwrap()
            .insert(member.member_id.clone(), v);
    }

    fn is_parked(&self, member: &MemberRef) -> bool {
        self.parked
            .lock()
            .unwrap()
            .get(&member.member_id)
            .copied()
            .unwrap_or(false)
    }

    fn uncertain_http(what: &str, e: HttpError) -> AdapterError {
        match e {
            HttpError::Uncertain(detail) => AdapterError::Uncertain(format!("{what}: {detail}")),
            other => AdapterError::Uncertain(format!("{what}: {other}")),
        }
    }
}

#[async_trait]
impl EngineAdapter for VllmAdapter {
    /// Spec §4: the builder performs Initialize end to end. Every other action
    /// stays refused until the slice that implements it lands — an adapter must
    /// never appear to grant a control path it does not have.
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<mllm_domain::completion::EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => crate::vllm::initialize::initialize(self, command).await,
            _ => Err(RuntimeError::Unsupported),
        }
    }

    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError> {
        // /v1/models presence tells us the API server is up and serving the
        // model id — but after a park the server stays up (F1 design §3),
        // so the local park tracking decides the phase.
        let serving = self.http.list_models().await.map_err(|e| {
            if matches!(e, HttpError::Unreachable(_)) {
                // The engine process is starting: its HTTP surface listens
                // only after weights are staged (minutes on GB10). Startup
                // non-listening is NOT a crash — the launcher owns crash
                // detection via process exit; readiness polls continue.
                AdapterError::Uncertain("engine not listening yet".into())
            } else {
                Self::uncertain_http("inspect", e)
            }
        })?;
        let phase = if self.is_parked(member) {
            Phase::Parked
        } else if serving.contains(&self.model_id) {
            Phase::Ready
        } else {
            Phase::Startup
        };
        Ok(EngineState {
            phase,
            retained_bytes: if self.is_parked(member) {
                level2_residue()
            } else {
                4096
            },
            build_fingerprint: Some(self.fingerprint.clone()),
        })
    }

    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        let Some(spec) = &self.launch else {
            return Err(AdapterError::UnsupportedCapability);
        };
        let mut spec = spec.clone();

        spec.api_key = None; // launch secret rides the env, not argv (redaction)
        let mut cmd = crate::vllm::args::render_command(&spec)
            .map_err(|e| AdapterError::Uncertain(format!("render: {e}")))?;
        // The engine credential is delivered via environment (never argv),
        // redacted from fingerprints and journals (SPEC §8.2/§13.3).
        //
        // Spec §3 retires this name. The only caller left is the legacy F1
        // `Controller`, which the coordinator replaces; a native launch reaches
        // the engine through `initialize`, which sets `VLLM_API_KEY` instead and
        // never comes through here. The branch retires with that controller at S5,
        // and is left alone until then rather than changing behaviour nothing in
        // this slice exercises.
        if let Some(key) = &plan.engine_api_key {
            cmd.env.insert("MLLM_ENGINE_API_KEY".into(), key.clone());
        }
        // The engine's runtime PATH carries its own venv bin (the JIT
        // compile step needs the venv's tools, e.g. ninja).
        if let Some(extra) = &spec.engine_path_extra {
            let sys = std::env::var("PATH").unwrap_or_default();
            cmd.env.insert("PATH".into(), format!("{extra}:{sys}"));
        }
        if let Some(log) = &spec.engine_log {
            // One log per deployment member: concurrent/repeat launches
            // must not truncate each other's evidence, but a shared append
            // target for the same member keeps the runbook readable.
            cmd.env.insert(
                "MLLM_ENGINE_LOG".into(),
                format!("{log}.{}", plan.member_id),
            );
        }
        Ok(cmd)
    }

    async fn check_readiness(&self, member: &MemberRef) -> Result<Readiness, AdapterError> {
        // SPEC §6.1: liveness of an HTTP server is not model readiness.
        // A parked engine is never Ready (parked-state observability) —
        // keyed by THIS member: another deployment's park must not make
        // this member read Initializing.
        if self.is_parked(member) {
            return Ok(Readiness::Initializing);
        }
        let ids = match self.http.list_models().await {
            Ok(ids) => ids,
            Err(HttpError::Unreachable(_)) => {
                // Not listening yet: Initializing (the readiness loop
                // polls); crash detection is the launcher's job.
                return Ok(Readiness::Initializing);
            }
            // The stage, not the step: the readiness loop adds "readiness" itself,
            // and naming the same thing twice in one reason helps nobody.
            Err(e) => return Err(Self::uncertain_http("model list", e)),
        };
        if ids.contains(&self.model_id) {
            Ok(Readiness::Ready)
        } else {
            Ok(Readiness::Initializing)
        }
    }

    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError> {
        // Quiescence = what the adapter can prove: no live work observed.
        match self.observe_work(member).await? {
            WorkObservation::Idle => Ok(Quiescence { quiescent: true }),
            WorkObservation::Streaming { .. } => Ok(Quiescence { quiescent: false }),
            WorkObservation::Unknown => Ok(Quiescence { quiescent: false }),
        }
    }

    async fn park(
        &self,
        member: &MemberRef,
        level: ParkLevel,
    ) -> Result<ParkOutcome, AdapterError> {
        // Deep-park security gate (SPEC §9.1 / T21): both sleep levels on the
        // vllm-sleep profile require the host-policy opt-in — the profile
        // itself is gated, not just the operations (design §7).
        self.require_policy("park")?;
        let outcome = self
            .http
            .sleep(match level {
                ParkLevel::One => 1,
                ParkLevel::Two => 2,
            })
            .await
            .map_err(|e| Self::uncertain_http("park", e))?;
        match outcome {
            crate::vllm::SleepOutcome::Applied => {
                self.set_parked(member, true);
                Ok(ParkOutcome::Parked {
                    retained_bytes: level2_residue(),
                })
            }
        }
    }

    async fn restore(&self, member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        // Waking allocations alone is not successful restoration (SPEC §9.1):
        // wake, then reload weights through the collective, then the caller
        // verifies readiness + generation.
        self.http
            .wake()
            .await
            .map_err(|e| Self::uncertain_http("restore wake", e))?;
        self.reload_weights(member).await?;
        // Discarded KV allocations must never be reused through stale prefix
        // metadata. Require invalidation after reload, before releasing Ready.
        self.http
            .reset_prefix_cache()
            .await
            .map_err(|e| Self::uncertain_http("restore cache reset", e))?;
        self.set_parked(member, false);
        Ok(RestoreOutcome::Restored)
    }

    async fn reload_weights(&self, _member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        // Collective control, invoked once through the lead (SPEC §11):
        // gated by the same policy as park.
        self.require_policy("reload_weights")?;
        self.http
            .collective_rpc()
            .await
            .map_err(|e| Self::uncertain_http("reload_weights", e))?;
        Ok(ReloadOutcome::Reloaded)
    }

    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        // vLLM exposes no per-request in-flight HTTP surface in the pinned
        // API; accounting rides on stream correlation in the router. The
        // adapter reports what it can prove: nothing observable → Unknown
        // unless parked (a parked engine is provably idle).
        if self.is_parked(member) {
            return Ok(WorkObservation::Idle);
        }
        Ok(WorkObservation::Unknown)
    }

    async fn cancel_work(
        &self,
        _member: &MemberRef,
        _req: &RequestRef,
        require_ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        // vLLM exposes no cancellation-ack API (F1 design §3): the adapter
        // reports uncertainty unless a stream closed cleanly (router-side).
        let _ = require_ack;
        Ok(CancellationOutcome::Uncertain)
    }
}
#[async_trait]
impl crate::traits::ChatForward for VllmAdapter {
    async fn forward_chat_stream_async(
        &self,
        body: &serde_json::Value,
        sink: &mut dyn crate::traits::ChatSink,
    ) -> Result<crate::traits::StreamEnded, AdapterError> {
        self.forward.stream_async(body, sink).await
    }
    async fn forward_chat(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        self.forward.collect(body).await
    }
    async fn forward_chat_stream(
        &self,
        body: &serde_json::Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<crate::traits::StreamEnded, AdapterError> {
        self.forward.stream(body, on_chunk).await
    }
}
