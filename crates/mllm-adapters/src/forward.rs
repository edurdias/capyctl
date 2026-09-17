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

fn uncertain() -> AdapterError {
    AdapterError::Uncertain("chat terminal result unverified".into())
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
            endpoint: base.join("/v1/chat/completions").expect("static path"),
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

    pub(crate) async fn collect(&self, body: &Value) -> Result<Value, AdapterError> {
        let mut chunks = Vec::new();
        let end = self.stream(body, &mut |chunk| chunks.push(chunk)).await?;
        if end != StreamEnded::Completed {
            return Err(uncertain());
        }
        let mut response = serde_json::from_str::<Value>(&chunks[0]).map_err(|_| uncertain())?;
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish = Value::Null;
        for chunk in chunks {
            let chunk: Value = serde_json::from_str(&chunk).map_err(|_| uncertain())?;
            if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str() {
                content.push_str(text);
            }
            if let Some(text) = chunk["choices"][0]["delta"]["reasoning_content"].as_str() {
                reasoning.push_str(text);
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
        response["choices"] = json!([{"index":0,"message":message,"finish_reason":finish}]);
        Ok(response)
    }

    pub(crate) async fn stream(
        &self,
        body: &Value,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<StreamEnded, AdapterError> {
        self.stream_async(body, &mut CallbackSink(on_chunk)).await
    }

    pub(crate) async fn stream_async(
        &self,
        body: &Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        tokio::time::timeout(Duration::from_secs(300), self.stream_inner(body, sink))
            .await
            .map_err(|_| uncertain())?
    }

    async fn stream_inner(
        &self,
        body: &Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        let public = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(uncertain)?;
        if !body.is_object()
            || body.get("n").is_some_and(|n| n.as_u64() != Some(1))
            || ["tools", "tool_choice", "functions", "function_call"]
                .iter()
                .any(|k| body.get(k).is_some())
        {
            return Err(AdapterError::UnsupportedCapability);
        }
        let mut request = body.clone();
        request["model"] = json!(self.model);
        request["stream"] = json!(true);
        let mut send = self.client.post(self.endpoint.clone()).json(&request);
        if let Some(key) = &self.key {
            send = send.bearer_auth(key);
        }
        let response = send.send().await.map_err(|_| uncertain())?;
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
        while let Some(bytes) = tokio::time::timeout(Duration::from_secs(60), stream.next())
            .await
            .map_err(|_| uncertain())?
        {
            for byte in bytes.map_err(|_| uncertain())? {
                // A byte can finish at most one event. Hold only that payload
                // while awaiting capacity; never collect a transport chunk's
                // events into a second queue.
                let mut payload = None;
                let done = parser.byte(byte, &mut |chunk| payload = Some(chunk))?;
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
            if delta
                .keys()
                .any(|k| !matches!(k.as_str(), "role" | "content" | "reasoning_content"))
                || delta.get("role").is_some_and(|r| r != "assistant")
                || ["content", "reasoning_content"].iter().any(|key| {
                    delta
                        .get(*key)
                        .is_some_and(|value| !value.is_null() && !value.is_string())
                })
            {
                return Err(uncertain());
            }
            match choices[0].get("finish_reason") {
                Some(Value::Null) => {}
                Some(Value::String(s))
                    if matches!(s.as_str(), "stop" | "length" | "content_filter") =>
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
