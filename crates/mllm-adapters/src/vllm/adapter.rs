//! The vLLM adapter: implements the F0 `EngineAdapter` contract over vLLM's
//! OpenAI-compatible HTTP API. Engine-specific endpoints and launch
//! parameters for vLLM live here and only here (SPEC §9).

use async_trait::async_trait;

use crate::fake::ParkPolicy;
use crate::traits::{
    AdapterError, CancellationOutcome, EngineAdapter, EngineState, MemberRef, ParkLevel,
    ParkOutcome, Phase, PlanInput, Quiescence, Readiness, ReloadOutcome, RequestRef,
    RenderedCommand, RestoreOutcome, WorkObservation,
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
/// known park state locally (`parked` flag) and reports `Phase::Parked`
/// from it; `/v1/models` alone never establishes Ready after a park. The
/// controller corroborates via operation provenance.
pub struct VllmAdapter {
    http: EngineHttp,
    fingerprint: String,
    policy: ParkPolicy,
    model_id: String,
    parked: std::sync::atomic::AtomicBool,
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
            http: EngineHttp::new(base, api_key),
            fingerprint,
            policy,
            model_id,
            parked: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn require_policy(&self, _what: &str) -> Result<(), AdapterError> {
        // Deep-park security gate (SPEC §9.1 / T21): the profile-level
        // opt-in is required for every sleep/collective operation.
        match self.policy {
            ParkPolicy::Denied => Err(AdapterError::PolicyDenied),
            ParkPolicy::ExperimentalAllowed => Ok(()),
        }
    }

    fn set_parked(&self, v: bool) {
        self.parked
            .store(v, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_parked(&self) -> bool {
        self.parked.load(std::sync::atomic::Ordering::SeqCst)
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
    async fn inspect(&self, _member: &MemberRef) -> Result<EngineState, AdapterError> {
        // /v1/models presence tells us the API server is up and serving the
        // model id — but after a park the server stays up (F1 design §3),
        // so the local park tracking decides the phase.
        let serving = self.http.list_models().await.map_err(|e| {
            if matches!(e, HttpError::Unreachable(_)) {
                AdapterError::Crash(Phase::Startup)
            } else {
                Self::uncertain_http("inspect", e)
            }
        })?;
        let phase = if self.is_parked() {
            Phase::Parked
        } else if serving.contains(&self.model_id) {
            Phase::Ready
        } else {
            Phase::Startup
        };
        Ok(EngineState {
            phase,
            retained_bytes: if self.is_parked() { level2_residue() } else { 4096 },
            build_fingerprint: Some(self.fingerprint.clone()),
        })
    }

    async fn render_plan(&self, _plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        // Full argument rendering is Task 4's contract (args.rs); the
        // adapter-level operation exists so the trait is complete.
        Err(AdapterError::UnsupportedCapability)
    }

    async fn check_readiness(&self, _member: &MemberRef) -> Result<Readiness, AdapterError> {
        // SPEC §6.1: liveness of an HTTP server is not model readiness.
        // A parked engine is never Ready (parked-state observability).
        if self.is_parked() {
            return Ok(Readiness::Initializing);
        }
        let ids = self.http.list_models().await.map_err(|e| {
            if matches!(e, HttpError::Unreachable(_)) {
                AdapterError::Crash(Phase::Startup)
            } else {
                Self::uncertain_http("readiness", e)
            }
        })?;
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
            WorkObservation::Streaming { .. } => {
                Ok(Quiescence { quiescent: false })
            }
            WorkObservation::Unknown => Ok(Quiescence { quiescent: false }),
        }
    }

    async fn park(&self, _member: &MemberRef, level: ParkLevel) -> Result<ParkOutcome, AdapterError> {
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
                self.set_parked(true);
                Ok(ParkOutcome::Parked { retained_bytes: level2_residue() })
            }
        }
    }

    async fn restore(&self, _member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        // Waking allocations alone is not successful restoration (SPEC §9.1):
        // wake, then reload weights through the collective, then the caller
        // verifies readiness + generation.
        self.http.wake().await.map_err(|e| Self::uncertain_http("restore wake", e))?;
        self.reload_weights(_member).await?;
        self.set_parked(false);
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

    async fn observe_work(&self, _member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        // vLLM exposes no per-request in-flight HTTP surface in the pinned
        // API; accounting rides on stream correlation in the router. The
        // adapter reports what it can prove: nothing observable → Unknown
        // unless parked (a parked engine is provably idle).
        if self.is_parked() {
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
    async fn forward_chat(&self, body: &serde_json::Value) -> Result<serde_json::Value, AdapterError> {
        // Non-streaming: buffer the SSE stream until [DONE] and join the
        // chunks into the engine's final JSON (F1 keeps one code path).
        let mut text = String::new();
        let end = self
            .http
            .chat_completion_stream(body, |c| text.push_str(&c.text))
            .await
            .map_err(|e| match e {
                HttpError::Unreachable(detail) => {
                    let _ = detail;
                    AdapterError::Crash(Phase::Startup)
                }
                other => AdapterError::Uncertain(format!("chat: {other:?}")),
            })?;
        let _ = end;
        let content: String = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or(text);
        Ok(serde_json::json!({
            "model": body["model"],
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}}]
        }))
    }
}
