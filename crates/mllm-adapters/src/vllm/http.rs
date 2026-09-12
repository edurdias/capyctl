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
#[derive(Debug, Clone)]
pub struct EngineHttp {
    base: reqwest::Url,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl EngineHttp {
    pub fn new(base: reqwest::Url, api_key: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(ENGINE_CONTROL_TIMEOUT_SECS))
            .build()
            .expect("reqwest client builds with static config");
        Self {
            base,
            api_key,
            client,
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
        let req = self.auth(self.client.post(url));
        self.post_outcome(req, "sleep").await.map(|()| SleepOutcome::Applied)
    }

    /// `POST /wake_up` — wake allocations (weights + KV restore are the
    /// caller's responsibility to verify; see `restore` in the adapter).
    pub async fn wake(&self) -> Result<WakeOutcome, HttpError> {
        let req = self.auth(self.client.post(self.url("/wake_up")));
        self.post_outcome(req, "wake_up")
            .await
            .map(|()| WakeOutcome::Applied)
    }

    /// `POST /collective_rpc` — the dangerous collective control surface
    /// (vLLM security docs [S2]). Reachable only under the deep-park policy
    /// gate; the adapter invokes it exactly once per collective (SPEC §11).
    pub async fn collective_rpc(&self) -> Result<WakeOutcome, HttpError> {
        let req = self.auth(self.client.post(self.url("/collective_rpc")));
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
        let req = self.auth(self.client.post(self.url("/v1/chat/completions")).json(request));
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
                            if std::env::var("MLLM_SSE_DEBUG").is_ok() {
                                eprintln!("SSE-CHUNK: {payload}");
                            }
                            on_chunk(&StreamChunk {
                                text: payload.to_string(),
                                done: false,
                            });
                        }
                    }
                }
                Some(Err(e)) => return Err(HttpError::Body(e.to_string())),
                None => {
                    return Ok(StreamEnd::BackendClosed);
                }
            }
        }
    }

    async fn get_ok(&self, path: &str) -> Result<(), HttpError> {
        let req = self.auth(self.client.get(self.url(path)));
        let resp = req.send().await.map_err(http_err(path))?;
        check_status(resp.status())
    }

    async fn get_body(&self, path: &str) -> Result<String, HttpError> {
        let req = self.auth(self.client.get(self.url(path)));
        let resp = req.send().await.map_err(http_err(path))?;
        check_status(resp.status())?;
        resp.text()
            .await
            .map_err(|e| HttpError::Body(e.to_string()))
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