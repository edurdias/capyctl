//! Shared, bounded text-chat transport. Terminal success is not lifecycle or
//! readiness evidence; cancellation and partial output never prove idle.
use crate::traits::{AdapterError, ChatSink, DeliveryFailed, StreamEnded};
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

const EVENT_LIMIT: usize = 64 * 1024;
const STREAM_LIMIT: usize = 16 * 1024 * 1024;

struct CallbackSink<'a>(&'a mut (dyn FnMut(String) + Send));
#[async_trait::async_trait]
impl ChatSink for CallbackSink<'_> {
    async fn send(&mut self, chunk: String) -> Result<(), DeliveryFailed> {
        (self.0)(chunk);
        Ok(())
    }
}

/// SPEC §10 (preserve supported payloads, tool calls, structured-output
/// parameters, reasoning fields and multimodal content), T19 T21: the top-level
/// chat request fields a forward carries. The OpenAI chat fields, then the
/// sampling, structured-output and reasoning extensions vLLM and SGLang accept.
/// Anything else, and in particular an engine-internal field (a request id, a
/// LoRA path, hidden states, a custom logit processor, disaggregation
/// bootstrap, KV transfer parameters, engine extras, scheduling priority or a
/// server-side chat template), is refused before forwarding, never dropped.
pub const CHAT_REQUEST_FIELDS: &[&str] = &[
    // OpenAI chat completions.
    "model",
    "messages",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "n",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "user",
    "seed",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "response_format",
    "functions",
    "function_call",
    "reasoning_effort",
    "modalities",
    "metadata",
    // Sampling extensions.
    "top_k",
    "min_p",
    "repetition_penalty",
    "min_tokens",
    "stop_token_ids",
    "ignore_eos",
    "skip_special_tokens",
    "spaces_between_special_tokens",
    "include_stop_str_in_output",
    "no_stop_trim",
    "length_penalty",
    // Chat templating inputs (never the template itself).
    "chat_template_kwargs",
    "add_generation_prompt",
    "continue_final_message",
    "echo",
    "documents",
    // Structured output.
    "guided_json",
    "guided_regex",
    "guided_choice",
    "guided_grammar",
    "structured_outputs",
    "json_schema",
    "regex",
    "ebnf",
    // Reasoning.
    "separate_reasoning",
    "stream_reasoning",
    "include_reasoning",
    // Prefix-cache partition (SPEC §10): forwarded unchanged to an engine that
    // honours it, refused for one that would ignore it.
    "cache_salt",
];

/// SPEC §10: the longest `cache_salt` forwarded, in bytes. vLLM bounds the
/// field at 1024 characters, so no salt this admits is one it refuses.
pub const MAX_CACHE_SALT_BYTES: usize = 1024;

/// Whether `body` asks for a prefix-cache partition (a non-null `cache_salt`).
pub fn carries_cache_salt(body: &Value) -> bool {
    body.get("cache_salt").is_some_and(|salt| !salt.is_null())
}

/// Whether `body` is a JSON object carrying only [`CHAT_REQUEST_FIELDS`].
pub fn chat_request_allowed(body: &Value) -> bool {
    body.as_object().is_some_and(|fields| {
        fields
            .keys()
            .all(|name| CHAT_REQUEST_FIELDS.contains(&name.as_str()))
    })
}

/// Why a chat request is refused before anything is sent to an engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatRequestRefusal {
    /// Not a JSON object, or no non-empty `model`.
    Malformed(&'static str),
    /// SPEC §10, T21: a field outside [`CHAT_REQUEST_FIELDS`] (engine-internal
    /// or unknown). Refused, never dropped.
    Field(String),
    /// An allowed OpenAI field whose response this transport cannot relay
    /// faithfully, so forwarding it would silently change its meaning.
    Unsupported(&'static str),
}

impl std::fmt::Display for ChatRequestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed chat request: {what}"),
            Self::Field(name) => write!(f, "request field `{name}` is not accepted"),
            Self::Unsupported(what) => write!(f, "unsupported chat request parameter: {what}"),
        }
    }
}

impl From<ChatRequestRefusal> for AdapterError {
    fn from(refusal: ChatRequestRefusal) -> Self {
        match refusal {
            ChatRequestRefusal::Field(_) => AdapterError::PolicyDenied,
            ChatRequestRefusal::Malformed(_) | ChatRequestRefusal::Unsupported(_) => {
                AdapterError::UnsupportedCapability
            }
        }
    }
}

/// Validate a chat request before anything is sent, so every refusal is a
/// deterministic client error with nothing reaching an engine (SPEC §10, T19).
///
/// SPEC §10 requires tool calls to be preserved: `tools`, `tool_choice` and
/// `parallel_tool_calls` are forwarded and the engine's `tool_calls` deltas are
/// relayed (and assembled for a non-streaming response). Two parameters stay
/// refused because the relay cannot carry their answer faithfully: `n` other
/// than 1 (the stream parser follows exactly one choice, so extra choices
/// would be dropped), and the deprecated `functions` / `function_call` pair
/// (its `function_call` delta is not the `tool_calls` shape the relay
/// validates, and neither vLLM nor SGLang documents it as supported).
pub fn validate_chat_request(body: &Value) -> Result<(), ChatRequestRefusal> {
    let Some(fields) = body.as_object() else {
        return Err(ChatRequestRefusal::Malformed(
            "the body is not a JSON object",
        ));
    };
    if let Some(name) = fields
        .keys()
        .find(|name| !CHAT_REQUEST_FIELDS.contains(&name.as_str()))
    {
        return Err(ChatRequestRefusal::Field(name.clone()));
    }
    if !fields
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| !model.is_empty())
    {
        return Err(ChatRequestRefusal::Malformed("model is required"));
    }
    if fields
        .get("n")
        .is_some_and(|n| !n.is_null() && n.as_u64() != Some(1))
    {
        return Err(ChatRequestRefusal::Unsupported("n other than 1"));
    }
    if fields.get("cache_salt").is_some_and(|salt| {
        !salt.is_null()
            && !salt
                .as_str()
                .is_some_and(|salt| !salt.is_empty() && salt.len() <= MAX_CACHE_SALT_BYTES)
    }) {
        return Err(ChatRequestRefusal::Malformed(
            "cache_salt must be a non-empty string of at most 1024 bytes",
        ));
    }
    if fields.contains_key("functions") || fields.contains_key("function_call") {
        return Err(ChatRequestRefusal::Unsupported(
            "the deprecated functions/function_call parameters; use tools",
        ));
    }
    Ok(())
}

/// ADR 0030 §2, SPEC §10: the top-level fields of a decision request
/// (`POST /v1/systemone`), the System One contract llama.cpp v0.7.0 and SGLang
/// 0.5.22 serve: the input `state`, the typed `questions`, and the `images` and
/// `files` some models accept. The list is closed. An engine-specific
/// extension (SGLang's `chat_template_kwargs`, which llama.cpp does not read)
/// is refused, never forwarded to an engine that would silently ignore it.
pub const DECISION_REQUEST_FIELDS: &[&str] = &["model", "state", "questions", "images", "files"];

/// ADR 0030 §2: the fields of one question.
pub const DECISION_QUESTION_FIELDS: &[&str] = &["type", "instructions", "criteria"];

/// ADR 0030 §2: the question types (`noul`: yes or no, `choice`: one of
/// named options, `score`: an ordered scale).
pub const DECISION_QUESTION_TYPES: &[&str] = &["noul", "choice", "score"];

/// Why a decision request is refused before anything is sent to an engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionRequestRefusal {
    /// Not a JSON object, no non-empty `model`, or a field of the wrong shape.
    Malformed(&'static str),
    /// A top-level field outside [`DECISION_REQUEST_FIELDS`]. Refused, never
    /// dropped.
    Field(String),
}

impl std::fmt::Display for DecisionRequestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed decision request: {what}"),
            Self::Field(name) => write!(f, "decision request field `{name}` is not accepted"),
        }
    }
}

impl From<DecisionRequestRefusal> for AdapterError {
    fn from(refusal: DecisionRequestRefusal) -> Self {
        match refusal {
            DecisionRequestRefusal::Field(_) => AdapterError::PolicyDenied,
            DecisionRequestRefusal::Malformed(_) => AdapterError::UnsupportedCapability,
        }
    }
}

/// ADR 0030 §2: validate a decision request against its own closed contract
/// before anything is sent, so every refusal is a deterministic client error
/// with nothing reaching an engine (SPEC §10, T43). Shapes only: the counts
/// an engine bounds (options, levels, questions, images) and the semantics of
/// each question are the engine's to judge, and its invalid-request answer
/// is relayed. Nothing here builds a prompt or reads the state (SPEC §10).
pub fn validate_decision_request(body: &Value) -> Result<(), DecisionRequestRefusal> {
    use DecisionRequestRefusal::Malformed;
    let Some(fields) = body.as_object() else {
        return Err(Malformed("the body is not a JSON object"));
    };
    if let Some(name) = fields
        .keys()
        .find(|name| !DECISION_REQUEST_FIELDS.contains(&name.as_str()))
    {
        return Err(DecisionRequestRefusal::Field(name.clone()));
    }
    if !fields
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| !model.is_empty())
    {
        return Err(Malformed("model is required"));
    }
    // Text, a JSON object the engine gives the model as JSON text, or chat
    // messages; null is passed for the engine to judge.
    if !fields.get("state").is_some_and(|state| {
        state.is_string() || state.is_object() || state.is_array() || state.is_null()
    }) {
        return Err(Malformed(
            "state is required: a string, an object or an array of messages",
        ));
    }
    let questions = fields
        .get("questions")
        .and_then(Value::as_object)
        .filter(|questions| !questions.is_empty())
        .ok_or(Malformed(
            "questions must be a non-empty object of question id to question",
        ))?;
    for question in questions.values() {
        let question = question
            .as_object()
            .ok_or(Malformed("each question must be an object"))?;
        if question
            .keys()
            .any(|name| !DECISION_QUESTION_FIELDS.contains(&name.as_str()))
        {
            return Err(Malformed(
                "a question carries only type, instructions and criteria",
            ));
        }
        let kind = question
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| DECISION_QUESTION_TYPES.contains(kind))
            .ok_or(Malformed("a question's type must be noul, choice or score"))?;
        if question
            .get("instructions")
            .is_some_and(|text| !(text.is_string() || text.is_object() || text.is_array()))
        {
            return Err(Malformed(
                "a question's instructions must be a string, an object or an array",
            ));
        }
        // The System One forms: a `choice` names its options in an object
        // (option to description), a `noul` may describe `true` and `false`
        // in an object, and a `score` lists its levels low to high.
        let criteria_shaped = match question.get("criteria") {
            None => true,
            Some(criteria) if kind == "score" => criteria.is_array(),
            Some(criteria) => criteria.is_object(),
        };
        if !criteria_shaped {
            return Err(Malformed(
                "criteria must be an object for choice and noul questions and an array for score questions",
            ));
        }
    }
    for field in ["images", "files"] {
        if fields.get(field).is_some_and(|items| {
            !items
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string))
        }) {
            return Err(Malformed("images and files must be arrays of strings"));
        }
    }
    Ok(())
}

/// Whether `body` is a decision request the host ingress may forward.
pub fn decision_request_allowed(body: &Value) -> bool {
    validate_decision_request(body).is_ok()
}

/// ADR 0030 §2: completion evidence for a decision request: an `answers`
/// object holding at least one answer, each an object naming its `type`.
/// Loose on purpose: what each answer carries differs by type and engine (a
/// `noul` answer carries only its probability), and the router relays it.
pub fn decision_answered(answer: &Value) -> bool {
    answer
        .get("answers")
        .and_then(Value::as_object)
        .is_some_and(|answers| {
            !answers.is_empty()
                && answers
                    .values()
                    .all(|answer| answer.get("type").is_some_and(Value::is_string))
        })
}

/// ADR 0030 §2: the largest decision answer read, the same bound as a
/// collected chat answer's stream ([`STREAM_LIMIT`]). An answer is a few
/// probabilities per question, so a larger one is not an answer.
pub const DECISION_RESPONSE_LIMIT: usize = STREAM_LIMIT;

fn uncertain() -> AdapterError {
    AdapterError::Uncertain("chat terminal result unverified".into())
}

fn decision_uncertain() -> AdapterError {
    AdapterError::Uncertain("decision answer unverified".into())
}

/// The only upstream paths a forwarder may reach on an engine.
///
/// SPEC §10: the router relays inference and nothing else. An engine also serves
/// its own administrative surface — park, sleep, weight reload, collective RPC —
/// and reaching any of it on a client's behalf would put engine control behind the
/// inference port, where none of the lifecycle authority's accounting applies. The
/// list is closed rather than filtered so a new path has to be added deliberately.
/// ADR 0030 §2 adds the decision endpoint, for decisions deployments.
const FORWARDED_PATHS: [&str; 3] = ["/v1/models", "/v1/chat/completions", "/v1/systemone"];

/// Resolve one upstream URL, or refuse the path.
///
/// Returning `None` rather than joining the path is the refusal: a caller cannot
/// build a request it has no URL for.
fn upstream(base: &reqwest::Url, path: &str) -> Option<reqwest::Url> {
    if !FORWARDED_PATHS.contains(&path) {
        return None;
    }
    base.join(path).ok()
}

/// Holds only service-selected endpoint, model, and inference authentication.
/// Intentionally no Debug implementation: credentials must never be formatted.
pub(crate) struct ChatHttp {
    endpoint: reqwest::Url,
    /// ADR 0030 §2: the engine's decision endpoint.
    decisions: reqwest::Url,
    model: String,
    key: Option<String>,
    client: reqwest::Client,
    /// SPEC §10: whether this engine partitions its prefix cache by
    /// `cache_salt`. When it does not, a request carrying one is refused.
    honours_cache_salt: bool,
}

/// SPEC §10: whether an engine family partitions its prefix cache by
/// `cache_salt`. vLLM and SGLang do; TensorFold ignores the field, and an
/// unrecorded family is treated as not honouring it, so a requested isolation
/// is never silently absent.
pub fn engine_honours_cache_salt(engine: Option<&str>) -> bool {
    matches!(engine, Some("vllm" | "sglang"))
}

impl ChatHttp {
    pub(crate) fn new(
        base: reqwest::Url,
        model: String,
        key: Option<String>,
        honours_cache_salt: bool,
    ) -> Self {
        Self {
            endpoint: upstream(&base, "/v1/chat/completions").expect("a forwarded path"),
            decisions: upstream(&base, "/v1/systemone").expect("a forwarded path"),
            model,
            key,
            honours_cache_salt,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("static client configuration"),
        }
    }

    /// The callback path's collection, with the transport's own bounds (60 s
    /// between reads, 300 s overall), for callers that bound nothing
    /// themselves. The router uses [`ChatHttp::collect_observed`] instead.
    pub(crate) async fn collect(&self, body: &Value) -> Result<Value, AdapterError> {
        tokio::time::timeout(
            Duration::from_secs(300),
            self.collect_observed(body, &mut NoObserver, Some(Duration::from_secs(60))),
        )
        .await
        .map_err(|_| uncertain())?
    }

    /// SPEC §10: collect a non-streaming response while reporting every backend
    /// event to `observer`, so the caller bounds it exactly as it bounds a
    /// relayed stream (request deadline for the first event, then an idle
    /// bound), instead of by a fixed wall-clock cap.
    pub(crate) async fn collect_observed(
        &self,
        body: &Value,
        observer: &mut dyn ChatSink,
        read_idle: Option<Duration>,
    ) -> Result<Value, AdapterError> {
        let mut chunks = Vec::new();
        // SPEC §10 (found live 2026-10-03): vLLM streams usage only when asked;
        // the collected response must carry it as a non-streaming one does.
        let mut body = body.clone();
        if body.is_object() && body.get("stream_options").is_none_or(Value::is_null) {
            body["stream_options"] = json!({"include_usage": true});
        }
        let end = self
            .stream_inner(
                &body,
                &mut Collecting {
                    chunks: &mut chunks,
                    observer,
                },
                read_idle,
            )
            .await?;
        if end != StreamEnded::Completed {
            return Err(uncertain());
        }
        assemble(chunks)
    }
}

/// Discards progress: the self-bounded collection path.
struct NoObserver;
#[async_trait::async_trait]
impl ChatSink for NoObserver {
    async fn send(&mut self, _chunk: String) -> Result<(), DeliveryFailed> {
        Ok(())
    }
}

/// Keeps every chunk for assembly and forwards backend progress.
struct Collecting<'a> {
    chunks: &'a mut Vec<String>,
    observer: &'a mut dyn ChatSink,
}
#[async_trait::async_trait]
impl ChatSink for Collecting<'_> {
    fn progressed(&mut self) {
        self.observer.progressed();
    }
    async fn send(&mut self, chunk: String) -> Result<(), DeliveryFailed> {
        self.observer.collected(&chunk);
        self.chunks.push(chunk);
        Ok(())
    }
}

/// SPEC §6.1, ADR 0023 §4: the readiness probe, bounded by what remains of the
/// startup budget and by nothing shorter. The probe is part of startup: an
/// engine may build kernels on its first request (TensorFold 0.6.1 and 0.6.2
/// build four CUDA extensions there for Qwen3.8 dense from an empty cache, found
/// live 2026-10-02), so the transport's 60 s read
/// bound would cut a healthy start short and have it killed mid build. `None`
/// when the budget passed first. Ordinary forwarding keeps its own bounds.
pub(crate) async fn startup_probe(
    engine: &(dyn crate::traits::ChatForward + '_),
    body: &Value,
    budget: Duration,
) -> Option<Result<Value, AdapterError>> {
    tokio::time::timeout(budget, engine.forward_chat_observed(body, &mut NoObserver))
        .await
        .ok()
}

/// The fields an engine streams a reasoning trace in: SGLang and TensorFold
/// send `reasoning_content`; vLLM 0.29 and 0.30 send `reasoning`
/// (`DeltaMessage.reasoning`, vllm/entrypoints/generate/base/protocol.py).
/// SPEC §10: each is relayed, and collected, under the name the engine used.
const REASONING_FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];

/// SPEC §10 (preserve stream events), §13.3 (allowlists): the `delta` keys a
/// streamed chunk may carry. Any other key ends the stream uncertain, so a field
/// is listed only once an engine is shown to emit it on `/v1/chat/completions`.
/// Every `DeltaMessage` field the supported engines serialize on that path:
/// - vLLM 0.29 (`vllm/entrypoints/generate/base/protocol.py:329-333`):
///   `role`, `content`, `reasoning`, `tool_calls`; 0.30 found live 2026-10-02.
/// - SGLang 0.5.20 (`sglang/srt/entrypoints/openai/protocol.py:1262-1266`):
///   `role`, `content`, `reasoning_content`, `tool_calls`.
/// - TensorFold 0.6.0 and 0.6.3 (`tensorfold/server/app.py` `on_delta`,
///   `http.py` `stream_chunk`): `role`, `content`, `reasoning_content`,
///   `tool_calls`.
/// - llama.cpp v0.6.0 (`tools/server/server-chat.cpp`
///   `server_chat_msg_diff_to_json_oaicompat`, `server-task.cpp` opening
///   delta): `role`, `content`, `reasoning_content`, `tool_calls` (ADR 0029
///   §11). Its chunk-level `timings` (on the last chunk) is relayed unchanged.
///
/// Deliberately absent: SGLang's `hidden_states` (protocol.py:1267) is sent only
/// for `return_hidden_states`, a request field this relay refuses; vLLM's
/// `citations` belongs to its Cohere endpoint, not chat completions. Choice
/// fields beside `delta` (`logprobs`, vLLM `stop_reason`/`token_ids`, SGLang
/// `matched_stop`) and chunk fields (`usage` with its details) are relayed
/// unchanged. Table: docs/guide/requests.md "Streamed fields".
const STREAM_DELTA_FIELDS: [&str; 5] = [
    "role",
    "content",
    REASONING_FIELDS[0],
    REASONING_FIELDS[1],
    "tool_calls",
];

/// SPEC §6.1, ADR 0023 §6: a readiness probe's answer is non-empty content
/// or, for a model that reasons first, non-empty reasoning in either field.
pub fn probe_answered(answer: &Value) -> bool {
    let message = &answer["choices"][0]["message"];
    ["content", REASONING_FIELDS[0], REASONING_FIELDS[1]]
        .iter()
        .any(|field| {
            message[*field]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        })
}

/// Assemble one non-streaming response from a completed stream's chunks.
fn assemble(chunks: Vec<String>) -> Result<Value, AdapterError> {
    // A completed stream always carried a chunk; never index blindly.
    let first = chunks.first().ok_or_else(uncertain)?;
    let mut response = serde_json::from_str::<Value>(first).map_err(|_| uncertain())?;
    let mut content = String::new();
    let mut reasoning = [String::new(), String::new()];
    let mut tool_calls = std::collections::BTreeMap::new();
    let mut finish = Value::Null;
    // SPEC §10 "Preserve supported payloads": per-token log probabilities
    // arrive one chunk at a time and are concatenated in stream order, so a
    // collected response carries what the stream carried. `None` until an
    // engine sends any, which keeps the field absent when none was asked for.
    let mut logprobs: Option<Vec<Value>> = None;
    for chunk in chunks {
        let chunk: Value = serde_json::from_str(&chunk).map_err(|_| uncertain())?;
        if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str() {
            content.push_str(text);
        }
        for (field, trace) in REASONING_FIELDS.iter().zip(reasoning.iter_mut()) {
            if let Some(text) = chunk["choices"][0]["delta"][*field].as_str() {
                trace.push_str(text);
            }
        }
        fold_tool_calls(&mut tool_calls, &chunk["choices"][0]["delta"]["tool_calls"]);
        match &chunk["choices"][0]["logprobs"] {
            Value::Null => {}
            Value::Object(block) => match block.get("content") {
                Some(Value::Array(tokens)) => logprobs
                    .get_or_insert_with(Vec::new)
                    .extend(tokens.iter().cloned()),
                None | Some(Value::Null) => {}
                Some(_) => return Err(uncertain()),
            },
            _ => return Err(uncertain()),
        }
        if !chunk["choices"][0]["finish_reason"].is_null() {
            finish = chunk["choices"][0]["finish_reason"].clone();
        }
        // SGLang sends `"usage": null` on tool-call deltas; only a present
        // usage object replaces what was collected.
        if chunk.get("usage").is_some_and(|usage| !usage.is_null()) {
            response["usage"] = chunk["usage"].clone();
        }
        // ADR 0023 §7: TensorFold's statistics ride the final chunk; a
        // collected response carries them as the engine's own would. vLLM's
        // per-request `metrics` ride its final usage chunk the same way
        // (0.30 `ChatCompletionStreamResponse.metrics`), and llama.cpp's
        // `timings` ride its last chunk (ADR 0029 §11; found live 2026-10-09:
        // a collected answer had lost them, so `prefill_ms` was missing).
        for field in ["tensorfold", "metrics", "timings"] {
            if let Some(stats) = chunk.get(field).filter(|v| v.is_object()) {
                response[field] = stats.clone();
            }
        }
    }
    response["object"] = json!("chat.completion");
    // vLLM's first chunk carries `prompt_text` and `prompt_token_ids` (null
    // unless asked for); they describe the stream, not the completion.
    if let Some(fields) = response.as_object_mut() {
        for field in ["prompt_text", "prompt_token_ids"] {
            if fields.get(field).is_some_and(Value::is_null) {
                fields.remove(field);
            }
        }
    }
    // Collecting must not silently discard the trace a streaming caller would
    // have received. The field is omitted entirely when the engine sent none,
    // so a non-reasoning response keeps its existing shape.
    let mut message = json!({"role":"assistant","content":content});
    for (field, trace) in REASONING_FIELDS.iter().zip(reasoning) {
        if !trace.is_empty() {
            message[*field] = json!(trace);
        }
    }
    // SPEC §10 preserves tool calls: the collected message carries them in
    // the OpenAI non-streaming shape, with `content` null when the engine
    // produced only calls.
    if !tool_calls.is_empty() {
        if content.is_empty() {
            message["content"] = Value::Null;
        }
        message["tool_calls"] = Value::Array(tool_calls.into_values().collect());
    }
    let mut choice = json!({"index":0,"message":message,"finish_reason":finish});
    if let Some(tokens) = logprobs {
        choice["logprobs"] = json!({ "content": tokens });
    }
    response["choices"] = json!([choice]);
    Ok(response)
}

impl ChatHttp {
    /// The callback and collecting paths have no caller-side bound, so they
    /// keep the transport's own: 60 s between reads and 300 s overall.
    pub(crate) async fn stream(
        &self,
        body: &Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<StreamEnded, AdapterError> {
        tokio::time::timeout(
            Duration::from_secs(300),
            self.stream_inner(
                body,
                &mut CallbackSink(on_chunk),
                Some(Duration::from_secs(60)),
            ),
        )
        .await
        .map_err(|_| uncertain())?
    }

    /// SPEC §10: the router bounds a relayed stream by the request deadline
    /// and an idle bound between events, measured from `ChatSink::progressed`.
    /// A fixed cap here cut a progressing stream at 300 s and left its lease
    /// uncertain (found live 2026-09-23, matrix M30 vLLM), so there is none.
    pub(crate) async fn stream_async(
        &self,
        body: &Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        self.stream_inner(body, sink, None).await
    }

    /// Send one request to `url` and screen the answer's status: a connection
    /// never established, or a role's `shutting_down` refusal, carried nothing
    /// (`NotAccepted`); an engine's complete invalid-request answer is
    /// `Rejected`, and its complete error answer is `EngineError`. Any other
    /// transport error is `unverified()`. The caller judges every other
    /// response.
    async fn post(
        &self,
        url: &reqwest::Url,
        request: &Value,
        unverified: fn() -> AdapterError,
    ) -> Result<reqwest::Response, AdapterError> {
        let mut send = self.client.post(url.clone()).json(request);
        if let Some(key) = &self.key {
            send = send.bearer_auth(key);
        }
        let response = send.send().await.map_err(|error| {
            // SPEC §10, ADR 0013 §10: a connection that was never established
            // carried no request, so nothing can be running for it and the
            // router may fail over. Any other transport error may follow a sent
            // request and stays uncertain.
            if error.is_connect() {
                AdapterError::NotAccepted("the connection to the serving host was refused".into())
            } else {
                unverified()
            }
        })?;
        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(refused_before_forwarding(response).await);
        }
        // SPEC §10 (found live 2026-09-24): an invalid-request answer is the
        // engine's complete reply, relayed as such rather than as uncertainty.
        let status = response.status().as_u16();
        if rejection_status(status) {
            return Err(rejected_by_engine(response).await);
        }
        // SPEC §10 (amended 2026-10-17, found live): an engine's complete
        // error answer is relayed with its message; the request is cancelling
        // rather than uncertain, so the engine's quiescence settles it.
        if engine_error_status(status) {
            return Err(failed_by_engine(response, unverified).await);
        }
        Ok(response)
    }

    /// ADR 0030 §2: forward one decision request and read its answer whole.
    /// The request goes as one JSON body with `model` replaced by the served
    /// name; the answer comes back with `model` replaced by the public name.
    /// Only a complete JSON object of at most [`DECISION_RESPONSE_LIMIT`]
    /// bytes with an `answers` object ([`decision_answered`]) is completion;
    /// a truncated, oversized, ambiguous or malformed body stays uncertain,
    /// since the engine may have run the request. No wall-clock bound of its
    /// own: the router bounds the whole exchange by the request deadline.
    pub(crate) async fn decide(&self, body: &Value) -> Result<Value, AdapterError> {
        validate_decision_request(body)?;
        let public = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(AdapterError::UnsupportedCapability)?;
        let mut request = body.clone();
        request["model"] = json!(self.model);
        let response = self
            .post(&self.decisions, &request, decision_uncertain)
            .await?;
        if !response.status().is_success() || !content_type_is(&response, "application/json") {
            return Err(decision_uncertain());
        }
        let body = read_whole(response, DECISION_RESPONSE_LIMIT)
            .await
            .ok_or_else(decision_uncertain)?;
        // Identity and evidence must have one interpretation: a duplicated
        // key is not an answer.
        let StrictValue(mut answer) =
            serde_json::from_slice(&body).map_err(|_| decision_uncertain())?;
        if !decision_answered(&answer) {
            return Err(decision_uncertain());
        }
        answer["model"] = json!(public);
        Ok(answer)
    }

    async fn stream_inner(
        &self,
        body: &Value,
        sink: &mut dyn ChatSink,
        read_idle: Option<Duration>,
    ) -> Result<StreamEnded, AdapterError> {
        // SPEC §10 / T21: engine-internal fields never reach the engine, and a
        // parameter the relay cannot answer faithfully is refused before
        // anything is sent (tools are forwarded: SPEC §10 preserves tool calls).
        validate_chat_request(body)?;
        // SPEC §10: a requested prefix-cache partition is forwarded unchanged
        // only to an engine that honours it; anywhere else it would be silently
        // absent, so the request is refused before anything is sent.
        if !self.honours_cache_salt && carries_cache_salt(body) {
            return Err(AdapterError::CacheSaltUnsupported);
        }
        let public = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(AdapterError::UnsupportedCapability)?;
        let mut request = body.clone();
        request["model"] = json!(self.model);
        request["stream"] = json!(true);
        let response = self.post(&self.endpoint, &request, uncertain).await?;
        if !response.status().is_success() || !content_type_is(&response, "text/event-stream") {
            return Err(uncertain());
        }
        let mut stream = response.bytes_stream();
        let mut parser = Parser::new(&self.model, public);
        loop {
            let next = match read_idle {
                Some(idle) => tokio::time::timeout(idle, stream.next())
                    .await
                    .map_err(|_| uncertain())?,
                None => stream.next().await,
            };
            let Some(bytes) = next else { break };
            for byte in bytes.map_err(|_| uncertain())? {
                // A byte can finish at most one event. Hold only that payload
                // while awaiting capacity; never collect a transport chunk's
                // events into a second queue.
                let mut payload = None;
                let done = parser.byte(byte, &mut |chunk| payload = Some(chunk))?;
                // SPEC §10: an event is progress, except a chunk that only
                // opens the reply (`opens_reply_only`).
                if done
                    || payload
                        .as_deref()
                        .is_some_and(|chunk| !opens_reply_only(chunk))
                {
                    sink.progressed();
                }
                if let Some(chunk) = payload {
                    let sink_failed = !matches!(
                        tokio::time::timeout(Duration::from_secs(10), sink.send(chunk)).await,
                        Ok(Ok(()))
                    );
                    // SPEC §10 (amended 2026-10-01): a client that left is not proof the engine
                    // stopped, so the engine is asked to stop: the connection closes here and
                    // the lease stays charged until the engine reports quiescence.
                    if sink_failed {
                        return Ok(StreamEnded::Cancelled);
                    }
                }
                if done {
                    return Ok(StreamEnded::Completed);
                }
            }
        }
        // Even a finish_reason without the protocol terminator is uncertain.
        Err(uncertain())
    }
}

/// SPEC §10 (found live 2026-10-02): whether `chunk` only opens the reply: every
/// choice's delta states the `role` and nothing else that is not null or empty,
/// with no finish reason, no log probabilities and no usage. TensorFold sends
/// this chunk as soon as it accepts a request, before it prefills the prompt;
/// vLLM and SGLang send it with their first output. It is relayed like any
/// chunk, but it is not progress: until the first output, the request deadline
/// bounds the stream, whatever the engine sent first.
pub fn opens_reply_only(chunk: &str) -> bool {
    if !chunk.contains("\"role\"") {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(chunk) else {
        return false;
    };
    let empty = |value: &Value| value.is_null() || value.as_str() == Some("");
    value.get("usage").is_none_or(Value::is_null)
        && value["choices"]
            .as_array()
            .filter(|choices| !choices.is_empty())
            .is_some_and(|choices| {
                choices.iter().all(|choice| {
                    choice["finish_reason"].is_null()
                        && choice["logprobs"].is_null()
                        && choice["delta"].as_object().is_some_and(|delta| {
                            delta.get("role").is_some_and(|role| !role.is_null())
                                && delta
                                    .iter()
                                    .all(|(field, value)| field == "role" || empty(value))
                        })
                })
            })
}

/// SPEC §10: the statuses an OpenAI-compatible engine answers when it rejects a
/// request as invalid before running it (a prompt over its context, an argument
/// or tool choice it cannot serve, a body too large). With a JSON body read in
/// full they are the engine's complete answer, so nothing runs on the request's
/// behalf. A 5xx error answer is [`engine_error_status`]; every other error
/// status stays uncertain: a 408 or a 499 can follow accepted work, and a
/// 401/403 is not the engine judging the request.
pub fn rejection_status(status: u16) -> bool {
    matches!(status, 400 | 413 | 422)
}

/// SPEC §10 (amended 2026-10-17, found live): the statuses of an engine's
/// error answer, every 5xx but 503. With a JSON object body read in full
/// (llama.cpp v0.7.0 answers an input over its batch size with a 500 and its
/// complete JSON error) the engine has answered, but the answer does not
/// prove it stopped, so the request is cancelling: charged until the engine
/// reports quiescence. A 503 keeps its own reading: a role's `shutting_down`
/// refusal is not accepted, any other 503 stays uncertain.
pub fn engine_error_status(status: u16) -> bool {
    (500..600).contains(&status) && status != 503
}

/// The largest rejection body read, and the longest message kept from it.
pub const REJECTION_BODY_LIMIT: usize = 16 * 1024;
const REJECTION_MESSAGE_LIMIT: usize = 512;

/// The code a host ingress answers its own failures with: a 502 when the
/// engine could not be reached or its answer could not be relayed whole. It
/// is not the engine's answer, so it stays uncertain.
pub const INGRESS_FAILURE_CODE: &str = "inference_unavailable";

/// The message of an engine's invalid-request answer: vLLM's `error.message`,
/// SGLang's top-level `message`, a relayed `engine_rejected` body, or the
/// first `msg` of a validation `detail` array (SGLang 0.5.22 answers an
/// invalid decision request with FastAPI's 422, found live 2026-10-17), cut
/// to a bounded length on a character boundary. `None` when the body is not a
/// JSON object, which leaves the answer unproven.
pub fn rejection_message(body: &[u8]) -> Option<String> {
    let object = json_object(body)?;
    Some(answer_message(
        &object,
        "the engine rejected the request as invalid",
    ))
}

/// SPEC §10 (amended 2026-10-17): the message of an engine's error answer,
/// read and bounded as [`rejection_message`] reads a rejection's (llama.cpp's
/// `error.message`, a relayed `engine_error` body). `None` when the body is
/// not a JSON object, or is a host ingress's own failure
/// ([`INGRESS_FAILURE_CODE`]), which leaves the answer unproven.
pub fn engine_error_message(body: &[u8]) -> Option<String> {
    let object = json_object(body)?;
    if object
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        == Some(INGRESS_FAILURE_CODE)
    {
        return None;
    }
    Some(answer_message(&object, "the engine answered with an error"))
}

/// `body` parsed as one JSON object, or `None`.
fn json_object(body: &[u8]) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_slice(body).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

/// The engine's message in its answer `object`, or `fallback` when it names
/// none, cut to [`REJECTION_MESSAGE_LIMIT`] bytes on a character boundary.
fn answer_message(object: &serde_json::Map<String, Value>, fallback: &str) -> String {
    let message = object
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| object.get("message").and_then(Value::as_str))
        .or_else(|| {
            let detail = object.get("detail")?;
            detail
                .as_str()
                .or_else(|| detail.get(0)?.get("msg")?.as_str())
        })
        .unwrap_or(fallback);
    let mut end = message.len().min(REJECTION_MESSAGE_LIMIT);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

/// A response body read whole, or `None` when it is longer than `limit` (by
/// its declared length or as read) or its transfer fails, a truncated body
/// included. No time bound of its own.
async fn read_whole(response: reqwest::Response, limit: usize) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len() + chunk.len() > limit {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// Whether `response` declares the media type `media` (parameters ignored).
fn content_type_is(response: &reqwest::Response, media: &str) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| h.split(';').next().unwrap_or("").trim() == media)
}

/// An engine's answer body read whole, bounded in size
/// ([`REJECTION_BODY_LIMIT`]) and time, or `None` when it is not.
async fn read_answer(response: reqwest::Response) -> Option<Vec<u8>> {
    tokio::time::timeout(
        Duration::from_secs(5),
        read_whole(response, REJECTION_BODY_LIMIT),
    )
    .await
    .ok()
    .flatten()
}

/// Read an invalid-request answer in full (bounded in size and time). Only a
/// complete JSON body is evidence; anything else stays uncertain.
async fn rejected_by_engine(response: reqwest::Response) -> AdapterError {
    let status = response.status().as_u16();
    match read_answer(response)
        .await
        .as_deref()
        .and_then(rejection_message)
    {
        Some(message) => AdapterError::Rejected { status, message },
        None => uncertain(),
    }
}

/// SPEC §10 (amended 2026-10-17): read an engine's error answer in full,
/// within the same bounds as a rejection. Only a complete JSON object is the
/// engine's answer; a truncated, oversized, slow or non-JSON body stays
/// `unverified()`.
async fn failed_by_engine(
    response: reqwest::Response,
    unverified: fn() -> AdapterError,
) -> AdapterError {
    let status = response.status().as_u16();
    match read_answer(response)
        .await
        .as_deref()
        .and_then(engine_error_message)
    {
        Some(message) => AdapterError::EngineError { status, message },
        None => unverified(),
    }
}

/// The largest refusal body read when deciding whether a 503 proves the request
/// never reached the engine.
const REFUSAL_LIMIT: usize = 4 * 1024;

/// SPEC §§4.3, 10: a role that is shutting down refuses new work before it
/// forwards anything, with a 503 whose body names `shutting_down` (see the
/// role admission gate in `capyctl-cli`'s `shutdown.rs`, which wraps every host
/// ingress). Only that exact, bounded answer is evidence the engine never saw the
/// request. Any other 503, an oversized or unreadable body, or a slow one, stays
/// uncertain: an engine's own 503 can arrive after it accepted work.
async fn refused_before_forwarding(response: reqwest::Response) -> AdapterError {
    let read =
        tokio::time::timeout(Duration::from_secs(5), read_whole(response, REFUSAL_LIMIT)).await;
    let Some(body) = read.ok().flatten() else {
        return uncertain();
    };
    let refusal = serde_json::from_slice::<Value>(&body).ok();
    match refusal.as_ref().map(|v| &v["error"]) {
        Some(error)
            if error["code"] == "shutting_down" && error["retryable"] == Value::Bool(true) =>
        {
            AdapterError::NotAccepted("the serving role is shutting down".into())
        }
        _ => uncertain(),
    }
}

/// A forwarder bound to one engine incarnation.
///
/// Intentionally no Debug, for the same reason as `ChatHttp`: the value it holds is
/// an inference credential.
struct EngineForward(ChatHttp);

#[async_trait::async_trait]
impl crate::traits::ChatForward for EngineForward {
    async fn forward_chat_stream_async(
        &self,
        body: &Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        self.0.stream_async(body, sink).await
    }
    async fn forward_chat(&self, body: &Value) -> Result<Value, AdapterError> {
        self.0.collect(body).await
    }
    async fn forward_chat_observed(
        &self,
        body: &Value,
        observer: &mut dyn ChatSink,
    ) -> Result<Value, AdapterError> {
        self.0.collect_observed(body, observer, None).await
    }
    async fn forward_decision(&self, body: &Value) -> Result<Value, AdapterError> {
        self.0.decide(body).await
    }
    async fn forward_chat_stream(
        &self,
        body: &Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<StreamEnded, AdapterError> {
        self.0.stream(body, on_chunk).await
    }
}

/// A forwarder for one running engine: the endpoint it was leased, the name it was
/// launched to serve, and the key that launch was given.
///
/// SPEC §13.3: the key is held only to authenticate to that engine. It is never
/// formatted, logged, or returned, and the value this builds has no Debug so it
/// cannot be printed by accident.
///
/// Built per incarnation rather than per engine family. An endpoint and a key both
/// belong to a single launch, so a forwarder built once at boot would keep
/// addressing a port and presenting a credential that a later launch replaced.
///
/// `honours_cache_salt` is the engine family's answer from
/// [`engine_honours_cache_salt`] (SPEC §10).
pub fn engine_forwarder(
    base: reqwest::Url,
    model: String,
    key: Option<String>,
    honours_cache_salt: bool,
) -> std::sync::Arc<dyn crate::traits::ChatForward> {
    std::sync::Arc::new(EngineForward(ChatHttp::new(
        base,
        model,
        key,
        honours_cache_salt,
    )))
}

struct Parser<'a> {
    backend: &'a str,
    public: &'a str,
    line: Vec<u8>,
    data: String,
    total: usize,
    event_bytes: usize,
    id: Option<String>,
    finished: bool,
}
impl<'a> Parser<'a> {
    fn new(backend: &'a str, public: &'a str) -> Self {
        Self {
            backend,
            public,
            line: vec![],
            data: String::new(),
            total: 0,
            event_bytes: 0,
            id: None,
            finished: false,
        }
    }
    fn byte(
        &mut self,
        byte: u8,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<bool, AdapterError> {
        self.total += 1;
        self.event_bytes += 1;
        if self.total > STREAM_LIMIT || self.event_bytes > EVENT_LIMIT {
            return Err(uncertain());
        }
        if byte != b'\n' {
            self.line.push(byte);
            return Ok(false);
        }
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes).map_err(|_| uncertain())?;
        if line.is_empty() {
            self.event_bytes = 0;
            if self.data.is_empty() {
                return Ok(false);
            }
            let data = std::mem::take(&mut self.data);
            if data.trim() == "[DONE]" {
                return if self.finished {
                    Ok(true)
                } else {
                    Err(uncertain())
                };
            }
            self.chunk(&data, on_chunk)?;
        } else if let Some(data) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
        } else if !line.starts_with(':') {
            return Err(uncertain());
        }
        Ok(false)
    }
    fn chunk(
        &mut self,
        data: &str,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<(), AdapterError> {
        let StrictValue(mut chunk) = serde_json::from_str(data).map_err(|_| uncertain())?;
        // SPEC §10, T19 (found live 2026-10-03): SGLang checks a prompt against
        // its KV pool after the stream opened and answers with one error event.
        // Before any chunk, that event is the engine's complete invalid-request
        // answer, as a 400 body would be; anything later stays uncertain.
        // SPEC §10 (amended 2026-10-17): one with a 5xx code but 503 is its
        // complete error answer, as a 500 body would be.
        if self.id.is_none() {
            if let Some(status) = chunk["error"]["code"]
                .as_u64()
                .and_then(|code| u16::try_from(code).ok())
            {
                if rejection_status(status) {
                    return Err(match rejection_message(data.as_bytes()) {
                        Some(message) => AdapterError::Rejected { status, message },
                        None => uncertain(),
                    });
                }
                if engine_error_status(status) {
                    return Err(match engine_error_message(data.as_bytes()) {
                        Some(message) => AdapterError::EngineError { status, message },
                        None => uncertain(),
                    });
                }
            }
        }
        if chunk["model"].as_str() != Some(self.backend)
            || chunk["object"] != "chat.completion.chunk"
            || chunk["created"].as_u64().is_none()
        {
            return Err(uncertain());
        }
        let id = chunk["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(uncertain)?;
        if self.id.as_deref().is_some_and(|old| old != id) {
            return Err(uncertain());
        }
        self.id = Some(id.into());
        let choices = chunk["choices"].as_array().ok_or_else(uncertain)?;
        if choices.is_empty() && self.finished && chunk["usage"].is_object() {
            // Final usage event contains no generation delta.
        } else {
            if self.finished || choices.len() != 1 || choices[0]["index"].as_u64() != Some(0) {
                return Err(uncertain());
            }
            let delta = choices[0]["delta"].as_object().ok_or_else(uncertain)?;
            // SPEC §10 requires reasoning fields to be preserved. A reasoning model
            // streams its trace as `reasoning_content` deltas (SGLang, TensorFold)
            // or `reasoning` deltas (vLLM 0.29 and 0.30), so rejecting the key
            // fails every chunk and the whole stream, even though the engine is
            // behaving correctly. It is relayed unchanged and validated like
            // `content`; the allowlist stays closed to everything else so unknown
            // fields are still never passed through untested.
            // SPEC §10 also preserves tool calls: `tool_calls` deltas are relayed
            // once their shape is validated (see `valid_tool_call_deltas`).
            // A null role is an absent role: SGLang 0.5.20 serializes every
            // tool-call delta with `"role": null` (found live 2026-09-24).
            if delta
                .keys()
                .any(|k| !STREAM_DELTA_FIELDS.contains(&k.as_str()))
                || delta
                    .get("role")
                    .is_some_and(|r| !r.is_null() && r != "assistant")
                || ["content", REASONING_FIELDS[0], REASONING_FIELDS[1]]
                    .iter()
                    .any(|key| {
                        delta
                            .get(*key)
                            .is_some_and(|value| !value.is_null() && !value.is_string())
                    })
                || delta
                    .get("tool_calls")
                    .is_some_and(|calls| !calls.is_null() && !valid_tool_call_deltas(calls))
            {
                return Err(uncertain());
            }
            match choices[0].get("finish_reason") {
                Some(Value::Null) => {}
                Some(Value::String(s))
                    if matches!(
                        s.as_str(),
                        "stop" | "length" | "content_filter" | "tool_calls"
                    ) =>
                {
                    self.finished = true
                }
                _ => return Err(uncertain()),
            }
        }
        chunk["model"] = json!(self.public);
        on_chunk(chunk.to_string());
        Ok(())
    }
}

/// SPEC §10: the OpenAI streaming tool-call delta shape — an array of objects
/// with an integer `index` and optional `id`, `type` ("function") and
/// `function` (`name`, `arguments` strings). Anything else stays uncertain.
fn valid_tool_call_deltas(calls: &Value) -> bool {
    let optional_string =
        |value: Option<&Value>| value.is_none_or(|v| v.is_null() || v.is_string());
    calls.as_array().is_some_and(|calls| {
        calls.iter().all(|call| {
            call.as_object().is_some_and(|call| {
                call.keys()
                    .all(|k| matches!(k.as_str(), "index" | "id" | "type" | "function"))
                    && call.get("index").and_then(Value::as_u64).is_some()
                    && optional_string(call.get("id"))
                    && call
                        .get("type")
                        .is_none_or(|t| t.is_null() || t == "function")
                    && call.get("function").is_none_or(|function| {
                        function.is_null()
                            || function.as_object().is_some_and(|function| {
                                function
                                    .keys()
                                    .all(|k| matches!(k.as_str(), "name" | "arguments"))
                                    && optional_string(function.get("name"))
                                    && optional_string(function.get("arguments"))
                            })
                    })
            })
        })
    })
}

/// Fold one chunk's validated `tool_calls` deltas into the calls collected so
/// far, keyed by `index`: `id`, `type` and `name` are taken when first sent,
/// `arguments` fragments are concatenated in stream order.
fn fold_tool_calls(calls: &mut std::collections::BTreeMap<u64, Value>, deltas: &Value) {
    for delta in deltas.as_array().into_iter().flatten() {
        let Some(index) = delta["index"].as_u64() else {
            continue;
        };
        let call = calls.entry(index).or_insert_with(|| {
            json!({"id": Value::Null, "type": "function",
                   "function": {"name": "", "arguments": ""}})
        });
        if let Some(id) = delta["id"].as_str() {
            call["id"] = json!(id);
        }
        if let Some(name) = delta["function"]["name"].as_str() {
            if call["function"]["name"].as_str().is_some_and(str::is_empty) {
                call["function"]["name"] = json!(name);
            }
        }
        if let Some(fragment) = delta["function"]["arguments"].as_str() {
            let joined = format!(
                "{}{fragment}",
                call["function"]["arguments"].as_str().unwrap_or_default()
            );
            call["function"]["arguments"] = json!(joined);
        }
    }
}

// serde_json::Value otherwise accepts duplicate keys with last-value-wins.
// Identity and terminal fields must have one unambiguous interpretation.
struct StrictValue(Value);
impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("unambiguous JSON")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = vec![];
                while let Some(StrictValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, StrictValue(value))) =
                    map.next_entry::<String, StrictValue>()?
                {
                    if values.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{assemble, probe_answered, upstream};
    use crate::traits::AdapterError;
    use serde_json::{json, Value};

    // T41 (ADR 0023 §7): the final chunk's `tensorfold` object survives
    // collection, beside usage.
    #[test]
    fn a_collected_response_keeps_the_tensorfold_object() {
        let chunk = |delta: Value, finish: Value| {
            json!({"id": "c", "object": "chat.completion.chunk",
            "created": 1, "model": "m", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}).to_string()
        };
        let mut end: Value = serde_json::from_str(&chunk(json!({}), json!("stop"))).unwrap();
        end["tensorfold"] = json!({"accepted": 2});
        end["usage"] = json!({"total_tokens": 4});
        let response = assemble(vec![
            chunk(json!({"role": "assistant"}), Value::Null),
            chunk(json!({"content": "hi"}), Value::Null),
            end.to_string(),
        ])
        .unwrap();
        assert_eq!(response["tensorfold"], json!({"accepted": 2}));
        assert_eq!(response["usage"]["total_tokens"], 4);
        assert_eq!(response["choices"][0]["message"]["content"], "hi");
    }

    // T41 (ADR 0023 §7): with `stream_options.include_usage`, TensorFold 0.6.3
    // to 0.6.5 send usage as their own final chunk with `choices: []` after the
    // finish chunk that carries the `tensorfold` object (found live 2026-10-02).
    // Collection keeps both.
    #[test]
    fn a_collected_response_keeps_a_separate_usage_chunk() {
        let chunk = |delta: Value, finish: Value| {
            json!({"id": "c", "object": "chat.completion.chunk",
            "created": 1, "model": "m", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}).to_string()
        };
        let mut end: Value = serde_json::from_str(&chunk(json!({}), json!("stop"))).unwrap();
        end["tensorfold"] = json!({"accepted": 2});
        let usage = json!({"id": "c", "object": "chat.completion.chunk", "created": 1,
            "model": "m", "choices": [], "usage": {"total_tokens": 4}});
        let response = assemble(vec![
            chunk(json!({"role": "assistant"}), Value::Null),
            chunk(json!({"content": "hi"}), Value::Null),
            end.to_string(),
            usage.to_string(),
        ])
        .unwrap();
        assert_eq!(response["tensorfold"], json!({"accepted": 2}));
        assert_eq!(response["usage"]["total_tokens"], 4);
        assert_eq!(response["choices"][0]["message"]["content"], "hi");
        assert_eq!(response["choices"][0]["finish_reason"], "stop");
    }

    // T40 (owner decision 2026-10-09): vLLM 0.30 with `--enable-per-request-metrics`
    // puts its `metrics` object on the final usage chunk (`serving.py`
    // `final_usage_chunk`); a collected response carries it top-level, where
    // its own non-streaming answer does.
    #[test]
    fn a_collected_response_keeps_the_vllm_metrics_object() {
        let usage = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":12,"total_tokens":20,"completion_tokens":8},"metrics":{"time_to_first_token_ms":41.5,"generation_time_ms":70.0,"queue_time_ms":0.8,"mean_itl_ms":10.0,"tokens_per_second":71.2}}"#;
        let response = assemble(vec![
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],"prompt_token_ids":null}"#.to_owned(),
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"logprobs":null,"finish_reason":"stop","stop_reason":null}]}"#.to_owned(),
            usage.to_owned(),
        ])
        .unwrap();
        assert_eq!(response["metrics"]["time_to_first_token_ms"], 41.5);
        assert_eq!(response["metrics"]["mean_itl_ms"], 10.0);
        assert_eq!(response["usage"]["completion_tokens"], 8);
        assert_eq!(response["choices"][0]["message"]["content"], "hi");
    }

    // T40 T42 (ADR 0029 §11, found live 2026-10-09): llama.cpp's `timings`
    // ride its last chunk; a collected response carries them top-level, where
    // llama-server's own non-streaming answer does.
    #[test]
    fn a_collected_response_keeps_the_llamacpp_timings() {
        let last = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":14,"total_tokens":30,"completion_tokens":16},"timings":{"cache_n":13,"prompt_n":1,"prompt_ms":6.927,"predicted_n":16,"predicted_ms":114.765,"predicted_per_second":130.7}}"#;
        let response = assemble(vec![
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}]}"#.to_owned(),
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#.to_owned(),
            last.to_owned(),
        ])
        .unwrap();
        assert_eq!(response["timings"]["prompt_ms"], 6.927);
        assert_eq!(response["timings"]["predicted_per_second"], 130.7);
        assert_eq!(response["usage"]["completion_tokens"], 16);
        assert_eq!(response["choices"][0]["message"]["content"], "hi");
    }

    // T41 T22 (ADR 0023 §6): reasoning alone answers the probe; nothing does not.
    #[test]
    fn a_reasoning_only_answer_answers_the_probe() {
        assert!(probe_answered(
            &json!({"choices": [{"message": {"content": "", "reasoning_content": "ok"}}]})
        ));
        assert!(probe_answered(
            &json!({"choices": [{"message": {"content": "Ready."}}]})
        ));
        assert!(!probe_answered(
            &json!({"choices": [{"message": {"content": ""}}]})
        ));
        assert!(!probe_answered(&json!({})));
        // vLLM 0.29 and 0.30 name the trace `reasoning` (found live 2026-10-02).
        assert!(probe_answered(
            &json!({"choices": [{"message": {"content": "", "reasoning": "ok"}}]})
        ));
        assert!(!probe_answered(
            &json!({"choices": [{"message": {"content": "", "reasoning": ""}}]})
        ));
    }

    async fn answering(status: u16, body: serde_json::Value) -> reqwest::Url {
        use axum::response::IntoResponse;
        let status = axum::http::StatusCode::from_u16(status).unwrap();
        let router = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || {
                let body = body.clone();
                async move { (status, axum::Json(body)).into_response() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}").parse().unwrap()
    }

    // T17 T38: a host ingress that is shutting down refuses before forwarding;
    // that exact answer is evidence of non-acceptance. Any other 503 stays
    // uncertain, because an engine can answer 503 after accepting work, and
    // the same body with a 500 is an engine's error answer, never a refusal.
    #[tokio::test]
    async fn only_a_shutting_down_refusal_proves_the_engine_never_saw_the_request() {
        let request = serde_json::json!({"model":"m","messages":[]});
        let refusal = serde_json::json!({"error":{"code":"shutting_down","message":"restarting","retryable":true}});
        let forward =
            crate::forward::engine_forwarder(answering(503, refusal).await, "m".into(), None, true);
        assert!(matches!(
            forward.forward_chat(&request).await,
            Err(AdapterError::NotAccepted(_))
        ));
        for body in [
            serde_json::json!({"error":{"code":"overloaded","retryable":true}}),
            serde_json::json!({"error":{"code":"shutting_down"}}),
            serde_json::json!({"object":"error","code":503}),
        ] {
            let forward = crate::forward::engine_forwarder(
                answering(503, body).await,
                "m".into(),
                None,
                true,
            );
            assert!(matches!(
                forward.forward_chat(&request).await,
                Err(AdapterError::Uncertain(_))
            ));
        }
        let refusal = serde_json::json!({"error":{"code":"shutting_down","retryable":true}});
        let forward =
            crate::forward::engine_forwarder(answering(500, refusal).await, "m".into(), None, true);
        assert!(matches!(
            forward.forward_chat(&request).await,
            Err(AdapterError::EngineError { status: 500, .. })
        ));
    }

    // T19 T17 (SPEC §10, found live 2026-09-24): an engine that rejects a
    // request as invalid (400, 413, 422 with a JSON body read in full) has
    // answered it; nothing runs on its behalf, so the rejection is terminal
    // evidence and carries the engine's own message, from vLLM's
    // `error.message`, SGLang's top-level `message`, or the host ingress's
    // relayed `engine_rejected` body. A rejection that cannot be read as
    // JSON, and every other status but an engine error answer, stays
    // uncertain.
    #[tokio::test]
    async fn an_invalid_request_rejection_is_terminal_and_keeps_the_engine_message() {
        let request = serde_json::json!({"model":"m","messages":[]});
        for (status, body, needle) in [
            (
                400,
                serde_json::json!({"error":{"message":"This model's maximum context length is 16384 tokens.","type":"BadRequestError","code":400}}),
                "maximum context length",
            ),
            (
                422,
                serde_json::json!({"object":"error","message":"tool_choice requires a parser","code":422}),
                "tool_choice",
            ),
            (
                413,
                serde_json::json!({"error":{"code":"engine_rejected","message":"prompt too large"}}),
                "prompt too large",
            ),
        ] {
            let forward = crate::forward::engine_forwarder(
                answering(status, body).await,
                "m".into(),
                None,
                true,
            );
            match forward.forward_chat(&request).await {
                Err(AdapterError::Rejected {
                    status: got,
                    message,
                }) => {
                    assert_eq!(got, status);
                    assert!(message.contains(needle), "{message}");
                }
                other => panic!("status {status}: {:?}", other.map(|_| ())),
            }
        }
        let long = "x".repeat(4000);
        let forward = crate::forward::engine_forwarder(
            answering(400, serde_json::json!({"error":{"message": long}})).await,
            "m".into(),
            None,
            true,
        );
        match forward.forward_chat(&request).await {
            Err(AdapterError::Rejected { message, .. }) => {
                assert!(message.len() <= 512, "{}", message.len())
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
        for (status, body) in [
            (400, serde_json::json!("not an object")),
            (401, serde_json::json!({"error":{"message":"Unauthorized"}})),
            (500, serde_json::json!("not an object")),
        ] {
            let forward = crate::forward::engine_forwarder(
                answering(status, body).await,
                "m".into(),
                None,
                true,
            );
            assert!(
                matches!(
                    forward.forward_chat(&request).await,
                    Err(AdapterError::Uncertain(_))
                ),
                "status {status}"
            );
        }
    }

    /// How [`replying`] answers every path.
    enum Raw {
        /// The whole body.
        Whole(Vec<u8>),
        /// These bytes of a body, then a failed transfer.
        Truncated(Vec<u8>),
    }

    /// An engine answering `status` with a JSON body on every path.
    async fn replying(status: u16, raw: Raw) -> reqwest::Url {
        let status = axum::http::StatusCode::from_u16(status).unwrap();
        let raw = std::sync::Arc::new(raw);
        let router = axum::Router::new().fallback(move || {
            let raw = raw.clone();
            async move {
                let body = match &*raw {
                    Raw::Whole(bytes) => axum::body::Body::from(bytes.clone()),
                    Raw::Truncated(bytes) => {
                        axum::body::Body::from_stream(futures::stream::iter([
                            Ok(axum::body::Bytes::from(bytes.clone())),
                            Err(std::io::Error::other("the engine went away")),
                        ]))
                    }
                };
                axum::response::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}").parse().unwrap()
    }

    // T17 T19 T43 (SPEC §10, amended 2026-10-17, found live): llama.cpp v0.7.0
    // answers an input over its batch size with a 500 and its complete JSON
    // error. Every 5xx but 503 whose body is read whole as a JSON object is
    // the engine's error answer on the chat, stream and decision paths alike,
    // carrying the engine's bounded message. A truncated, oversized or
    // non-JSON body stays uncertain.
    #[tokio::test]
    async fn an_engine_error_answer_carries_the_engine_message() {
        use super::{engine_forwarder, REJECTION_BODY_LIMIT};
        let llama = br#"{"error":{"code":500,"message":"input (8195 tokens) is too large to process. increase the physical batch size","type":"server_error"}}"#;
        let chat = json!({"model":"p","messages":[]});
        let decision = json!({"model":"p","state":"s",
            "questions":{"q":{"type":"noul","instructions":"x"}}});
        for status in [500, 502, 504] {
            let forward = engine_forwarder(
                replying(status, Raw::Whole(llama.to_vec())).await,
                "m".into(),
                None,
                true,
            );
            let mut relayed = Vec::new();
            for result in [
                forward.forward_chat(&chat).await.map(|_| ()),
                forward
                    .forward_chat_stream(&chat, &mut |chunk| relayed.push(chunk))
                    .await
                    .map(|_| ()),
                forward.forward_decision(&decision).await.map(|_| ()),
            ] {
                match result {
                    Err(AdapterError::EngineError {
                        status: got,
                        message,
                    }) => {
                        assert_eq!(got, status);
                        assert!(message.starts_with("input (8195 tokens)"), "{message}");
                    }
                    other => panic!("status {status}: {other:?}"),
                }
            }
            assert!(relayed.is_empty(), "{relayed:?}");
        }
        let long = json!({"error":{"message":"é".repeat(4000)}}).to_string();
        let forward = engine_forwarder(
            replying(500, Raw::Whole(long.into_bytes())).await,
            "m".into(),
            None,
            true,
        );
        match forward.forward_chat(&chat).await {
            Err(AdapterError::EngineError { message, .. }) => {
                assert!(message.len() <= 512, "{}", message.len())
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
        let oversized = json!({"error":{"message":"x".repeat(REJECTION_BODY_LIMIT)}}).to_string();
        // A host ingress's own failure is not the engine's answer.
        let ingress = json!({"error":{"code":super::INGRESS_FAILURE_CODE,
            "message":"Inference request is not authorized or could not complete"}});
        for (status, raw) in [
            (500, Raw::Truncated(llama[..llama.len() / 2].to_vec())),
            (500, Raw::Whole(llama[..llama.len() / 2].to_vec())),
            (500, Raw::Whole(oversized.into_bytes())),
            (500, Raw::Whole(b"[\"not an object\"]".to_vec())),
            (502, Raw::Whole(ingress.to_string().into_bytes())),
            // A 503 keeps its own reading: not a shutting-down refusal.
            (503, Raw::Whole(llama.to_vec())),
        ] {
            let forward = engine_forwarder(replying(status, raw).await, "m".into(), None, true);
            for result in [
                forward.forward_chat(&chat).await.map(|_| ()),
                forward.forward_decision(&decision).await.map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(AdapterError::Uncertain(_))),
                    "{status}: {result:?}"
                );
            }
        }
    }

    // T38, ADR 0013 §10: a refused connection carried no request, so it is
    // evidence of non-acceptance and the router may fail over; a connection
    // that was accepted and then dropped mid-request stays uncertain.
    #[tokio::test]
    async fn a_refused_connection_is_not_accepted_and_a_dropped_one_is_uncertain() {
        let request = serde_json::json!({"model":"m","messages":[]});
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        let forward = crate::forward::engine_forwarder(
            format!("http://{address}").parse().unwrap(),
            "m".into(),
            None,
            true,
        );
        assert!(matches!(
            forward.forward_chat(&request).await,
            Err(AdapterError::NotAccepted(_))
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::AsyncReadExt;
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                drop(socket);
            }
        });
        let forward = crate::forward::engine_forwarder(
            format!("http://{address}").parse().unwrap(),
            "m".into(),
            None,
            true,
        );
        assert!(matches!(
            forward.forward_chat(&request).await,
            Err(AdapterError::Uncertain(_))
        ));
    }

    /// SPEC §10, T19 T21: a chat request carries the OpenAI chat fields and the
    /// supported sampling, structured-output, reasoning and multimodal
    /// extensions. Engine-internal fields (request ids, adapters, hidden
    /// states, custom logit processors, disaggregation bootstrap, KV transfer,
    /// engine extras, scheduling priority, server-side templates) are refused
    /// before anything is sent, never silently dropped.
    // T19 T21
    #[tokio::test]
    async fn engine_internal_request_fields_are_refused_before_forwarding() {
        use super::chat_request_allowed;
        for allowed in [
            serde_json::json!({"model":"m","messages":[],"temperature":0,"max_tokens":8}),
            serde_json::json!({"model":"m","messages":[],"response_format":{"type":"json_object"},
                               "chat_template_kwargs":{"enable_thinking":false},"top_k":20,
                               "stream_options":{"include_usage":true},"logprobs":true,
                               "separate_reasoning":true,"guided_json":{},"seed":1}),
        ] {
            assert!(chat_request_allowed(&allowed), "{allowed}");
        }
        for field in [
            "rid",
            "lora_path",
            "return_hidden_states",
            "custom_logit_processor",
            "bootstrap_host",
            "bootstrap_port",
            "bootstrap_room",
            "kv_transfer_params",
            "vllm_xargs",
            "priority",
            "chat_template",
            "logits_processors",
            "unknown_engine_field",
        ] {
            let mut body = serde_json::json!({"model":"m","messages":[]});
            body[field] = serde_json::json!(1);
            assert!(!chat_request_allowed(&body), "{field}");
            let forward = crate::forward::engine_forwarder(
                answering(200, serde_json::json!({})).await,
                "m".into(),
                None,
                true,
            );
            assert!(
                matches!(
                    forward.forward_chat(&body).await,
                    Err(AdapterError::PolicyDenied)
                ),
                "{field}"
            );
        }
        assert!(!chat_request_allowed(&serde_json::json!([
            "not", "an", "object"
        ])));
    }

    /// An engine that records each request body and answers with `sse`.
    async fn recording(
        sse: String,
    ) -> (
        reqwest::Url,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        let router = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                record.lock().unwrap().push(body);
                let sse = sse.clone();
                async move { ([("content-type", "text/event-stream")], sse) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        (format!("http://{address}").parse().unwrap(), seen)
    }

    fn tool_chunk(delta: serde_json::Value, finish: serde_json::Value) -> String {
        let chunk = serde_json::json!({"id":"c","object":"chat.completion.chunk","created":1,
            "model":"m","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        format!("data: {chunk}\n\n")
    }

    /// SPEC §10 (found live 2026-10-03 with vLLM 0.29): a non-streaming request
    /// is collected from a stream, and vLLM streams usage only when asked, so
    /// the collection asks for it (`stream_options.include_usage`) and the
    /// response carries `usage`, as the engine's own non-streaming answer does.
    /// The null `prompt_text` and `prompt_token_ids` of vLLM's first chunk are
    /// not carried into the completion.
    // T19
    #[tokio::test]
    async fn a_collected_response_asks_for_and_carries_usage() {
        let first = serde_json::json!({"id":"c","object":"chat.completion.chunk","created":1,
            "model":"m","prompt_text":null,"prompt_token_ids":null,
            "choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]});
        let usage = serde_json::json!({"id":"c","object":"chat.completion.chunk","created":1,
            "model":"m","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}});
        let sse = [
            format!("data: {first}\n\n"),
            tool_chunk(
                serde_json::json!({"content":"hi"}),
                serde_json::json!("stop"),
            ),
            format!("data: {usage}\n\n"),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat();
        let (url, seen) = recording(sse).await;
        let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
        let request = serde_json::json!({"model":"public","messages":[]});
        let response = forward.forward_chat(&request).await.unwrap();
        assert_eq!(response["usage"]["total_tokens"], 4, "{response}");
        assert_eq!(response["choices"][0]["message"]["content"], "hi");
        assert!(response.get("prompt_text").is_none(), "{response}");
        assert!(response.get("prompt_token_ids").is_none(), "{response}");
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(sent["stream_options"]["include_usage"], true, "{sent}");
        // A streaming caller's own stream options are relayed as sent.
        let mut relayed = Vec::new();
        forward
            .forward_chat_stream(&request, &mut |chunk| relayed.push(chunk))
            .await
            .unwrap();
        let sent = seen.lock().unwrap()[1].clone();
        assert!(sent.get("stream_options").is_none(), "{sent}");
    }

    /// SPEC §10 "preserve ... tool calls": `tools` and `tool_choice` reach the
    /// engine unchanged, streamed `tool_calls` deltas are relayed, and a
    /// non-streaming response assembles them (arguments concatenated in order,
    /// `finish_reason: tool_calls`).
    // T19
    #[tokio::test]
    async fn tool_requests_are_forwarded_and_tool_calls_are_preserved() {
        let sse = [
            tool_chunk(serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_1",
                "type":"function","function":{"name":"get_weather","arguments":""}}]}), serde_json::Value::Null),
            tool_chunk(serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}),
                serde_json::Value::Null),
            tool_chunk(serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}),
                serde_json::json!("tool_calls")),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat();
        let (url, seen) = recording(sse).await;
        let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
        let tools = serde_json::json!([{"type":"function","function":{"name":"get_weather",
            "parameters":{"type":"object"}}}]);
        let request = serde_json::json!({"model":"public","messages":[],"tools":tools,
            "tool_choice":"auto","parallel_tool_calls":false});
        let response = forward.forward_chat(&request).await.unwrap();
        let message = &response["choices"][0]["message"];
        assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(message["content"], serde_json::Value::Null);
        assert_eq!(
            message["tool_calls"],
            serde_json::json!([{"id":"call_1","type":"function",
            "function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}])
        );
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(sent["tools"], tools);
        assert_eq!(sent["tool_choice"], "auto");
        let mut relayed = Vec::new();
        let end = forward
            .forward_chat_stream(&request, &mut |chunk| relayed.push(chunk))
            .await
            .unwrap();
        assert_eq!(end, crate::traits::StreamEnded::Completed);
        assert_eq!(relayed.len(), 3);
        assert!(relayed[0].contains("get_weather"), "{}", relayed[0]);
    }

    /// SPEC §10, T19 (found live 2026-09-24): SGLang 0.5.20 serializes its
    /// tool-call deltas through a pydantic model without dropping unset
    /// fields, so every tool-call chunk carries `"role": null`, `"content":
    /// null` and `"reasoning_content": null` beside the call, plus `logprobs`
    /// and `matched_stop` on the choice and `"usage": null` on the chunk. Its
    /// finish chunk carries `"reasoning_content": null` alone. These are the
    /// exact bytes SGLang 0.5.20 emits for `--tool-call-parser qwen25`
    /// (`serving_chat._process_tool_call_stream`, `sse_utils.build_sse_content`);
    /// a null role is an absent role, not a different one.
    // T19
    #[tokio::test]
    async fn sglang_tool_call_deltas_with_null_fields_are_relayed() {
        let sse = [
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"reasoning_content":null,"role":"assistant","content":""},"logprobs":null,"finish_reason":null,"matched_stop":null}]}"#,
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"id":"call_x","index":0,"type":"function","function":{"name":"get_weather","arguments":""}}]},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}"#,
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"id":null,"index":0,"type":"function","function":{"name":null,"arguments":"{\"city\": \"Paris\"}"}}]},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}"#,
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"reasoning_content":null},"logprobs":null,"finish_reason":"tool_calls","matched_stop":null}]}"#,
            "data: [DONE]",
        ]
        .map(|line| format!("{line}\n\n"))
        .concat();
        let (url, _) = recording(sse).await;
        let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
        let request = serde_json::json!({"model":"public","messages":[],
            "tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"get_weather"}}});
        let mut relayed = Vec::new();
        let end = forward
            .forward_chat_stream(&request, &mut |chunk| relayed.push(chunk))
            .await
            .unwrap();
        assert_eq!(end, crate::traits::StreamEnded::Completed);
        assert_eq!(relayed.len(), 4);
        let response = forward.forward_chat(&request).await.unwrap();
        assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            response["choices"][0]["message"]["tool_calls"],
            serde_json::json!([{
            "id":"call_x","type":"function",
            "function":{"name":"get_weather","arguments":"{\"city\": \"Paris\"}"}}])
        );
    }

    /// Streams `lines` (each one SSE `data:` event) through the engine
    /// forwarder and returns the stream's end and the relayed chunks.
    async fn relay_stream(
        lines: &[&str],
    ) -> (
        Result<crate::traits::StreamEnded, crate::traits::AdapterError>,
        Vec<String>,
    ) {
        let sse = lines
            .iter()
            .map(|line| format!("{line}\n\n"))
            .collect::<String>();
        let (url, _) = recording(sse).await;
        let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
        let request = serde_json::json!({"model":"public","messages":[],
            "stream_options":{"include_usage":true}});
        let mut relayed = Vec::new();
        let end = forward
            .forward_chat_stream(&request, &mut |chunk| relayed.push(chunk))
            .await;
        (end, relayed)
    }

    /// SPEC §10, §13.3, T19: every delta, choice and usage field vLLM 0.29
    /// serializes on a streamed chat completion survives the relay: `role`,
    /// `reasoning`, `content`, `tool_calls` deltas
    /// (generate/base/protocol.py:329-333), the choice's `logprobs`,
    /// `stop_reason` and `token_ids` (chat_completion/protocol.py:151-162), and
    /// usage with `prompt_tokens_details.cached_tokens` and
    /// `completion_tokens_details` (serve/engine/protocol.py:96-115).
    // T19
    #[tokio::test]
    async fn vllm_stream_fields_survive_the_relay() {
        let usage = r#""usage":{"prompt_tokens":12,"total_tokens":20,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":8},"completion_tokens_details":{"reasoning_tokens":3}}"#;
        let usage_chunk = format!(
            r#"data: {{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],{usage}}}"#
        );
        let (end, relayed) = relay_stream(&[
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"reasoning":"think"},"logprobs":null,"finish_reason":null,"token_ids":[11]}]}"#,
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"Hi"},"logprobs":{"content":[{"token":"Hi","logprob":-0.1,"bytes":[72,105],"top_logprobs":[]}]},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_1","type":"function","index":0,"function":{"name":"f","arguments":"{}"}}]},"logprobs":null,"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":""},"logprobs":null,"finish_reason":"tool_calls","stop_reason":null}]}"#,
            &usage_chunk,
            "data: [DONE]",
        ])
        .await;
        assert_eq!(end.unwrap(), crate::traits::StreamEnded::Completed);
        assert_eq!(relayed.len(), 6);
        assert!(
            relayed[1].contains(r#""reasoning":"think""#),
            "{}",
            relayed[1]
        );
        assert!(relayed[1].contains(r#""token_ids":[11]"#), "{}", relayed[1]);
        assert!(
            relayed[4].contains(r#""stop_reason":null"#),
            "{}",
            relayed[4]
        );
        assert!(
            relayed[5].contains(r#""cached_tokens":8"#),
            "{}",
            relayed[5]
        );
        assert!(
            relayed[5].contains(r#""reasoning_tokens":3"#),
            "{}",
            relayed[5]
        );
    }

    /// SPEC §10, §13.3, T19: SGLang 0.5.20's streamed fields survive:
    /// `reasoning_content` and null siblings (protocol.py:1262-1266), the
    /// choice's `matched_stop` (protocol.py:1286, set on the finish chunk by
    /// serving_chat.py:1939-1947), and the usage chunk's
    /// `prompt_tokens_details.cached_tokens` and `reasoning_tokens`
    /// (protocol.py:188-214, `--enable-cache-report`).
    // T19
    #[tokio::test]
    async fn sglang_stream_fields_survive_the_relay() {
        let (end, relayed) = relay_stream(&[
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":null,"tool_calls":null},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}"#,
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":"think","tool_calls":null},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}"#,
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":null,"content":"Hi","reasoning_content":null,"tool_calls":null},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}"#,
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"logprobs":null,"finish_reason":"stop","matched_stop":151645}],"usage":null}"#,
            r#"data: {"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":12,"total_tokens":20,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":8},"reasoning_tokens":3}}"#,
            "data: [DONE]",
        ])
        .await;
        assert_eq!(end.unwrap(), crate::traits::StreamEnded::Completed);
        assert_eq!(relayed.len(), 5);
        assert!(
            relayed[3].contains(r#""matched_stop":151645"#),
            "{}",
            relayed[3]
        );
        assert!(
            relayed[4].contains(r#""cached_tokens":8"#),
            "{}",
            relayed[4]
        );
    }

    /// SPEC §10, §13.3, T19: TensorFold 0.6.x streams `role`, `content`,
    /// `reasoning_content` and `tool_calls` deltas (app.py `on_delta`, http.py
    /// `stream_chunk`), puts its run statistics beside `choices` on the finish
    /// chunk (http.py `response_extras`: `exact_mode`, `tensorfold`,
    /// `speculative`), and sends usage with `prompt_tokens_details` and
    /// `completion_tokens_details` in its own chunk. All of it survives.
    // T19
    #[tokio::test]
    async fn tensorfold_stream_fields_survive_the_relay() {
        let (end, relayed) = relay_stream(&[
            r#"data: {"id":"chatcmpl-t","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-t","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"reasoning_content":"think"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-t","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-t","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"exact_mode":"target-verified","tensorfold":{"tokens_per_second":40.5,"prefill_seconds":0.2,"time_to_first_token":0.3}}"#,
            r#"data: {"id":"chatcmpl-t","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20,"prompt_tokens_details":{"cached_tokens":8},"completion_tokens_details":{"reasoning_tokens":3}}}"#,
            "data: [DONE]",
        ])
        .await;
        assert_eq!(end.unwrap(), crate::traits::StreamEnded::Completed);
        assert_eq!(relayed.len(), 5);
        assert!(
            relayed[3].contains(r#""tokens_per_second":40.5"#),
            "{}",
            relayed[3]
        );
        assert!(
            relayed[4].contains(r#""cached_tokens":8"#),
            "{}",
            relayed[4]
        );
    }

    /// SPEC §13.3, T19: the delta allowlist stays closed. Fields an engine can
    /// stream only on paths this relay does not serve (SGLang `hidden_states`,
    /// vLLM Cohere `citations`) and any unknown key still end the stream
    /// uncertain instead of being relayed untested.
    // T19
    #[tokio::test]
    async fn unlisted_delta_keys_still_end_the_stream_uncertain() {
        for key in ["hidden_states", "citations", "audio"] {
            let chunk = format!(
                r#"data: {{"id":"abc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{{"index":0,"delta":{{"content":"Hi","{key}":[1]}},"finish_reason":null}}]}}"#
            );
            let (end, relayed) = relay_stream(&[&chunk, "data: [DONE]"]).await;
            assert!(end.is_err(), "{key}: {end:?}");
            assert!(relayed.is_empty(), "{key}");
        }
    }

    /// SPEC §10, T19 (found live 2026-10-03): SGLang checks a prompt against its
    /// KV pool after the stream opened and answers with one error event, then
    /// `[DONE]`. Before any chunk that event is the engine's complete
    /// invalid-request answer, so it is relayed as a rejection (a 400, or an
    /// in-band `engine_rejected`), not as an unverified completion. SPEC §10
    /// (amended 2026-10-17): one with a 5xx code but 503 is the engine's
    /// complete error answer. An error event after output, or with any other
    /// code, stays uncertain.
    // T19
    #[tokio::test]
    async fn an_error_event_before_any_output_is_the_engine_rejection() {
        let error = |code: u16| {
            let event = serde_json::json!({"error":{"object":"error",
                "message":"Input length (258174 tokens) exceeds the maximum allowed length (186353 tokens). Use a shorter input or enable --allow-auto-truncate.",
                "type":"BAD_REQUEST","param":null,"code":code}});
            format!("data: {event}\n\n")
        };
        let request = serde_json::json!({"model":"p","messages":[]});
        let sse = [error(400), "data: [DONE]\n\n".to_owned()].concat();
        let (url, _) = recording(sse).await;
        let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
        match forward.forward_chat(&request).await {
            Err(AdapterError::Rejected { status, message }) => {
                assert_eq!(status, 400);
                assert!(
                    message.starts_with("Input length (258174 tokens)"),
                    "{message}"
                );
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
        let mut relayed = Vec::new();
        let streamed = forward
            .forward_chat_stream(&request, &mut |chunk| relayed.push(chunk))
            .await;
        assert!(
            matches!(streamed, Err(AdapterError::Rejected { status: 400, .. })),
            "{:?}",
            streamed.map(|_| ())
        );
        assert!(relayed.is_empty(), "{relayed:?}");
        for sse in [
            // Output already reached the client: the answer is not a rejection.
            [
                tool_chunk(serde_json::json!({"content":"hi"}), serde_json::Value::Null),
                error(400),
                "data: [DONE]\n\n".to_owned(),
            ]
            .concat(),
            // A 503 keeps its own reading; in-band it proves nothing.
            [error(503), "data: [DONE]\n\n".to_owned()].concat(),
        ] {
            let (url, _) = recording(sse).await;
            let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
            let result = forward.forward_chat(&request).await;
            assert!(
                matches!(result, Err(AdapterError::Uncertain(_))),
                "{:?}",
                result.map(|_| ())
            );
        }
        let sse = [error(500), "data: [DONE]\n\n".to_owned()].concat();
        let (url, _) = recording(sse).await;
        let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
        let mut relayed = Vec::new();
        let streamed = forward
            .forward_chat_stream(&request, &mut |chunk| relayed.push(chunk))
            .await;
        assert!(
            matches!(streamed, Err(AdapterError::EngineError { status: 500, .. })),
            "{:?}",
            streamed.map(|_| ())
        );
        assert!(relayed.is_empty(), "{relayed:?}");
    }

    /// SPEC §10, T19: a malformed `tool_calls` delta is not relayed as success.
    // T19
    #[tokio::test]
    async fn a_malformed_tool_call_delta_stays_uncertain() {
        for bad in [
            serde_json::json!({"tool_calls":[{"id":"x"}]}),
            serde_json::json!({"tool_calls":[{"index":0,"type":"shell"}]}),
            serde_json::json!({"tool_calls":[{"index":0,"function":{"name":1}}]}),
            serde_json::json!({"tool_calls":"x"}),
            serde_json::json!({"role":"user","tool_calls":[{"index":0}]}),
            serde_json::json!({"role":1,"tool_calls":[{"index":0}]}),
        ] {
            let sse = [
                tool_chunk(bad.clone(), serde_json::json!("tool_calls")),
                "data: [DONE]\n\n".to_owned(),
            ]
            .concat();
            let (url, _) = recording(sse).await;
            let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
            let result = forward
                .forward_chat(&serde_json::json!({"model":"p","messages":[]}))
                .await;
            assert!(matches!(result, Err(AdapterError::Uncertain(_))), "{bad}");
        }
    }

    /// SPEC §10, T19: what the relay cannot answer faithfully is refused before
    /// anything is sent, with a typed reason the router turns into a 400.
    // T19
    #[tokio::test]
    async fn unrelayable_parameters_are_refused_before_anything_is_sent() {
        use super::{validate_chat_request, ChatRequestRefusal};
        let base = serde_json::json!({"model":"p","messages":[]});
        assert_eq!(validate_chat_request(&base), Ok(()));
        let mut one = base.clone();
        one["n"] = serde_json::json!(1);
        assert_eq!(validate_chat_request(&one), Ok(()));
        for (field, value) in [
            ("n", serde_json::json!(2)),
            ("functions", serde_json::json!([])),
            ("function_call", serde_json::json!("auto")),
        ] {
            let mut body = base.clone();
            body[field] = value;
            assert!(
                matches!(
                    validate_chat_request(&body),
                    Err(ChatRequestRefusal::Unsupported(_))
                ),
                "{field}"
            );
            let (url, seen) = recording(String::new()).await;
            let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
            assert!(
                matches!(
                    forward.forward_chat(&body).await,
                    Err(AdapterError::UnsupportedCapability)
                ),
                "{field}"
            );
            assert!(
                seen.lock().unwrap().is_empty(),
                "{field} reached the engine"
            );
        }
        let mut internal = base.clone();
        internal["rid"] = serde_json::json!("x");
        assert_eq!(
            validate_chat_request(&internal),
            Err(ChatRequestRefusal::Field("rid".into()))
        );
        assert!(matches!(
            validate_chat_request(&serde_json::json!({"messages":[]})),
            Err(ChatRequestRefusal::Malformed(_))
        ));
    }

    /// SPEC §10: an engine forwarder relays inference and nothing else. The paths
    /// refused below are real engine surfaces — sleep and wake, weight reload,
    /// collective RPC, tokenisation — and each one is engine control, which belongs
    /// to the lifecycle authority rather than to whoever can reach the router.
    // T19
    #[test]
    fn a_forwarder_reaches_the_chat_and_model_paths_and_refuses_every_other() {
        let base: reqwest::Url = "http://127.0.0.1:8000".parse().unwrap();
        // T43 (ADR 0030 §2): the decision endpoint, for decisions deployments.
        for path in ["/v1/models", "/v1/chat/completions", "/v1/systemone"] {
            assert_eq!(
                upstream(&base, path).map(String::from),
                Some(format!("http://127.0.0.1:8000{path}")),
                "{path} is forwarded"
            );
        }
        for path in [
            "/sleep",
            "/wake_up",
            "/collective_rpc",
            "/v1/load_lora_adapter",
            "/tokenize",
            "/v1/embeddings",
            "/v1/completions",
            "/health",
            "",
            "/",
            "/v1/models/",
            "/v1/chat/completions?x=1",
            "/v1/systemone/",
            "/v1/systemone?x=1",
        ] {
            assert!(
                upstream(&base, path).is_none(),
                "{path} must not be reachable through a forwarder"
            );
        }
    }

    fn decision(extra: Value) -> Value {
        let mut body = json!({
            "model": "m",
            "state": "I was charged twice.",
            "questions": {
                "route": {"type": "choice", "instructions": "Which team?",
                          "criteria": {"billing": "payments", "shipping": null}},
                "urgency": {"type": "score", "instructions": "How urgent?",
                            "criteria": ["can wait", "today"]},
                "angry": {"type": "noul", "instructions": "Angry?",
                          "criteria": {"true": "yes", "false": "no"}},
            },
        });
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            body[key] = value;
        }
        body
    }

    // T43 (ADR 0030 §2): a decision request is held to its own closed
    // contract: the System One forms pass, anything else is refused before
    // sending, and the chat contract never stands in for it.
    #[test]
    fn a_decision_request_is_held_to_its_own_closed_contract() {
        use super::{validate_chat_request, validate_decision_request, DecisionRequestRefusal};
        for accepted in [
            decision(json!({})),
            decision(json!({"state": {"ticket": "x", "plan": "pro"}})),
            decision(json!({"state": [{"role": "user", "content": "hi"}]})),
            decision(json!({"images": ["data:image/png;base64,AAAA"], "files": []})),
            decision(json!({"questions": {"q": {"type": "noul"}}})),
        ] {
            assert_eq!(validate_decision_request(&accepted), Ok(()), "{accepted}");
        }
        for (field, value) in [
            ("chat_template_kwargs", json!({})),
            ("messages", json!([])),
            ("stream", json!(true)),
        ] {
            assert_eq!(
                validate_decision_request(&decision(json!({ field: value }))),
                Err(DecisionRequestRefusal::Field(field.into())),
                "{field}"
            );
        }
        let malformed = [
            json!([]),
            json!({"state": "x", "questions": {"q": {"type": "noul"}}}),
            decision(json!({"model": ""})),
            decision(json!({"state": 3})),
            decision(json!({"questions": {}})),
            decision(json!({"questions": []})),
            decision(json!({"questions": {"q": "noul"}})),
            decision(json!({"questions": {"q": {"type": "unknown"}}})),
            decision(json!({"questions": {"q": {"instructions": "x"}}})),
            decision(json!({"questions": {"q": {"type": "noul", "weight": 2}}})),
            decision(json!({"questions": {"q": {"type": "noul", "instructions": 1}}})),
            decision(json!({"questions": {"q": {"type": "choice", "criteria": ["a", "b"]}}})),
            decision(json!({"questions": {"q": {"type": "score", "criteria": {"0": "a"}}}})),
            decision(json!({"images": "data:image/png;base64,AAAA"})),
            decision(json!({"files": [1]})),
        ];
        for body in malformed {
            assert!(
                matches!(
                    validate_decision_request(&body),
                    Err(DecisionRequestRefusal::Malformed(_))
                ),
                "{body}"
            );
        }
        let mut without_state = decision(json!({}));
        without_state.as_object_mut().unwrap().remove("state");
        assert!(validate_decision_request(&without_state).is_err());
        assert!(validate_chat_request(&decision(json!({}))).is_err());
    }

    // T43 (ADR 0030 §2): only an `answers` object of typed answers is
    // completion evidence, in the shapes llama.cpp v0.7.0 and SGLang 0.5.22
    // answer (a `noul` answer carries only its probability).
    #[test]
    fn only_typed_answers_are_decision_evidence() {
        use super::decision_answered;
        assert!(decision_answered(&json!({"model": "m", "answers": {
            "route": {"type": "choice", "choice": "billing",
                      "probabilities": {"billing": 0.95, "shipping": 0.05}, "confidence": 0.9},
            "angry": {"type": "noul", "noul": 0.62},
        }, "usage": {"input_tokens": 40, "output_tokens": 0}})));
        for body in [
            json!({}),
            json!({"answers": {}}),
            json!({"answers": []}),
            json!({"answers": {"q": 0.5}}),
            json!({"answers": {"q": {"noul": 0.5}}}),
            json!({"error": {"message": "x"}}),
        ] {
            assert!(!decision_answered(&body), "{body}");
        }
    }

    // T43 (found live 2026-10-17): SGLang 0.5.22 refuses an invalid decision
    // request with FastAPI's 422 `detail` array; its message is relayed.
    #[test]
    fn a_validation_detail_is_the_rejection_message() {
        let body = br#"{"detail":[{"type":"union_tag_invalid","loc":["body","questions","q"],"msg":"Input tag 'unknown' found using 'type' does not match any of the expected tags","input":{}}]}"#;
        assert!(super::rejection_message(body)
            .unwrap()
            .contains("does not match any of the expected tags"));
    }

    /// Counts the progress reports and the chunks a forward delivers.
    #[derive(Default)]
    struct Counting {
        progressed: usize,
        chunks: Vec<String>,
    }
    #[async_trait::async_trait]
    impl crate::traits::ChatSink for Counting {
        fn progressed(&mut self) {
            self.progressed += 1;
        }
        async fn send(&mut self, chunk: String) -> Result<(), crate::traits::DeliveryFailed> {
            self.chunks.push(chunk);
            Ok(())
        }
    }

    /// SPEC §10 (found live 2026-10-02, TensorFold 0.6.1 and 0.6.2): TensorFold
    /// sends its `role` chunk before it prefills the prompt, so counting that
    /// chunk as progress put a long prefill under the stream idle bound and cut
    /// a 256k-token prompt at 120 s. The opening chunk is relayed but is not
    /// progress; the first output and every event after it are, collected or
    /// streamed.
    // T17 T19
    #[tokio::test]
    async fn the_role_opening_chunk_is_relayed_but_is_not_progress() {
        let sse = [
            tool_chunk(json!({"role":"assistant"}), Value::Null),
            tool_chunk(json!({"content":"hi"}), Value::Null),
            tool_chunk(json!({}), json!("stop")),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat();
        let (url, _) = recording(sse).await;
        let forward = crate::forward::engine_forwarder(url, "m".into(), None, true);
        let request = json!({"model":"public","messages":[]});
        let mut sink = Counting::default();
        let end = forward
            .forward_chat_stream_async(&request, &mut sink)
            .await
            .unwrap();
        assert_eq!(end, crate::traits::StreamEnded::Completed);
        assert_eq!(sink.chunks.len(), 3, "the opening chunk is still relayed");
        // The content chunk, the finish chunk and the terminator.
        assert_eq!(sink.progressed, 3);
        let mut observer = Counting::default();
        let response = forward
            .forward_chat_observed(&request, &mut observer)
            .await
            .unwrap();
        assert_eq!(response["choices"][0]["message"]["content"], "hi");
        assert_eq!(observer.progressed, 3);
        // Only a chunk that opens the reply and carries nothing else qualifies.
        use super::opens_reply_only;
        let chunk = |delta: Value, finish: Value| {
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
        };
        assert!(opens_reply_only(&chunk(
            json!({"role":"assistant"}),
            Value::Null
        )));
        assert!(opens_reply_only(&chunk(
            json!({"role":"assistant","content":"","reasoning_content":null}),
            Value::Null
        )));
        for output in [
            chunk(json!({"role":"assistant","content":"hi"}), Value::Null),
            chunk(
                json!({"role":"assistant","tool_calls":[{"index":0}]}),
                Value::Null,
            ),
            chunk(json!({"role":"assistant"}), json!("stop")),
            chunk(json!({"content":""}), Value::Null),
            json!({"choices":[{"delta":{"role":"assistant"}}],"usage":{"total_tokens":1}})
                .to_string(),
            json!({"choices":[]}).to_string(),
            "not json".to_owned(),
        ] {
            assert!(!opens_reply_only(&output), "{output}");
        }
    }

    // SPEC §10 (`cache_salt`): the field is allowed, its value is a non-empty
    // string of at most MAX_CACHE_SALT_BYTES bytes, and null means absent.
    #[test]
    fn cache_salt_is_allowed_and_its_value_bounded() {
        use super::{validate_chat_request, ChatRequestRefusal, MAX_CACHE_SALT_BYTES};
        let with = |salt: Value| json!({"model":"m","messages":[],"cache_salt":salt});
        for salt in [
            json!("tenant-a"),
            json!("s".repeat(MAX_CACHE_SALT_BYTES)),
            Value::Null,
        ] {
            assert_eq!(validate_chat_request(&with(salt.clone())), Ok(()), "{salt}");
            assert!(super::chat_request_allowed(&with(salt)));
        }
        for salt in [
            json!(""),
            json!(7),
            json!({"tenant":"a"}),
            json!("s".repeat(MAX_CACHE_SALT_BYTES + 1)),
        ] {
            assert!(
                matches!(
                    validate_chat_request(&with(salt.clone())),
                    Err(ChatRequestRefusal::Malformed(_))
                ),
                "{salt}"
            );
        }
    }

    // SPEC §10 (`cache_salt_unsupported`): only vLLM and SGLang honour the
    // partition; a forwarder for any other engine refuses a request carrying
    // one before connecting (the endpoint here accepts no connection).
    #[tokio::test]
    async fn a_forwarder_for_an_engine_ignoring_cache_salt_refuses_it_before_sending() {
        use super::engine_honours_cache_salt;
        assert!(engine_honours_cache_salt(Some("vllm")));
        assert!(engine_honours_cache_salt(Some("sglang")));
        assert!(!engine_honours_cache_salt(Some("tensorfold")));
        // T42 T37 (ADR 0029 §11): llama-server never reads the field.
        assert!(!engine_honours_cache_salt(Some("llamacpp")));
        assert!(!engine_honours_cache_salt(None));
        let forward = crate::forward::engine_forwarder(
            "http://127.0.0.1:9".parse().unwrap(),
            "m".into(),
            None,
            false,
        );
        let request = json!({"model":"m","messages":[],"cache_salt":"tenant-a"});
        assert!(matches!(
            forward.forward_chat(&request).await,
            Err(AdapterError::CacheSaltUnsupported)
        ));
        let mut chunks = |_chunk: String| {};
        assert!(matches!(
            forward.forward_chat_stream(&request, &mut chunks).await,
            Err(AdapterError::CacheSaltUnsupported)
        ));
        // Without the field the same forwarder sends (and here fails to connect).
        let plain = json!({"model":"m","messages":[]});
        assert!(!matches!(
            forward.forward_chat(&plain).await,
            Err(AdapterError::CacheSaltUnsupported)
        ));
    }
}
