//! Single-attempt, bounded controls for the selected SGLang source pin.

use std::time::Duration;

use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    Client, Url,
};
use serde_json::{json, Value};

use crate::traits::{RuntimeAction, RuntimeError};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const FLUSH_RESPONSE: &[u8] = b"Cache flushed.\nPlease check backend logs for more details. (When there are running or waiting requests, the operation will not be performed.)\n";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReloadReply {
    success: bool,
    #[serde(rename = "message")]
    _message: Option<String>,
    #[serde(rename = "num_paused_requests")]
    _num_paused_requests: Option<u64>,
}

#[derive(serde::Deserialize)]
struct ProbeReply {
    model: String,
    choices: Vec<ProbeChoice>,
}
#[derive(serde::Deserialize)]
struct ProbeChoice {
    index: u32,
    message: ProbeMessage,
    finish_reason: String,
}
#[derive(serde::Deserialize)]
struct ProbeMessage {
    role: String,
    content: String,
}

/// Credentials and the checkpoint root deliberately have no Debug surface.
pub(super) struct ControlHttp {
    client: Client,
    base: Url,
    inference: HeaderValue,
    admin: HeaderValue,
    checkpoint: String,
    model: String,
}

pub(super) fn uncertain() -> RuntimeError {
    RuntimeError::Uncertain("SGLang control requires reconciliation".into())
}

/// A reasoning model emits its chain of thought before its answer. When the engine
/// is launched without a reasoning parser that trace stays in `content` instead of
/// being separated, so an exact-marker probe fails against a runtime that is in fact
/// healthy, and every transition gated on the probe is blocked.
///
/// The marker check itself is deliberately not relaxed: requiring exact content is
/// what distinguishes a restored runtime from one whose weights were resumed but
/// never reloaded, which produces plausible-looking output. Instead the leak is
/// named, so the operator is told to configure a reasoning parser rather than being
/// left with an indistinguishable uncertainty.
///
/// Only the presence of a terminator is inspected. No response content is read into
/// the error, because it can carry private model output.
const REASONING_TERMINATORS: &[&str] = &["</think>", "</reasoning>", "<|end_thinking|>"];

fn leaked_reasoning(content: &str) -> bool {
    REASONING_TERMINATORS
        .iter()
        .any(|terminator| content.contains(terminator))
}

pub(super) fn reasoning_not_separated() -> RuntimeError {
    RuntimeError::Uncertain(
        "SGLang probe content carries a reasoning trace; launch the engine with a \
         reasoning parser so the answer is separated from the trace"
            .into(),
    )
}

pub(super) fn action_timeout(action: RuntimeAction) -> Result<Duration, RuntimeError> {
    let seconds = match action {
        RuntimeAction::Park | RuntimeAction::Restore => 60,
        RuntimeAction::ReloadWeights => 300,
        RuntimeAction::InvalidateCache => 10,
        RuntimeAction::Probe => 30,
        RuntimeAction::Drain => 10,
        _ => return Err(RuntimeError::Unsupported),
    };
    Ok(Duration::from_secs(seconds))
}

impl ControlHttp {
    pub(super) fn new(
        base: Url,
        checkpoint: String,
        model: String,
        inference: String,
        admin: String,
    ) -> Result<Self, RuntimeError> {
        if inference.is_empty()
            || admin.is_empty()
            || inference == admin
            || [&inference, &admin]
                .iter()
                .any(|value| value.len() > 4096 || value.chars().any(char::is_whitespace))
        {
            return Err(RuntimeError::Unsupported);
        }
        let header = |secret: String| {
            let mut value = HeaderValue::from_str(&format!("Bearer {secret}"))
                .map_err(|_| RuntimeError::Unsupported)?;
            value.set_sensitive(true);
            Ok(value)
        };
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .map_err(|_| RuntimeError::Unsupported)?;
        Ok(Self {
            client,
            base,
            checkpoint,
            model,
            inference: header(inference)?,
            admin: header(admin)?,
        })
    }

    pub(super) async fn execute(
        &self,
        action: RuntimeAction,
        timeout: Duration,
    ) -> Result<(), RuntimeError> {
        let (path, body) = match action {
            RuntimeAction::Park => (
                "/release_memory_occupation",
                json!({"tags":["kv_cache","weights"]}),
            ),
            RuntimeAction::Restore => (
                "/resume_memory_occupation",
                json!({"tags":["kv_cache","weights"]}),
            ),
            RuntimeAction::ReloadWeights => (
                "/update_weights_from_disk",
                json!({"model_path":self.checkpoint,"load_format":"auto","abort_all_requests":false,"is_async":false,"keep_pause":false,"recapture_cuda_graph":false,"flush_cache":true}),
            ),
            RuntimeAction::InvalidateCache => ("/flush_cache?timeout=0", Value::Null),
            RuntimeAction::Probe => (
                "/v1/chat/completions",
                json!({"model":self.model,"messages":[{"role":"user","content":"Reply with exactly OK."}],"temperature":0,"max_tokens":8,"stream":false}),
            ),
            _ => return Err(RuntimeError::Unsupported),
        };
        let mut response = self
            .client
            .post(self.base.join(path).map_err(|_| uncertain())?)
            .header(
                AUTHORIZATION,
                if action == RuntimeAction::Probe {
                    &self.inference
                } else {
                    &self.admin
                },
            )
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|_| uncertain())?;
        // Never read, propagate, or log a native error body: it can contain paths
        // and credentials. A non-success acknowledgement never authorizes retry.
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(uncertain());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| uncertain())? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(uncertain());
            }
            bytes.extend_from_slice(&chunk);
        }
        // This pin's flush endpoint returns PlainTextResponse, not JSON. Require
        // its exact successful acknowledgement and corroborating observer facts.
        if action == RuntimeAction::InvalidateCache {
            return if bytes == FLUSH_RESPONSE {
                Ok(())
            } else {
                Err(uncertain())
            };
        }
        let value = if bytes.iter().all(u8::is_ascii_whitespace) {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| uncertain())?
        };
        let accepted = match action {
            RuntimeAction::Park | RuntimeAction::Restore => {
                value.is_null() || value.as_object().is_some_and(|object| object.is_empty())
            }
            RuntimeAction::ReloadWeights => {
                serde_json::from_slice::<ReloadReply>(&bytes).is_ok_and(|reply| reply.success)
            }
            RuntimeAction::Probe => {
                let reply = serde_json::from_slice::<ProbeReply>(&bytes).ok();
                // Name the misconfiguration before the generic failure, but only for
                // an otherwise well-formed reply from this runtime: a malformed or
                // foreign reply is ordinary uncertainty, not a parser problem.
                let leaked = reply.as_ref().is_some_and(|reply| {
                    value.get("error").is_none()
                        && reply.model == self.model
                        && reply.choices.len() == 1
                        && leaked_reasoning(&reply.choices[0].message.content)
                });
                if leaked {
                    return Err(reasoning_not_separated());
                }
                value.get("error").is_none()
                    && reply.is_some_and(|reply| {
                        reply.model == self.model
                            && reply.choices.len() == 1
                            && reply.choices[0].index == 0
                            && reply.choices[0].message.role == "assistant"
                            && reply.choices[0].message.content.trim() == "OK"
                            && reply.choices[0].finish_reason == "stop"
                    })
            }
            _ => false,
        };
        if accepted {
            Ok(())
        } else {
            Err(uncertain())
        }
    }
}
