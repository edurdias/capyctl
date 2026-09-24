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
    "model", "messages", "stream", "stream_options", "max_tokens", "max_completion_tokens",
    "temperature", "top_p", "n", "stop", "presence_penalty", "frequency_penalty",
    "logit_bias", "logprobs", "top_logprobs", "user", "seed", "tools", "tool_choice",
    "parallel_tool_calls", "response_format", "functions", "function_call",
    "reasoning_effort", "modalities", "metadata",
    // Sampling extensions.
    "top_k", "min_p", "repetition_penalty", "min_tokens", "stop_token_ids", "ignore_eos",
    "skip_special_tokens", "spaces_between_special_tokens", "include_stop_str_in_output",
    "no_stop_trim", "length_penalty",
    // Chat templating inputs (never the template itself).
    "chat_template_kwargs", "add_generation_prompt", "continue_final_message", "echo",
    "documents",
    // Structured output.
    "guided_json", "guided_regex", "guided_choice", "guided_grammar", "structured_outputs",
    "json_schema", "regex", "ebnf",
    // Reasoning.
    "separate_reasoning", "stream_reasoning", "include_reasoning",
];

/// Whether `body` is a JSON object carrying only [`CHAT_REQUEST_FIELDS`].
pub fn chat_request_allowed(body: &Value) -> bool {
    body.as_object()
        .is_some_and(|fields| fields.keys().all(|name| CHAT_REQUEST_FIELDS.contains(&name.as_str())))
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
        return Err(ChatRequestRefusal::Malformed("the body is not a JSON object"));
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
    if fields.get("n").is_some_and(|n| !n.is_null() && n.as_u64() != Some(1)) {
        return Err(ChatRequestRefusal::Unsupported("n other than 1"));
    }
    if fields.contains_key("functions") || fields.contains_key("function_call") {
        return Err(ChatRequestRefusal::Unsupported(
            "the deprecated functions/function_call parameters; use tools",
        ));
    }
    Ok(())
}

fn uncertain() -> AdapterError {
    AdapterError::Uncertain("chat terminal result unverified".into())
}

/// The only upstream paths a forwarder may reach on an engine.
///
/// SPEC §10: the router relays inference and nothing else. An engine also serves
/// its own administrative surface — park, sleep, weight reload, collective RPC —
/// and reaching any of it on a client's behalf would put engine control behind the
/// inference port, where none of the lifecycle authority's accounting applies. The
/// list is closed rather than filtered so a new path has to be added deliberately.
const FORWARDED_PATHS: [&str; 2] = ["/v1/models", "/v1/chat/completions"];

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
    model: String,
    key: Option<String>,
    client: reqwest::Client,
}

impl ChatHttp {
    pub(crate) fn new(base: reqwest::Url, model: String, key: Option<String>) -> Self {
        Self {
            endpoint: upstream(&base, "/v1/chat/completions").expect("a forwarded path"),
            model,
            key,
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
        let end = self
            .stream_inner(body, &mut Collecting { chunks: &mut chunks, observer }, read_idle)
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
        self.chunks.push(chunk);
        Ok(())
    }
}

/// Assemble one non-streaming response from a completed stream's chunks.
fn assemble(chunks: Vec<String>) -> Result<Value, AdapterError> {
    // A completed stream always carried a chunk; never index blindly.
    let first = chunks.first().ok_or_else(uncertain)?;
    let mut response = serde_json::from_str::<Value>(first).map_err(|_| uncertain())?;
    let mut content = String::new();
    let mut reasoning = String::new();
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
        if let Some(text) = chunk["choices"][0]["delta"]["reasoning_content"].as_str() {
            reasoning.push_str(text);
        }
        fold_tool_calls(&mut tool_calls, &chunk["choices"][0]["delta"]["tool_calls"]);
        match &chunk["choices"][0]["logprobs"] {
            Value::Null => {}
            Value::Object(block) => match block.get("content") {
                Some(Value::Array(tokens)) => {
                    logprobs.get_or_insert_with(Vec::new).extend(tokens.iter().cloned())
                }
                None | Some(Value::Null) => {}
                Some(_) => return Err(uncertain()),
            },
            _ => return Err(uncertain()),
        }
        if !chunk["choices"][0]["finish_reason"].is_null() {
            finish = chunk["choices"][0]["finish_reason"].clone();
        }
        if chunk.get("usage").is_some() {
            response["usage"] = chunk["usage"].clone();
        }
    }
    response["object"] = json!("chat.completion");
    // Collecting must not silently discard the trace a streaming caller would
    // have received. The field is omitted entirely when the engine sent none,
    // so a non-reasoning response keeps its existing shape.
    let mut message = json!({"role":"assistant","content":content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
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
            self.stream_inner(body, &mut CallbackSink(on_chunk), Some(Duration::from_secs(60))),
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
        let public = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(AdapterError::UnsupportedCapability)?;
        let mut request = body.clone();
        request["model"] = json!(self.model);
        request["stream"] = json!(true);
        let mut send = self.client.post(self.endpoint.clone()).json(&request);
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
                uncertain()
            }
        })?;
        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(refused_before_forwarding(response).await);
        }
        if !response.status().is_success()
            || response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_none_or(|h| h.split(';').next().unwrap_or("").trim() != "text/event-stream")
        {
            return Err(uncertain());
        }
        let mut stream = response.bytes_stream();
        let mut parser = Parser::new(&self.model, public);
        let mut delivery_failed = false;
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
                if payload.is_some() || done {
                    // Progress is the backend's, delivered or drained.
                    sink.progressed();
                }
                if let Some(chunk) = payload.filter(|_| !delivery_failed) {
                    delivery_failed = !matches!(
                        tokio::time::timeout(Duration::from_secs(10), sink.send(chunk)).await,
                        Ok(Ok(()))
                    );
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

/// The largest refusal body read when deciding whether a 503 proves the request
/// never reached the engine.
const REFUSAL_LIMIT: usize = 4 * 1024;

/// SPEC §§4.3, 10: a role that is shutting down refuses new work before it
/// forwards anything, with a 503 whose body names `shutting_down` (see the
/// role admission gate in `mllm-cli`'s `shutdown.rs`, which wraps every host
/// ingress). Only that exact, bounded answer is evidence the engine never saw the
/// request. Any other 503, an oversized or unreadable body, or a slow one, stays
/// uncertain: an engine's own 503 can arrive after it accepted work.
async fn refused_before_forwarding(response: reqwest::Response) -> AdapterError {
    if response
        .content_length()
        .is_some_and(|length| length > REFUSAL_LIMIT as u64)
    {
        return uncertain();
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ())?;
            if body.len() + chunk.len() > REFUSAL_LIMIT {
                return Err(());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(())
    })
    .await;
    if !matches!(read, Ok(Ok(()))) {
        return uncertain();
    }
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
pub fn engine_forwarder(
    base: reqwest::Url,
    model: String,
    key: Option<String>,
) -> std::sync::Arc<dyn crate::traits::ChatForward> {
    std::sync::Arc::new(EngineForward(ChatHttp::new(base, model, key)))
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
            // streams its trace as `reasoning_content` deltas, so rejecting the key
            // fails every chunk and the whole stream, even though the engine is
            // behaving correctly. It is relayed unchanged and validated like
            // `content`; the allowlist stays closed to everything else so unknown
            // fields are still never passed through untested.
            // SPEC §10 also preserves tool calls: `tool_calls` deltas are relayed
            // once their shape is validated (see `valid_tool_call_deltas`).
            if delta.keys().any(|k| {
                !matches!(k.as_str(), "role" | "content" | "reasoning_content" | "tool_calls")
            }) || delta.get("role").is_some_and(|r| r != "assistant")
                || ["content", "reasoning_content"].iter().any(|key| {
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
                    if matches!(s.as_str(), "stop" | "length" | "content_filter" | "tool_calls") =>
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
    let optional_string = |value: Option<&Value>| value.is_none_or(|v| v.is_null() || v.is_string());
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
        let Some(index) = delta["index"].as_u64() else { continue };
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
    use super::upstream;
    use crate::traits::AdapterError;

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
    // uncertain, because an engine can answer 503 after accepting work.
    #[tokio::test]
    async fn only_a_shutting_down_refusal_proves_the_engine_never_saw_the_request() {
        let request = serde_json::json!({"model":"m","messages":[]});
        let refusal = serde_json::json!({"error":{"code":"shutting_down","message":"restarting","retryable":true}});
        let forward = crate::forward::engine_forwarder(answering(503, refusal).await, "m".into(), None);
        assert!(matches!(
            forward.forward_chat(&request).await,
            Err(AdapterError::NotAccepted(_))
        ));
        for body in [
            serde_json::json!({"error":{"code":"overloaded","retryable":true}}),
            serde_json::json!({"error":{"code":"shutting_down"}}),
            serde_json::json!({"object":"error","code":503}),
        ] {
            let forward = crate::forward::engine_forwarder(answering(503, body).await, "m".into(), None);
            assert!(matches!(
                forward.forward_chat(&request).await,
                Err(AdapterError::Uncertain(_))
            ));
        }
        let refusal = serde_json::json!({"error":{"code":"shutting_down","retryable":true}});
        let forward = crate::forward::engine_forwarder(answering(500, refusal).await, "m".into(), None);
        assert!(matches!(
            forward.forward_chat(&request).await,
            Err(AdapterError::Uncertain(_))
        ));
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
        for field in ["rid", "lora_path", "return_hidden_states", "custom_logit_processor",
                      "bootstrap_host", "bootstrap_port", "bootstrap_room", "kv_transfer_params",
                      "vllm_xargs", "priority", "chat_template", "logits_processors",
                      "unknown_engine_field"] {
            let mut body = serde_json::json!({"model":"m","messages":[]});
            body[field] = serde_json::json!(1);
            assert!(!chat_request_allowed(&body), "{field}");
            let forward = crate::forward::engine_forwarder(
                answering(200, serde_json::json!({})).await, "m".into(), None);
            assert!(matches!(forward.forward_chat(&body).await, Err(AdapterError::PolicyDenied)),
                    "{field}");
        }
        assert!(!chat_request_allowed(&serde_json::json!(["not", "an", "object"])));
    }

    /// An engine that records each request body and answers with `sse`.
    async fn recording(sse: String) -> (reqwest::Url, std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
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
        let forward = crate::forward::engine_forwarder(url, "m".into(), None);
        let tools = serde_json::json!([{"type":"function","function":{"name":"get_weather",
            "parameters":{"type":"object"}}}]);
        let request = serde_json::json!({"model":"public","messages":[],"tools":tools,
            "tool_choice":"auto","parallel_tool_calls":false});
        let response = forward.forward_chat(&request).await.unwrap();
        let message = &response["choices"][0]["message"];
        assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(message["content"], serde_json::Value::Null);
        assert_eq!(message["tool_calls"], serde_json::json!([{"id":"call_1","type":"function",
            "function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]));
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

    /// SPEC §10, T19: a malformed `tool_calls` delta is not relayed as success.
    // T19
    #[tokio::test]
    async fn a_malformed_tool_call_delta_stays_uncertain() {
        for bad in [
            serde_json::json!({"tool_calls":[{"id":"x"}]}),
            serde_json::json!({"tool_calls":[{"index":0,"type":"shell"}]}),
            serde_json::json!({"tool_calls":[{"index":0,"function":{"name":1}}]}),
            serde_json::json!({"tool_calls":"x"}),
        ] {
            let sse = [tool_chunk(bad.clone(), serde_json::json!("tool_calls")),
                       "data: [DONE]\n\n".to_owned()].concat();
            let (url, _) = recording(sse).await;
            let forward = crate::forward::engine_forwarder(url, "m".into(), None);
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
        for (field, value) in [("n", serde_json::json!(2)), ("functions", serde_json::json!([])),
                               ("function_call", serde_json::json!("auto"))] {
            let mut body = base.clone();
            body[field] = value;
            assert!(matches!(validate_chat_request(&body), Err(ChatRequestRefusal::Unsupported(_))),
                    "{field}");
            let (url, seen) = recording(String::new()).await;
            let forward = crate::forward::engine_forwarder(url, "m".into(), None);
            assert!(matches!(forward.forward_chat(&body).await,
                             Err(AdapterError::UnsupportedCapability)), "{field}");
            assert!(seen.lock().unwrap().is_empty(), "{field} reached the engine");
        }
        let mut internal = base.clone();
        internal["rid"] = serde_json::json!("x");
        assert_eq!(validate_chat_request(&internal), Err(ChatRequestRefusal::Field("rid".into())));
        assert!(matches!(validate_chat_request(&serde_json::json!({"messages":[]})),
                         Err(ChatRequestRefusal::Malformed(_))));
    }

    /// SPEC §10: an engine forwarder relays inference and nothing else. The paths
    /// refused below are real engine surfaces — sleep and wake, weight reload,
    /// collective RPC, tokenisation — and each one is engine control, which belongs
    /// to the lifecycle authority rather than to whoever can reach the router.
    // T19
    #[test]
    fn a_forwarder_reaches_the_chat_and_model_paths_and_refuses_every_other() {
        let base: reqwest::Url = "http://127.0.0.1:8000".parse().unwrap();
        for path in ["/v1/models", "/v1/chat/completions"] {
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
        ] {
            assert!(
                upstream(&base, path).is_none(),
                "{path} must not be reachable through a forwarder"
            );
        }
    }
}
