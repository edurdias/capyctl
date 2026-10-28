//! ADR 0023: the TensorFold adapter. Ready is `/health` `ok` plus the served
//! name listed (SPEC §6.1); there is no park, restore or reload path.
//! ADR 0039 §9, §10: on the native line, ready is `/health` `status: ok`
//! with the served name in `model_ids` and in `/v1/models`, and the idle
//! read is `/metrics`.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use capyctl_config::tensorfold_native::TensorfoldLine;

use super::http::{Http, NativeHealth, Read};
use super::{args::PlanInputTensorfold, HealthReport};
use crate::traits::*;

pub struct TensorfoldAdapter {
    endpoint: reqwest::Url,
    fingerprint: String,
    model_id: String,
    /// ADR 0039 §1: the installation's line; the readiness and idle reads
    /// branch on it.
    line: TensorfoldLine,
    http: Http,
    forward: Arc<dyn ChatForward>,
    launch: Option<PlanInputTensorfold>,
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    extensions_built: bool,
    claimed: Mutex<Option<(String, String)>>,
}

/// ADR 0039 §9: where a native engine stands on its way to ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NativeReadiness {
    /// The connection was refused: still loading (or exited, which the
    /// Initialize step learns from the process).
    Loading,
    /// `/health` answered 503 (1.1.5's `draining` or `unavailable`), with
    /// the start of its body.
    Unavailable(String),
    /// `/health` answered without `status: ok` and the served name in
    /// `model_ids` (a key-configured engine's bare `{"status": "ok"}`
    /// included), or `/v1/models` does not list the served name.
    Unlisted,
    /// Ready, with the stream count `/health` reported.
    Ready { max_batch_size: Option<u64> },
}

impl TensorfoldAdapter {
    pub fn new(endpoint: reqwest::Url, fingerprint: String, model_id: String) -> Self {
        Self {
            http: Http::new(endpoint.clone()),
            // ADR 0023 §3: TensorFold has no key; nothing is presented.
            forward: crate::forward::engine_forwarder(
                endpoint.clone(),
                model_id.clone(),
                None,
                // SPEC §10: TensorFold ignores `cache_salt`, so it is refused.
                crate::forward::CacheSalt::Refused,
            ),
            endpoint,
            fingerprint,
            model_id,
            line: TensorfoldLine::default(),
            launch: None,
            tools: None,
            extensions_built: false,
            claimed: Mutex::new(None),
        }
    }

    /// ADR 0039 §1: the installation's line, for an adapter that reads an
    /// engine without launching it (an owned launch takes its plan's).
    pub fn with_line(mut self, line: TensorfoldLine) -> Self {
        self.line = line;
        self
    }

    /// The launch plan; the adapter takes the plan's line (ADR 0039 §1).
    pub fn with_launch(mut self, launch: PlanInputTensorfold) -> Self {
        self.line = launch.line;
        self.launch = Some(launch);
        self
    }

    pub fn line(&self) -> TensorfoldLine {
        self.line
    }

    pub fn with_tools(mut self, tools: Arc<dyn OwnedProcessLaunch>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// ADR 0023 §4: whether `TORCH_EXTENSIONS_DIR` held a build when the
    /// launch was prepared; decides the startup bound.
    pub fn with_extensions_built(mut self, built: bool) -> Self {
        self.extensions_built = built;
        self
    }

    pub async fn health(&self) -> Result<HealthReport, AdapterError> {
        match self.http.health().await? {
            Read::Answer(report) => Ok(report),
            Read::NotYet => Err(AdapterError::Uncertain(
                "TensorFold is not answering /health".into(),
            )),
        }
    }

    pub(super) fn launch_parts(
        &self,
    ) -> Result<(PlanInputTensorfold, Arc<dyn OwnedProcessLaunch>), RuntimeError> {
        match (&self.launch, &self.tools) {
            (Some(launch), Some(tools)) => Ok((launch.clone(), tools.clone())),
            _ => Err(RuntimeError::Unsupported),
        }
    }

    pub(super) fn claim_incarnation(
        &self,
        binding: &str,
        incarnation: &str,
    ) -> Result<(), RuntimeError> {
        let mut claimed = self
            .claimed
            .lock()
            .map_err(|_| RuntimeError::Uncertain("claim poisoned".into()))?;
        if claimed.is_some() {
            return Err(RuntimeError::Unsupported);
        }
        *claimed = Some((binding.to_owned(), incarnation.to_owned()));
        Ok(())
    }

    pub(super) fn extensions_built(&self) -> bool {
        self.extensions_built
    }

    pub(super) fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// ADR 0039 §9: the native line's readiness read: a connection
    /// accepted, `/health` 200 with `status` `"ok"` and the served name in
    /// `model_ids`, then the served name in `/v1/models`. A 503 is not
    /// ready and keeps its body for the failure at the start bound.
    pub(super) async fn native_readiness(&self) -> Result<NativeReadiness, AdapterError> {
        let health = self.http.native_health().await?;
        let max_batch_size = match &health {
            NativeHealth::Refused => return Ok(NativeReadiness::Loading),
            NativeHealth::Unavailable(body) => {
                return Ok(NativeReadiness::Unavailable(body.clone()))
            }
            NativeHealth::Answer { max_batch_size, .. } if health.serves(&self.model_id) => {
                *max_batch_size
            }
            NativeHealth::Answer { .. } => return Ok(NativeReadiness::Unlisted),
        };
        Ok(match self.http.models().await? {
            Read::Answer(ids) if ids.contains(&self.model_id) => {
                NativeReadiness::Ready { max_batch_size }
            }
            _ => NativeReadiness::Unlisted,
        })
    }
}

#[async_trait]
impl EngineAdapter for TensorfoldAdapter {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => super::initialize::initialize(self, command).await,
            // ADR 0023 §6: no sleep, release or unload API.
            _ => Err(RuntimeError::Unsupported),
        }
    }

    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError> {
        let phase = match self.check_readiness(member).await? {
            Readiness::Ready => Phase::Ready,
            Readiness::Initializing => Phase::Startup,
        };
        Ok(EngineState {
            phase,
            retained_bytes: 0,
            build_fingerprint: Some(self.fingerprint.clone()),
        })
    }

    async fn render_plan(&self, _plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }

    async fn check_readiness(&self, _member: &MemberRef) -> Result<Readiness, AdapterError> {
        if self.line.is_native() {
            return Ok(match self.native_readiness().await? {
                NativeReadiness::Ready { .. } => Readiness::Ready,
                _ => Readiness::Initializing,
            });
        }
        // SPEC §6.1: liveness of an HTTP server is not model readiness.
        let healthy = matches!(
            self.http.health().await?,
            Read::Answer(HealthReport { ok: true, .. })
        );
        if !healthy {
            return Ok(Readiness::Initializing);
        }
        Ok(match self.http.models().await? {
            Read::Answer(ids) if ids.contains(&self.model_id) => Readiness::Ready,
            _ => Readiness::Initializing,
        })
    }

    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError> {
        Ok(Quiescence {
            quiescent: self.idle_before_signal(member).await == Some(EngineWork::Idle),
            unproven: None,
        })
    }

    async fn park(
        &self,
        _member: &MemberRef,
        _level: ParkLevel,
    ) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }

    async fn restore(&self, _member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }

    async fn reload_weights(&self, _member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }

    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Ok(match self.idle_before_signal(member).await {
            Some(EngineWork::Idle) => WorkObservation::Idle,
            _ => WorkObservation::Unknown,
        })
    }

    async fn cancel_work(
        &self,
        _member: &MemberRef,
        _req: &RequestRef,
        _ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        // TensorFold cancels a request when its socket closes; it sends no ack.
        Ok(CancellationOutcome::Uncertain)
    }

    async fn engine_quiescent(&self, member: &MemberRef, _after_ms: i64) -> bool {
        // SPEC §10 (amended 2026-10-01): the engine's own counters read now
        // (`/health` on 0.6, `/metrics` on the native line, ADR 0039 §10).
        self.idle_before_signal(member).await == Some(EngineWork::Idle)
    }

    async fn idle_before_signal(&self, _member: &MemberRef) -> Option<EngineWork> {
        // ADR 0039 §10: the native line's one `/metrics` read, both gauges
        // from one status snapshot. Refused: nothing listens (exited or
        // still loading), signalled at once; unanswered or unreadable:
        // signalled at the bound; work: held back at the bound.
        if self.line.is_native() {
            return Some(match self.http.native_work().await {
                Ok(Read::NotYet) => EngineWork::NotListening,
                Ok(Read::Answer(work)) if work.idle() => EngineWork::Idle,
                Ok(Read::Answer(_)) => EngineWork::Busy,
                Err(_) => EngineWork::Unanswered,
            });
        }
        // Spec §5: counters that disagree are not idle; either one reporting
        // work is busy.
        Some(match self.http.health().await {
            Ok(Read::NotYet) => EngineWork::NotListening,
            Ok(Read::Answer(report)) if report.idle() == Some(true) => EngineWork::Idle,
            Ok(Read::Answer(_)) => EngineWork::Busy,
            Err(_) => EngineWork::Unanswered,
        })
    }
}

#[async_trait]
impl ChatForward for TensorfoldAdapter {
    async fn forward_chat_stream_async(
        &self,
        body: &serde_json::Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        self.forward.forward_chat_stream_async(body, sink).await
    }

    async fn forward_chat(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        self.forward.forward_chat(body).await
    }

    async fn forward_chat_observed(
        &self,
        body: &serde_json::Value,
        observer: &mut dyn ChatSink,
    ) -> Result<serde_json::Value, AdapterError> {
        self.forward.forward_chat_observed(body, observer).await
    }

    async fn forward_chat_stream(
        &self,
        body: &serde_json::Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<StreamEnded, AdapterError> {
        self.forward.forward_chat_stream(body, on_chunk).await
    }

    async fn complete_probe(
        &self,
        served: &str,
        max_tokens: u32,
        bound: std::time::Duration,
    ) -> Result<crate::completion_probe::ProbeAnswer, AdapterError> {
        self.http.complete_probe(served, max_tokens, bound).await
    }
}
