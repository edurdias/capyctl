//! ADR 0029 §10: the llama.cpp adapter. Ready is `/health` 200 plus the served
//! name listed in `/v1/models` (SPEC §6.1); the slot settings llama-server
//! reports are compared with the rendered ones during Initialize. llama.cpp
//! is restart-only: there is no park, restore or reload path.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::args::PlanInputLlamacpp;
use super::http::{Http, Read};
use crate::traits::*;

pub struct LlamacppAdapter {
    endpoint: reqwest::Url,
    fingerprint: String,
    model_id: String,
    http: Http,
    forward: Arc<dyn ChatForward>,
    launch: Option<PlanInputLlamacpp>,
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    /// ADR 0029 §6: the root `/etc/llama.cpp/config.ini` is looked for under.
    system_root: PathBuf,
    claimed: Mutex<Option<(String, String)>>,
}

impl LlamacppAdapter {
    pub fn new(endpoint: reqwest::Url, fingerprint: String, model_id: String) -> Self {
        Self {
            http: Http::new(endpoint.clone()),
            // ADR 0029 §4: llama-server listens on loopback without a key;
            // nothing is presented.
            forward: crate::forward::engine_forwarder(
                endpoint.clone(),
                model_id.clone(),
                None,
                // ADR 0029 §11, SPEC §10: llama-server never reads
                // `cache_salt`, so a request carrying one is refused.
                false,
            ),
            endpoint,
            fingerprint,
            model_id,
            launch: None,
            tools: None,
            system_root: PathBuf::from(capyctl_config::llamacpp::SYSTEM_ROOT),
            claimed: Mutex::new(None),
        }
    }

    pub fn with_launch(mut self, launch: PlanInputLlamacpp) -> Self {
        self.launch = Some(launch);
        self
    }

    pub fn with_tools(mut self, tools: Arc<dyn OwnedProcessLaunch>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// ADR 0029 §6: look for `etc/llama.cpp/config.ini` under `root` instead
    /// of `/` (tests name their own root).
    pub fn with_system_root(mut self, root: PathBuf) -> Self {
        self.system_root = root;
        self
    }

    pub(super) fn launch_parts(
        &self,
    ) -> Result<(PlanInputLlamacpp, Arc<dyn OwnedProcessLaunch>), RuntimeError> {
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

    pub(super) fn system_root(&self) -> &std::path::Path {
        &self.system_root
    }

    pub(super) fn http(&self) -> &Http {
        &self.http
    }

    pub(super) fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(super) fn model_id(&self) -> &str {
        &self.model_id
    }
}

#[async_trait]
impl EngineAdapter for LlamacppAdapter {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => super::initialize::initialize(self, command).await,
            // ADR 0029 §10: restart-only; single-model llama-server has no
            // sleep, release or unload API.
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
        if !matches!(self.http.health().await?, Read::Answer(true)) {
            return Ok(Readiness::Initializing);
        }
        Ok(match self.http.served_model(&self.model_id).await? {
            Read::Answer(Some(_)) => Readiness::Ready,
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
        // ADR 0029 §10: llama-server cancels a task when its connection
        // closes (`server_response_reader::stop`); it sends no ack.
        Ok(CancellationOutcome::Uncertain)
    }

    async fn engine_quiescent(&self, member: &MemberRef, _after_ms: i64) -> bool {
        // SPEC §10 (amended 2026-10-01), ADR 0029 §10: `/metrics` read now.
        self.idle_before_signal(member).await == Some(EngineWork::Idle)
    }

    async fn idle_before_signal(&self, _member: &MemberRef) -> Option<EngineWork> {
        // ADR 0029 §10: `requests_processing` and `requests_deferred` both 0
        // is idle; a 503 (model not loaded) or nothing listening serves
        // nothing; any other answer without both gauges proves nothing.
        Some(match self.http.work().await {
            Ok(Read::NotYet) => EngineWork::NotListening,
            Ok(Read::Answer(gauges)) if gauges.idle() => EngineWork::Idle,
            Ok(Read::Answer(_)) => EngineWork::Busy,
            Err(_) => EngineWork::Unanswered,
        })
    }
}

#[async_trait]
impl ChatForward for LlamacppAdapter {
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
