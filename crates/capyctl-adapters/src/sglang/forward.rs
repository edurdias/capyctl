//! Forwarding uses the frozen binding, never a request-selected upstream.
use super::SglangAdapter;
use crate::traits::{AdapterError, ChatForward, StreamEnded};
use async_trait::async_trait;

#[async_trait]
impl ChatForward for SglangAdapter {
    async fn forward_chat_stream_async(
        &self,
        body: &crate::request::InferenceRequest,
        sink: &mut dyn crate::traits::ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        self.forward.stream_async(body, sink).await
    }
    async fn forward_chat(
        &self,
        body: &crate::request::InferenceRequest,
    ) -> Result<crate::answer::EngineAnswer, AdapterError> {
        self.forward.collect(body).await
    }
    async fn forward_chat_observed(
        &self,
        body: &crate::request::InferenceRequest,
        observer: &mut dyn crate::traits::ChatSink,
    ) -> Result<crate::answer::EngineAnswer, AdapterError> {
        self.forward.collect_observed(body, observer, None).await
    }
    async fn forward_chat_stream(
        &self,
        body: &crate::request::InferenceRequest,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<StreamEnded, AdapterError> {
        self.forward.stream(body, on_chunk).await
    }
    /// ADR 0028 §9: through the keyed control client; an adapter built
    /// without the launch's credentials cannot probe.
    async fn complete_probe(
        &self,
        _served: &str,
        max_tokens: u32,
        bound: std::time::Duration,
    ) -> Result<crate::completion_probe::ProbeAnswer, AdapterError> {
        let http = self
            .http
            .as_ref()
            .ok_or(AdapterError::UnsupportedCapability)?;
        http.complete_probe(max_tokens, bound).await
    }
    /// ADR 0030 §3: through the keyed control client, as the completion
    /// probe; an adapter built without the launch's credentials cannot probe.
    async fn decision_probe(
        &self,
        _served: &str,
        bound: std::time::Duration,
    ) -> Result<crate::decision_probe::DecisionAnswer, AdapterError> {
        let http = self
            .http
            .as_ref()
            .ok_or(AdapterError::UnsupportedCapability)?;
        http.decision_probe(bound).await
    }
}
