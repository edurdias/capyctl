//! HTTP client for vLLM's OpenAI-compatible engine API.
//!
//! Engine-specific knowledge lives here and only here (SPEC §9): the
//! adapter-facing surfaces (`/health`, `/v1/models`, SSE chat completions,
//! `/sleep`, `/wake_up`) are wrapped in outcome types that preserve
//! uncertainty — a lost acknowledgement is never reported as `Applied`.

use std::time::Duration;

use futures::StreamExt;

/// Engine-control request timeout (versioned default; F1 design §5/§8).
pub const ENGINE_CONTROL_TIMEOUT_SECS: u64 = 30;

/// Reload stages checkpoint bytes again (46s for Qwen3-4B on Spark).
/// Keep it bounded independently of the short sleep/wake controls.
pub const WEIGHT_RELOAD_TIMEOUT_SECS: u64 = 120;

/// SPEC §9.1 / T21: the longest body read from the engine (a model list, a
/// metrics exposition, a control acknowledgement, one SSE frame). A body with or
/// without a declared length is never read past it.
pub const MAX_ENGINE_BODY_BYTES: usize = 4 << 20;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("engine unreachable: {0}")]
    Unreachable(String),
    #[error("engine returned status {0}")]
    UnexpectedStatus(u16),
    #[error("engine rejected authentication")]
    AuthRejected,
    #[error("engine body error: {0}")]
    Body(String),
    /// The control effect may or may not have been applied: the connection
    /// failed after the request was dispatched. The caller must reconcile,
    /// never repeat blindly (SPEC §6.4 / design §3).
    #[error("engine control outcome uncertain: {0}")]
    Uncertain(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SleepOutcome {
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEnd {
    /// Backend closed the stream cleanly with `data: [DONE]`.
    Completed,
    /// Backend closed without `[DONE]` — the stream ended but was not
    /// confirmed complete.
    BackendClosed,
}

/// A decoded SSE data chunk (the `data:` payload without prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamChunk {
    pub text: String,
    pub done: bool,
}

/// HTTP client for one engine member's API surface.
#[derive(Clone)]
pub struct EngineHttp {
    base: reqwest::Url,
    api_key: Option<String>,
    /// SPEC §9.1 / T21: the key the development and control routes take. When
    /// set, the engine's guard keys those routes apart from inference, so the
    /// inference key alone cannot sleep, wake or reload the engine.
    admin_key: Option<String>,
    client: reqwest::Client,
}

/// SPEC §13.3: this carries the per-launch engine key, so it is never formatted
/// by the derive. `ChatHttp` and `EngineForward` have no `Debug` at all for the
/// same reason; this type is public and re-exported, so it keeps one that says
/// where it points and whether a key is set, and never what the key is.
impl std::fmt::Debug for EngineHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHttp")
            .field("base", &self.base.as_str())
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "<set>"
                } else {
                    "none"
                },
            )
            .field("admin_key", &crate::traits::redacted(self.admin_key.is_some()))
            .finish()
    }
}

impl EngineHttp {
    pub fn new(base: reqwest::Url, api_key: Option<String>) -> Self {
        // NO client-wide total timeout: a 30s total timeout on the shared
        // client would kill multi-minute SSE chat streams. The bounded
        // engine-control timeout is applied per control request (see
        // `control`); the stream path is deliberately unbounded.
        // SPEC §9.1 / T21: the engine listens on loopback and carries its key
        // on every request, so no proxy from the environment and no redirect
        // may move a request (or the key) anywhere else.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client builds with static config");
        Self {
            base,
            api_key,
            admin_key: None,
            client,
        }
    }

    /// SPEC §9.1 / T21: present `admin_key` on the control routes (sleep, wake,
    /// sleep state, collective RPC, prefix-cache reset) instead of the
    /// inference key.
    pub fn with_admin_key(mut self, admin_key: String) -> Self {
        self.admin_key = Some(admin_key);
        self
    }

    fn admin_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.admin_key.as_ref().or(self.api_key.as_ref()) {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    fn url(&self, path: &str) -> reqwest::Url {
        self.base.join(path).expect("path joins into base url")
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => reqwest::RequestBuilder::bearer_auth(req, k),
            None => req,
        }
    }

    /// Bounded engine-control request (versioned default, F1 design §5/§8):
    /// applied per control endpoint only. Chat streams never take it.
    fn control(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.timeout(Duration::from_secs(ENGINE_CONTROL_TIMEOUT_SECS))
    }

    /// Liveness probe. Explicitly NOT readiness (SPEC §6.1) — callers must
    /// use `list_models` for readiness.
    pub async fn health(&self) -> Result<bool, HttpError> {
        self.get_ok("/health").await.map(|()| true)
    }

    /// Served model ids from `/v1/models`. Presence of the deployment's
    /// model id is the readiness signal.
    pub async fn list_models(&self) -> Result<Vec<String>, HttpError> {
        let body = self.get_body("/v1/models").await?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| HttpError::Body(e.to_string()))?;
        let ids = v["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    /// `POST /sleep?level=N`. A transport failure after dispatch maps to
    /// `Uncertain` — the effect may or may not have been applied.
    pub async fn sleep(&self, level: u8) -> Result<SleepOutcome, HttpError> {
        let url = self.url(&format!("/sleep?level={level}"));
        let req = self.control(self.admin_auth(self.client.post(url)));
        self.post_outcome(req, "sleep")
            .await
            .map(|()| SleepOutcome::Applied)
    }

    /// `POST /wake_up` — wake allocations (weights + KV restore are the
    /// caller's responsibility to verify; see `restore` in the adapter).
    pub async fn wake(&self) -> Result<WakeOutcome, HttpError> {
        let req = self.control(self.admin_auth(self.client.post(self.url("/wake_up"))));
        self.post_outcome(req, "wake_up")
            .await
            .map(|()| WakeOutcome::Applied)
    }

    /// `POST /wake_up?tags=<tag>`: wake one allocation class only. SPEC §9.1
    /// restores a level-2 park in order: weights, `reload_weights`, then KV,
    /// so the two wakes are separate controls with separate evidence.
    pub async fn wake_tag(&self, tag: WakeTag) -> Result<WakeOutcome, HttpError> {
        let url = self.url(&format!("/wake_up?tags={}", tag.as_str()));
        let req = self.control(self.admin_auth(self.client.post(url)));
        self.post_outcome(req, "wake_up")
            .await
            .map(|()| WakeOutcome::Applied)
    }

    /// `GET /is_sleeping`: the engine's own report of its sleep state. It is
    /// a post-condition check on a sleep or wake, never readiness (SPEC §6.1).
    pub async fn is_sleeping(&self) -> Result<bool, HttpError> {
        let req = self.control(self.admin_auth(self.client.get(self.url("/is_sleeping"))));
        let resp = req.send().await.map_err(http_err("/is_sleeping"))?;
        check_status(resp.status())?;
        let body = read_bounded(resp).await?;
        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| HttpError::Body(e.to_string()))?;
        value["is_sleeping"]
            .as_bool()
            .ok_or_else(|| HttpError::Body("is_sleeping carries no state".into()))
    }

    /// Running and waiting request counts from the engine's Prometheus
    /// metrics (`vllm:num_requests_running`, `vllm:num_requests_waiting`),
    /// summed over label sets. `None` when either gauge is absent: a missing
    /// gauge is unknown work, never zero (SPEC §9.2, §10 step 4).
    pub async fn work_counts(&self) -> Result<Option<(f64, f64)>, HttpError> {
        let body = self.get_body("/metrics").await?;
        Ok(match (
            gauge_sum(&body, "vllm:num_requests_running"),
            gauge_sum(&body, "vllm:num_requests_waiting"),
        ) {
            (Some(running), Some(waiting)) => Some((running, waiting)),
            _ => None,
        })
    }

    /// Invalidate prefix-cache metadata after destructive restoration.
    /// HTTP success alone is insufficient: vLLM may acknowledge a refused
    /// reset with `{"success": false}` when blocks are still in use.
    pub async fn reset_prefix_cache(&self) -> Result<(), HttpError> {
        let req = self.control(self.admin_auth(self.client.post(self.url("/reset_prefix_cache"))));
        let response = req.send().await.map_err(http_err("reset_prefix_cache"))?;
        check_status(response.status())?;
        if !response.status().is_success() {
            return Err(HttpError::UnexpectedStatus(response.status().as_u16()));
        }
        let body = read_bounded(response)
            .await
            .map_err(|e| HttpError::Uncertain(format!("reset_prefix_cache acknowledgement: {e}")))?;
        let body: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            HttpError::Uncertain(format!("reset_prefix_cache acknowledgement: {e}"))
        })?;
        if body.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(HttpError::Uncertain(
                "reset_prefix_cache was not confirmed".into(),
            ));
        }
        Ok(())
    }

    /// `POST /collective_rpc` — the dangerous collective control surface
    /// (vLLM security docs [S2]). Reachable only under the deep-park policy
    /// gate; the adapter invokes it exactly once per collective (SPEC §11).
    pub async fn collective_rpc(&self) -> Result<WakeOutcome, HttpError> {
        let req = self
            .admin_auth(
                self.client
                    .post(self.url("/collective_rpc"))
                    .json(&serde_json::json!({"method": "reload_weights"})),
            )
            .timeout(Duration::from_secs(WEIGHT_RELOAD_TIMEOUT_SECS));
        self.post_outcome(req, "collective_rpc")
            .await
            .map(|()| WakeOutcome::Applied)
    }

    /// Consume an SSE chat-completions stream, invoking `on_chunk` per data
    /// payload. Returns how the stream ended.
    pub async fn chat_completion_stream(
        &self,
        request: &serde_json::Value,
        mut on_chunk: impl FnMut(&StreamChunk),
    ) -> Result<StreamEnd, HttpError> {
        // NOTE: no total timeout here — a chat completion stream may run
        // for minutes; the bounded timeout is control-endpoints only.
        let req = self.auth(
            self.client
                .post(self.url("/v1/chat/completions"))
                .json(request),
        );
        let resp = req.send().await.map_err(http_err("chat completions"))?;
        check_status(resp.status())?;
        if !resp.status().is_success() {
            return Err(HttpError::UnexpectedStatus(resp.status().as_u16()));
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        loop {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    // Normalize CRLF (uvicorn/Starlette SSE): frame parsing
                    // is LF-based.
                    let decoded = String::from_utf8_lossy(&bytes);
                    buf.push_str(&decoded.replace('\r', ""));
                    // SPEC §9.1 / T21: one unterminated frame is bounded too.
                    if buf.len() > MAX_ENGINE_BODY_BYTES {
                        return Err(HttpError::Body("chat frame exceeds the bound".into()));
                    }
                    // SSE frames are delimited by blank lines; data lines by
                    // `data: `. Parse complete frames out of the buffer.
                    while let Some(pos) = buf.find("\n\n") {
                        let frame = buf.drain(..pos + 2).collect::<String>();
                        for line in frame.lines() {
                            let Some(payload) = line.strip_prefix("data: ") else {
                                continue;
                            };
                            let payload = payload.trim();
                            if payload == "[DONE]" {
                                return Ok(StreamEnd::Completed);
                            }
                            on_chunk(&StreamChunk {
                                text: payload.to_string(),
                                done: false,
                            });
                        }
                    }
                }
                Some(Err(e)) => {
                    return Err(HttpError::Body(e.to_string()));
                }
                None => {
                    return Ok(StreamEnd::BackendClosed);
                }
            }
        }
    }

    async fn get_ok(&self, path: &str) -> Result<(), HttpError> {
        let req = self.control(self.auth(self.client.get(self.url(path))));
        let resp = req.send().await.map_err(http_err(path))?;
        check_status(resp.status())
    }

    async fn get_body(&self, path: &str) -> Result<String, HttpError> {
        let req = self.control(self.auth(self.client.get(self.url(path))));
        let resp = req.send().await.map_err(http_err(path))?;
        check_status(resp.status())?;
        read_bounded(resp).await
    }

    async fn post_outcome(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<(), HttpError> {
        let resp = req.send().await.map_err(|e| {
            // Connection-level failure after dispatch: the engine may have
            // applied the effect. Uncertainty, not failure (design §3).
            if is_dispatched_transport_error(&e) {
                HttpError::Uncertain(format!("{what}: {e}"))
            } else {
                http_err(what)(e)
            }
        })?;
        check_status(resp.status())
    }
}

/// SPEC §9.1 / T21: read an engine body up to [`MAX_ENGINE_BODY_BYTES`],
/// refusing a declared or streamed length past it.
async fn read_bounded(mut response: reqwest::Response) -> Result<String, HttpError> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_ENGINE_BODY_BYTES as u64)
    {
        return Err(HttpError::Body("engine body exceeds the bound".into()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| HttpError::Body(e.to_string()))?
    {
        if chunk.len() > MAX_ENGINE_BODY_BYTES - bytes.len() {
            return Err(HttpError::Body("engine body exceeds the bound".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|e| HttpError::Body(e.to_string()))
}

/// One allocation class `POST /wake_up` can wake on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeTag {
    Weights,
    KvCache,
}
impl WakeTag {
    fn as_str(self) -> &'static str {
        match self {
            WakeTag::Weights => "weights",
            WakeTag::KvCache => "kv_cache",
        }
    }
}

/// Sum one gauge over every label set in a Prometheus text exposition. `None`
/// when no sample of it is present or any sample does not parse.
fn gauge_sum(body: &str, name: &str) -> Option<f64> {
    let mut total = None;
    for line in body.lines() {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let value = if let Some(labels) = rest.strip_prefix('{') {
            labels.split_once('}')?.1
        } else if rest.starts_with(' ') {
            rest
        } else {
            // A longer metric name that shares this prefix.
            continue;
        };
        let sample: f64 = value.split_whitespace().next()?.parse().ok()?;
        if !sample.is_finite() || sample < 0.0 {
            return None;
        }
        total = Some(total.unwrap_or(0.0) + sample);
    }
    total
}

fn is_dispatched_transport_error(e: &reqwest::Error) -> bool {
    // A connect error (or a connect-phase timeout, which reqwest reports as
    // both is_connect and is_timeout) means the request never reached the
    // wire. Anything else after dispatch — connection closed mid-wait, body,
    // decode, response-phase timeout — is uncertainty about the effect.
    !e.is_connect()
}

fn http_err(what: &str) -> impl Fn(reqwest::Error) -> HttpError + '_ {
    move |e| {
        if e.is_connect() {
            HttpError::Unreachable(format!("{what}: {e}"))
        } else {
            HttpError::Body(format!("{what}: {e}"))
        }
    }
}

fn check_status(status: reqwest::StatusCode) -> Result<(), HttpError> {
    match status.as_u16() {
        200..=299 => Ok(()),
        401 | 403 => Err(HttpError::AuthRejected),
        s => Err(HttpError::UnexpectedStatus(s)),
    }
}
