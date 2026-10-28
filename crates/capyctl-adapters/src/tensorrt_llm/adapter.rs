//! ADR 0038 §5–§8: the TensorRT-LLM adapter. Ready is `/health` 200 plus the
//! served name listed in `/v1/models` (SPEC §6.1); the version, the model
//! list, the built KV pool and the launch's ownership are verified during
//! Initialize. TensorRT-LLM is restart-only: there is no park, restore or
//! reload path. It reports no idle signal (ADR 0031 §6 applies, ADR 0038
//! §7): no reading of `/metrics` is idle evidence, cancellation is never
//! acknowledged, and cancelling leases settle at the verified stop.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::args::PlanInputTensorrtLlm;
use super::http::{newest_load, Http, IterationLoad, Read};
use crate::traits::*;

/// Where a host reads its processes' state (`/proc`).
const PROC_ROOT: &str = "/proc";

pub struct TensorrtLlmAdapter {
    endpoint: reqwest::Url,
    fingerprint: String,
    model_id: String,
    http: Http,
    forward: Arc<dyn ChatForward>,
    launch: Option<PlanInputTensorrtLlm>,
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    /// ADR 0038 §9: where the descendant and listener checks read processes.
    proc_root: PathBuf,
    claimed: Mutex<Option<(String, String)>>,
}

impl TensorrtLlmAdapter {
    pub fn new(endpoint: reqwest::Url, fingerprint: String, model_id: String) -> Self {
        Self {
            http: Http::new(endpoint.clone()),
            // ADR 0038 §9: `trtllm-serve` listens on loopback without a key;
            // nothing is presented.
            forward: crate::forward::engine_forwarder(
                endpoint.clone(),
                model_id.clone(),
                None,
                // ADR 0038 §11, ADR 0033 §5: TensorRT-LLM partitions block
                // reuse by `cache_salt`, but the adapter's own requests are
                // probes, which carry no salt, and it does not know the
                // launch's deployment and revision; a salt reaching it is
                // refused, never forwarded unchanged. Client requests go
                // through the router's launch-specific forwarder.
                crate::forward::CacheSalt::Refused,
            ),
            endpoint,
            fingerprint,
            model_id,
            launch: None,
            tools: None,
            proc_root: PathBuf::from(PROC_ROOT),
            claimed: Mutex::new(None),
        }
    }

    pub fn with_launch(mut self, launch: PlanInputTensorrtLlm) -> Self {
        self.launch = Some(launch);
        self
    }

    pub fn with_tools(mut self, tools: Arc<dyn OwnedProcessLaunch>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// ADR 0038 §9: read processes under `root` instead of `/proc` (tests
    /// name their own root).
    pub fn with_proc_root(mut self, root: PathBuf) -> Self {
        self.proc_root = root;
        self
    }

    /// ADR 0038 §6: one `/metrics` read, as the host's load sampler makes it
    /// (the only reader: a read consumes the entries). `None` is unknown,
    /// never zero and never idle: the read returned no entry (no iteration
    /// since the last read, or a failed fetch, which `/metrics` does not
    /// distinguish) or its newest entry lacks a figure.
    pub async fn load(&self) -> Result<Option<IterationLoad>, AdapterError> {
        Ok(match self.http.metrics().await? {
            Read::NotYet => None,
            Read::Answer(entries) => newest_load(&entries),
        })
    }

    pub(super) fn launch_parts(
        &self,
    ) -> Result<(PlanInputTensorrtLlm, Arc<dyn OwnedProcessLaunch>), RuntimeError> {
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

    pub(super) fn http(&self) -> &Http {
        &self.http
    }

    pub(super) fn endpoint(&self) -> &reqwest::Url {
        &self.endpoint
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(super) fn proc_root(&self) -> &Path {
        &self.proc_root
    }
}

#[async_trait]
impl EngineAdapter for TensorrtLlmAdapter {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => super::initialize::initialize(self, command).await,
            // ADR 0038 §8: restart-only; `trtllm-serve` builds no `AsyncLLM`,
            // so it has no usable sleep, release or reload.
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
        if !matches!(self.http.health().await?, Read::Answer(())) {
            return Ok(Readiness::Initializing);
        }
        Ok(match self.http.models().await? {
            Read::Answer(ids) if ids.contains(&self.model_id) => Readiness::Ready,
            _ => Readiness::Initializing,
        })
    }

    async fn prepare_park(&self, _member: &MemberRef) -> Result<Quiescence, AdapterError> {
        // ADR 0038 §7: no idle signal, so the engine is never shown quiescent.
        Ok(Quiescence {
            quiescent: false,
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

    async fn observe_work(&self, _member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        // ADR 0038 §6, §7: no `/metrics` reading is idle evidence (no entry
        // reads 0 and 0; an empty read is ambiguous), and a read here would
        // consume the entries the host's load sampler reads ([`Self::load`]).
        // The router's own lease counts decide.
        Ok(WorkObservation::Unknown)
    }

    async fn cancel_work(
        &self,
        _member: &MemberRef,
        _req: &RequestRef,
        _ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        // ADR 0038 §7: the engine aborts a request only when its connection
        // closes, queued for its next iteration, with no abort by ID and no
        // acknowledgement; the lease settles at the verified stop.
        Ok(CancellationOutcome::Uncertain)
    }

    // ADR 0031 §6, ADR 0038 §7: `idle_before_signal` stays `None` (no idle
    // account) and `engine_quiescent` stays `false` (the trait defaults);
    // `wake_canary` stays unsupported (restart-only chat, no canary).
}

#[async_trait]
impl ChatForward for TensorrtLlmAdapter {
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
