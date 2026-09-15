//! The vLLM adapter: implements the F0 `EngineAdapter` contract over vLLM's
//! OpenAI-compatible HTTP API. Engine-specific endpoints and launch
//! parameters for vLLM live here and only here (SPEC §9).

use async_trait::async_trait;

use std::collections::HashMap;
use std::sync::Mutex;

use crate::fake::ParkPolicy;
use crate::traits::{
    AdapterError, CancellationOutcome, EngineAdapter, EngineState, MemberRef, ParkLevel,
    ParkOutcome, Phase, PlanInput, Quiescence, Readiness, ReloadOutcome, RenderedCommand,
    RequestRef, RestoreOutcome, WorkObservation,
};
use crate::vllm::http::{EngineHttp, HttpError};

/// Bytes the adapter reports as retained after a level-2 park (buffers the
/// engine keeps resident). F1: the live value is replaced by observed
/// engine telemetry during Spark qualification (design §8 step 4).
pub const LEVEL2_RETAINED_BYTES: i64 = 256;

/// Deep-park level-2 residue mapping (documented; simulator-tier constant
/// until live qualification measures real retention).
pub const fn level2_residue() -> i64 {
    LEVEL2_RETAINED_BYTES
}

/// vLLM adapter for one managed member's API surface.
///
/// Parked-state observability (F1 design §3): the adapter tracks the last
/// known park state locally per member (`parked` flags keyed by
/// `member.member_id`) and reports `Phase::Parked` from it; `/v1/models`
/// alone never establishes Ready after a park. The controller corroborates
/// via operation provenance. The park state is PER MEMBER (mirroring the
/// fake's per-member state): the adapter is a per-profile singleton shared
/// by deployments riding the same profile — member A's park must never
/// make member B report Parked or Initializing.
pub struct VllmAdapter {
    forward: crate::forward::ChatHttp,
    http: EngineHttp,
    fingerprint: String,
    policy: ParkPolicy,
    model_id: String,
    parked: Mutex<HashMap<String, bool>>,
    /// Launch contract (F1 design §4): the concrete vLLM serve command
    /// this adapter renders for managed launches.
    launch: Option<crate::vllm::args::PlanInputVllm>,
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
            http: EngineHttp::new(base, api_key),
            fingerprint,
            policy,
            model_id,
            parked: Mutex::new(HashMap::new()),
            launch: None,
        }
    }

    /// Attach the managed-launch contract (F1 design §4): the concrete
    /// serve command this adapter renders in `render_plan`.
    pub fn with_launch(mut self, launch: crate::vllm::args::PlanInputVllm) -> Self {
        self.launch = Some(launch);
        self
    }

    fn require_policy(&self, _what: &str) -> Result<(), AdapterError> {
        // Deep-park security gate (SPEC §9.1 / T21): the profile-level
        // opt-in is required for every sleep/collective operation.
        match self.policy {
            ParkPolicy::Denied => Err(AdapterError::PolicyDenied),
            ParkPolicy::ExperimentalAllowed => Ok(()),
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
            Err(e) => return Err(Self::uncertain_http("readiness", e)),
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
