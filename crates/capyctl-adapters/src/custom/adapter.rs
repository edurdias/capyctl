//! ADR 0031: the custom engine adapter. CapyCTL has no integration for the
//! program, so readiness is generic (§5): the leased port accepts a
//! connection, the manifest's `health` path answers 200 when stated, then the
//! existing probe of the profile's API (the bounded chat probe, or ADR 0030
//! §3's fixed decision), then the loopback check (§7). A custom engine is
//! restart-only (§4): no park, restore or reload. Its idle signal, when the
//! manifest states one, is the only account of its work (§6); without one
//! the engine is never quiescent while it runs.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use capyctl_config::custom::IdleSignal;
use capyctl_config::effective::DeploymentApi;

use super::args::PlanInputCustom;
use super::http::Http;
use crate::traits::*;

pub struct CustomAdapter {
    endpoint: reqwest::Url,
    fingerprint: String,
    model_id: String,
    http: Http,
    forward: Arc<dyn ChatForward>,
    launch: Option<PlanInputCustom>,
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    claimed: Mutex<Option<(String, String)>>,
    /// ADR 0030 §1, ADR 0031 §2: the API the manifest serves.
    api: DeploymentApi,
    /// ADR 0031 §5: the manifest's health path.
    health: Option<String>,
    /// ADR 0031 §6: the manifest's idle signal.
    idle: Option<IdleSignal>,
    /// Where the loopback check reads processes (`/proc`; tests name their own).
    proc_root: std::path::PathBuf,
}

impl CustomAdapter {
    /// An adapter for the engine at `endpoint` serving `model_id`, presenting
    /// `api_key` (ADR 0031 §7: only when the manifest uses `{api_key}`) on
    /// every forwarded request, probe and read.
    pub fn new(
        endpoint: reqwest::Url,
        fingerprint: String,
        model_id: String,
        api_key: Option<String>,
    ) -> Self {
        Self {
            http: Http::new(endpoint.clone(), api_key.as_deref()),
            forward: crate::forward::engine_forwarder(
                endpoint.clone(),
                model_id.clone(),
                api_key,
                // ADR 0031 §4, SPEC §10: CapyCTL cannot know a custom
                // engine honours `cache_salt`, so a request carrying one is
                // refused.
                crate::forward::engine_honours_cache_salt(Some(
                    capyctl_config::engine_policy::Engine::Custom.name(),
                )),
            ),
            endpoint,
            fingerprint,
            model_id,
            launch: None,
            tools: None,
            claimed: Mutex::new(None),
            api: DeploymentApi::Chat,
            health: None,
            idle: None,
            proc_root: std::path::PathBuf::from("/proc"),
        }
    }

    pub fn with_launch(mut self, launch: PlanInputCustom) -> Self {
        self.launch = Some(launch);
        self
    }

    pub fn with_tools(mut self, tools: Arc<dyn OwnedProcessLaunch>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// ADR 0031 §2: the API every deployment on the manifest serves.
    pub fn with_api(mut self, api: DeploymentApi) -> Self {
        self.api = api;
        self
    }

    /// ADR 0031 §5: the manifest's health path, polled before the probe.
    pub fn with_health(mut self, health: Option<String>) -> Self {
        self.health = health;
        self
    }

    /// ADR 0031 §6: the manifest's idle signal; `None` reports no work.
    pub fn with_idle(mut self, idle: Option<IdleSignal>) -> Self {
        self.idle = idle;
        self
    }

    /// The `/proc` the loopback check reads (tests name their own).
    pub fn with_proc_root(mut self, root: std::path::PathBuf) -> Self {
        self.proc_root = root;
        self
    }

    /// ADR 0030 §3: whether readiness is the fixed decision probe.
    pub(super) fn decisions(&self) -> bool {
        !self.api.is_chat()
    }

    pub(super) fn launch_parts(
        &self,
    ) -> Result<(PlanInputCustom, Arc<dyn OwnedProcessLaunch>), RuntimeError> {
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

    pub(super) fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(super) fn proc_root(&self) -> &std::path::Path {
        &self.proc_root
    }

    /// ADR 0030 §3: the fixed decision, on the loopback port with the key.
    pub(super) async fn decide(
        &self,
        served: &str,
        bound: std::time::Duration,
    ) -> Result<crate::decision_probe::DecisionAnswer, AdapterError> {
        crate::decision_probe::probe(
            self.http.probes(),
            self.http.base(),
            self.http.key(),
            served,
            bound,
        )
        .await
    }
}

#[async_trait]
impl EngineAdapter for CustomAdapter {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => super::initialize::initialize(self, command).await,
            // ADR 0031 §4: restart-only.
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

    /// ADR 0031 §5: the port accepts a connection and the health path, when
    /// stated, answers 200. SPEC §6.1: that is liveness, not readiness; the
    /// callers (Initialize, a host's re-proof) probe the API after it. The
    /// model list is never read.
    async fn check_readiness(&self, _member: &MemberRef) -> Result<Readiness, AdapterError> {
        if !self.http.port_accepts().await {
            return Ok(Readiness::Initializing);
        }
        if let Some(path) = &self.health {
            if !self.http.health(path).await? {
                return Ok(Readiness::Initializing);
            }
        }
        Ok(Readiness::Ready)
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

    /// ADR 0031 §6: idle only on an idle reading; anything else, and an
    /// engine without an idle signal, is unknown.
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
        // ADR 0031 §6: CapyCTL cannot tell a custom engine to stop a request;
        // only its idle signal, read after the hang-up, acknowledges it.
        Ok(CancellationOutcome::Uncertain)
    }

    /// ADR 0031 §6, SPEC §10: the cancellation acknowledgement is an idle
    /// reading made now; an engine without an idle signal never gives one
    /// while it runs.
    async fn engine_quiescent(&self, member: &MemberRef, _after_ms: i64) -> bool {
        self.idle.is_some() && self.idle_before_signal(member).await == Some(EngineWork::Idle)
    }

    /// ADR 0031 §6: the read before the stop signal. Idle, exited or not
    /// listening is signalled at once; unanswered is signalled at the bound;
    /// busy at the bound is not signalled. Without an idle signal there is
    /// no account and the stop proceeds on CapyCTL's own drain.
    async fn idle_before_signal(&self, _member: &MemberRef) -> Option<EngineWork> {
        let signal = self.idle.as_ref()?;
        Some(self.http.idle(signal).await)
    }

    /// ADR 0031 §5, ADR 0030 §4: a decisions launch's wake check is the fixed
    /// decision, observed when its answer arrived. A custom engine never
    /// parks, so only the reference at its first readiness is asked. A chat
    /// launch has no canary: its check after a restart is the startup chat
    /// probe its Initialize runs.
    async fn wake_canary(
        &self,
        _context: &capyctl_domain::completion::StepExecutionContext,
        _max_tokens: u32,
        bound: std::time::Duration,
    ) -> Result<crate::completion_probe::CanaryAnswer, RuntimeError> {
        if !self.decisions() {
            return Err(RuntimeError::Unsupported);
        }
        let answer = self.decide(&self.model_id, bound).await.map_err(|e| {
            RuntimeError::Uncertain(format!(
                "the custom engine did not answer the decision probe: {e}"
            ))
        })?;
        let observed_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
            .ok_or_else(|| RuntimeError::Uncertain("the clock is before the epoch".into()))?;
        Ok(crate::completion_probe::CanaryAnswer {
            answer: crate::completion_probe::CanaryEvidence::Decision(answer),
            observed_at_ms,
        })
    }
}

/// ADR 0031 §4: chat and decisions through the generic forwarder; an
/// engine's 4xx keeps the existing rejected mapping. No per-request engine
/// figures are read.
#[async_trait]
impl ChatForward for CustomAdapter {
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

    async fn forward_decision(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        self.forward.forward_decision(body).await
    }

    async fn forward_chat_stream(
        &self,
        body: &serde_json::Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<StreamEnded, AdapterError> {
        self.forward.forward_chat_stream(body, on_chunk).await
    }

    /// ADR 0030 §3: the fixed decision probe, on the loopback port with the
    /// launch's key. A chat custom engine is not asked one. The completion
    /// probe (ADR 0028 §9) is not offered: CapyCTL does not know a custom
    /// engine serves `/v1/completions`.
    async fn decision_probe(
        &self,
        served: &str,
        bound: std::time::Duration,
    ) -> Result<crate::decision_probe::DecisionAnswer, AdapterError> {
        if !self.decisions() {
            return Err(AdapterError::UnsupportedCapability);
        }
        self.decide(served, bound).await
    }
}
