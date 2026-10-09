//! Single-attempt, bounded controls for the selected SGLang source pin.

use std::time::Duration;

use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    Client, Url,
};
use serde_json::{json, Value};

use crate::traits::{AdapterError, RuntimeAction, RuntimeError};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// How long one readiness model list may take. The readiness loop caps each
/// poll by this and its own remaining budget, so a stall is a poll failure the
/// builder reports, never a hang past the coordinator's bound.
pub(super) const MODELS_TIMEOUT: Duration = Duration::from_secs(30);
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
    /// Present when the engine runs a reasoning parser. Only its emptiness is
    /// examined; the trace itself is never read, logged or copied into an error.
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Credentials and the checkpoint root deliberately have no Debug surface.
pub(super) struct ControlHttp {
    client: Client,
    base: Url,
    inference: HeaderValue,
    admin: HeaderValue,
    checkpoint: String,
    model: String,
    /// ADR 0014 amendment A17: the launch's park keeps the weights resident,
    /// so release and resume name the KV cache region alone.
    resident_weights: bool,
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

/// With a reasoning parser configured the trace is separated correctly, but a
/// reasoning model can still spend the whole probe budget thinking, leaving empty
/// content and a length finish. The runtime is healthy and the probe is simply too
/// small, which is a different repair from a missing parser.
///
/// Measured on host-a with Qwen3.5-27B and `--reasoning-parser qwen3`: eight
/// tokens yielded empty content, while 512 yielded the exact marker.
pub(super) fn reasoning_budget_too_small() -> RuntimeError {
    RuntimeError::Uncertain(
        "SGLang probe budget was consumed by reasoning before an answer was \
         produced; the recipe needs a probe budget that fits this model's trace"
            .into(),
    )
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
        // A disk reload scales with the checkpoint (61 GB took about 350 s
        // live, M27): the step's deadline, the deployment's wake timeout,
        // bounds it. This cap only keeps the value finite.
        RuntimeAction::ReloadWeights => 3600,
        RuntimeAction::InvalidateCache => 10,
        RuntimeAction::Probe => 30,
        RuntimeAction::Drain => 10,
        _ => return Err(RuntimeError::Unsupported),
    };
    Ok(Duration::from_secs(seconds))
}

/// How a readiness model list failed. Connection refusal is the engine still
/// staging weights; anything else is an answer worth recording.
pub(super) enum ModelsError {
    Unreachable,
    AuthRejected,
    Other(String),
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable => write!(f, "engine unreachable"),
            Self::AuthRejected => write!(f, "engine rejected authentication"),
            Self::Other(detail) => write!(f, "{detail}"),
        }
    }
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
            resident_weights: false,
        })
    }

    /// Park and wake the KV cache region alone (`weight_restore: resident`).
    pub(super) fn with_resident_weights(mut self, resident: bool) -> Self {
        self.resident_weights = resident;
        self
    }

    /// ADR 0028 §9 (decided 2026-10-06): the completion probe in SGLang's
    /// form, its native `POST /generate`, on this engine's loopback endpoint
    /// with its inference key (an inference route, not an admin one).
    /// Answers `output_ids` and `text`.
    pub(super) async fn complete_probe(
        &self,
        max_tokens: u32,
        bound: Duration,
    ) -> Result<crate::completion_probe::ProbeAnswer, AdapterError> {
        use crate::completion_probe as probe;
        let url = self
            .base
            .join(probe::SGLANG_PATH)
            .map_err(|_| AdapterError::Uncertain("completion probe: no endpoint".into()))?;
        let answer = probe::post(
            &self.client,
            url,
            Some(&self.inference),
            &probe::sglang_request(max_tokens),
            bound,
        )
        .await?;
        probe::sglang_answer(&answer)
    }

    /// Served model ids from `/v1/models`, guarded by the inference key the
    /// engine was launched with. Presence of the served name is the readiness
    /// signal; a connect refusal is the engine still staging weights.
    pub(super) async fn models(&self) -> Result<Vec<String>, ModelsError> {
        let mut response = self
            .client
            .get(self.base.join("/v1/models").map_err(|_| {
                ModelsError::Other("model list endpoint could not be resolved".into())
            })?)
            .header(AUTHORIZATION, &self.inference)
            .timeout(MODELS_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    ModelsError::Unreachable
                } else {
                    ModelsError::Other(format!("model list: {e}"))
                }
            })?;
        match response.status().as_u16() {
            200..=299 => {}
            401 | 403 => return Err(ModelsError::AuthRejected),
            status => return Err(ModelsError::Other(format!("model list status {status}"))),
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ModelsError::Other("model list body too large".into()));
        }
        // The same bounded read the controls use: a chunked body without a
        // declared length must not be read without limit.
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            if e.is_connect() {
                ModelsError::Unreachable
            } else {
                ModelsError::Other(format!("model list body: {e}"))
            }
        })? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(ModelsError::Other("model list body too large".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| ModelsError::Other(format!("model list body: {e}")))?;
        Ok(value["data"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub(super) async fn execute(
        &self,
        action: RuntimeAction,
        timeout: Duration,
    ) -> Result<(), RuntimeError> {
        // ADR 0014 amendment A17: a resident-weights park leaves the weights
        // region (the draft model's included) mapped.
        let tags = if self.resident_weights {
            json!({"tags":["kv_cache"]})
        } else {
            json!({"tags":["kv_cache","weights"]})
        };
        let (path, body) = match action {
            RuntimeAction::Park => ("/release_memory_occupation", tags),
            RuntimeAction::Restore => ("/resume_memory_occupation", tags),
            RuntimeAction::ReloadWeights => (
                "/update_weights_from_disk",
                json!({"model_path":self.checkpoint,"load_format":"auto","abort_all_requests":false,"is_async":false,"keep_pause":false,"recapture_cuda_graph":false,"flush_cache":true}),
            ),
            RuntimeAction::InvalidateCache => ("/flush_cache?timeout=0", Value::Null),
            // SPEC §8.3: ask for the exact usability marker without inviting
            // punctuation (Qwen2.5-1.5B otherwise answers "OK."). Found live
            // 2026-10-02 (SGLang 0.5.20 and 0.5.21): Qwen3-4B spent all 8
            // tokens thinking, so every deep wake was left uncertain.
            // `enable_thinking: false` asks the chat template to skip the
            // trace; a template without the switch ignores it.
            RuntimeAction::Probe => (
                "/v1/chat/completions",
                json!({"model":self.model,"messages":[{"role":"user","content":"Reply with exactly the two letters OK and nothing else. Do not add punctuation."}],"temperature":0,"max_tokens":8,"stream":false,"chat_template_kwargs":{"enable_thinking":false}}),
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
                // A separated trace that consumed the whole budget: healthy runtime,
                // undersized probe. Distinguished from a missing parser because the
                // repair differs.
                let starved = reply.as_ref().is_some_and(|reply| {
                    value.get("error").is_none()
                        && reply.model == self.model
                        && reply.choices.len() == 1
                        && reply.choices[0].message.content.trim().is_empty()
                        && reply.choices[0]
                            .message
                            .reasoning_content
                            .as_ref()
                            .is_some_and(|trace| !trace.trim().is_empty())
                        && reply.choices[0].finish_reason == "length"
                });
                if starved {
                    return Err(reasoning_budget_too_small());
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

#[cfg(test)]
mod reload_bound_tests {
    use super::*;

    // T16 T20: found live 2026-09-23 (matrix M27, host-a). A deep wake of
    // qwen3-30b-a3b reloaded 61 GB from disk in about 350 s; a fixed 300 s
    // bound on the reload left the wake uncertain although the engine
    // finished. The step's own deadline (the deployment's wake timeout) is the
    // bound; the reload has no shorter fixed cap of its own.
    #[test]
    fn a_weight_reload_is_bounded_by_its_step_deadline_not_a_fixed_cap() {
        let cap = action_timeout(RuntimeAction::ReloadWeights).unwrap();
        assert!(cap >= Duration::from_secs(3600), "{cap:?}");
        assert_eq!(
            action_timeout(RuntimeAction::Park).unwrap(),
            Duration::from_secs(60)
        );
    }
}
