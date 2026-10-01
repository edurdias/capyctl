//! ADR 0023: the TensorFold adapter. Ready is `/health` `ok` plus the served
//! name listed (SPEC §6.1); there is no park, restore or reload path.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::http::{Http, Read};
use super::{args::PlanInputTensorfold, HealthReport};
use crate::traits::*;

pub struct TensorfoldAdapter {
    endpoint: reqwest::Url,
    fingerprint: String,
    model_id: String,
    http: Http,
    forward: Arc<dyn ChatForward>,
    launch: Option<PlanInputTensorfold>,
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    extensions_built: bool,
    claimed: Mutex<Option<(String, String)>>,
}

impl TensorfoldAdapter {
    pub fn new(endpoint: reqwest::Url, fingerprint: String, model_id: String) -> Self {
        Self {
            http: Http::new(endpoint.clone()),
            // ADR 0023 §3: TensorFold has no key; nothing is presented.
            forward: crate::forward::engine_forwarder(endpoint.clone(), model_id.clone(), None),
            endpoint,
            fingerprint,
            model_id,
            launch: None,
            tools: None,
            extensions_built: false,
            claimed: Mutex::new(None),
        }
    }

    pub fn with_launch(mut self, launch: PlanInputTensorfold) -> Self {
        self.launch = Some(launch);
        self
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

    async fn idle_before_signal(&self, _member: &MemberRef) -> Option<EngineWork> {
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
}
