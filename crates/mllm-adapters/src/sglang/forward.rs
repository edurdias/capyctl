//! Forwarding uses the frozen binding, never a request-selected upstream.
use super::SglangAdapter;
use crate::traits::{AdapterError, ChatForward, StreamEnded};
use async_trait::async_trait;

#[async_trait]
impl ChatForward for SglangAdapter {
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
    ) -> Result<StreamEnded, AdapterError> {
        self.forward.stream(body, on_chunk).await
    }
}
