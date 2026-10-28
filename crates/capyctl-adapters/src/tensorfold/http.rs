//! ADR 0023 §4, §6: TensorFold's `/health` and `/v1/models`, bounded reads.
//! ADR 0039 §9, §10: the native line's `/health` (`status`, `model_ids`,
//! `max_batch_size`, and 1.1.5's 503 bodies) and its `/metrics` work gauges.
use std::time::Duration;

use crate::traits::AdapterError;

/// A read of TensorFold's own surfaces must finish within this.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest body read (a model list or a health object is small).
const MAX_BODY: usize = 256 * 1024;
/// The longest `/metrics` body read: the native line writes one KV usage
/// sample per live stream and its histograms beside the two gauges.
const MAX_METRICS_BODY: usize = 4 * 1024 * 1024;
/// The most of a 503 body a readiness failure quotes.
const MAX_QUOTED_BODY: usize = 256;

/// ADR 0039 §10: the native line's work gauges, both from one status read
/// (`server/metrics.zig`, 1.0.0 `:112-113`, 1.1.5 `:153-154`).
pub const REQUESTS_RUNNING: &str = "tensorfold:requests_running";
pub const REQUESTS_WAITING: &str = "tensorfold:requests_waiting";

/// ADR 0039 §9: what the native line's `GET /health` answered
/// (`server/status_routes.zig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeHealth {
    /// The connection was refused: the listener opens only once the engine
    /// has loaded, so it is still loading, or it has exited.
    Refused,
    /// A 503 (1.1.5 and later: `{"status": "draining", ...}` or
    /// `{"status": "unavailable"}`), with the start of its body.
    Unavailable(String),
    /// A 200 answer. `model_ids` is `None` for a body without it (a
    /// key-configured engine answers `{"status": "ok"}` alone, which is
    /// never ours).
    Answer {
        status: String,
        model_ids: Option<Vec<String>>,
        max_batch_size: Option<u64>,
    },
}

impl NativeHealth {
    /// ADR 0039 §9: `status` `"ok"` with `served` among `model_ids`.
    pub fn serves(&self, served: &str) -> bool {
        matches!(self, Self::Answer { status, model_ids: Some(ids), .. }
            if status == "ok" && ids.iter().any(|id| id == served))
    }
}

/// ADR 0039 §10: the native line's two work gauges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeWork {
    pub running: f64,
    pub waiting: f64,
}

impl NativeWork {
    pub fn idle(&self) -> bool {
        self.running == 0.0 && self.waiting == 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthReport {
    pub ok: bool,
    pub busy: bool,
    pub requests_running: u64,
}

impl HealthReport {
    /// `Some(true)` idle, `Some(false)` busy, `None` when the two counters
    /// disagree (spec §5: that is not idle either).
    pub fn idle(&self) -> Option<bool> {
        match (self.busy, self.requests_running) {
            (false, 0) => Some(true),
            (true, n) if n > 0 => Some(false),
            _ => None,
        }
    }
}

pub(crate) struct Http {
    base: reqwest::Url,
    client: reqwest::Client,
}

pub(crate) enum Read<T> {
    /// Not listening yet, or answering before the model is loaded.
    NotYet,
    Answer(T),
}

impl Http {
    pub(crate) fn new(base: reqwest::Url) -> Self {
        Self {
            base,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(READ_TIMEOUT)
                .build()
                .expect("static client configuration"),
        }
    }

    async fn json(&self, path: &str) -> Result<Read<serde_json::Value>, AdapterError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| AdapterError::Uncertain("bad engine URL".into()))?;
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() => return Ok(Read::NotYet),
            Err(_) => return Err(AdapterError::Uncertain(format!("{path} did not answer"))),
        };
        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Ok(Read::NotYet);
        }
        if !response.status().is_success() {
            return Err(AdapterError::Uncertain(format!(
                "{path} answered {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| AdapterError::Uncertain(format!("{path} body unreadable")))?;
        if bytes.len() > MAX_BODY {
            return Err(AdapterError::Uncertain(format!("{path} body too large")));
        }
        serde_json::from_slice(&bytes)
            .map(Read::Answer)
            .map_err(|_| AdapterError::Uncertain(format!("{path} is not JSON")))
    }

    pub(crate) async fn health(&self) -> Result<Read<HealthReport>, AdapterError> {
        Ok(match self.json("/health").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(HealthReport {
                ok: body["ok"] == true,
                busy: body["busy"]
                    .as_bool()
                    .ok_or_else(|| AdapterError::Uncertain("health has no busy".into()))?,
                requests_running: body["requests_running"].as_u64().ok_or_else(|| {
                    AdapterError::Uncertain("health has no requests_running".into())
                })?,
            }),
        })
    }

    pub(crate) async fn models(&self) -> Result<Read<Vec<String>>, AdapterError> {
        Ok(match self.json("/v1/models").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(
                body["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["id"].as_str().map(str::to_owned))
                    .collect(),
            ),
        })
    }

    /// A bounded `GET` of `path`: `None` when the connection was refused,
    /// else the status and at most `limit` bytes of body.
    async fn get(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Option<(reqwest::StatusCode, Vec<u8>)>, AdapterError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| AdapterError::Uncertain("bad engine URL".into()))?;
        let mut response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() => return Ok(None),
            Err(_) => return Err(AdapterError::Uncertain(format!("{path} did not answer"))),
        };
        let status = response.status();
        let too_large = || AdapterError::Uncertain(format!("{path} body too large"));
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AdapterError::Uncertain(format!("{path} body unreadable")))?
        {
            if bytes.len() + chunk.len() > limit {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some((status, bytes)))
    }

    /// ADR 0039 §9: the native line's `GET /health`. A refused connection
    /// is [`NativeHealth::Refused`]; a 503 keeps its body (1.1.5's
    /// `draining` and `unavailable`); any other non-success status, or a
    /// 200 that is not a JSON object, is an error.
    pub(crate) async fn native_health(&self) -> Result<NativeHealth, AdapterError> {
        let Some((status, bytes)) = self.get("/health", MAX_BODY).await? else {
            return Ok(NativeHealth::Refused);
        };
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim();
            let end = (0..=text.len().min(MAX_QUOTED_BODY))
                .rev()
                .find(|at| text.is_char_boundary(*at))
                .unwrap_or(0);
            return Ok(NativeHealth::Unavailable(text[..end].to_owned()));
        }
        if !status.is_success() {
            return Err(AdapterError::Uncertain(format!(
                "/health answered {status}"
            )));
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| AdapterError::Uncertain("/health is not JSON".into()))?;
        if !body.is_object() {
            return Err(AdapterError::Uncertain(
                "/health is not a JSON object".into(),
            ));
        }
        Ok(NativeHealth::Answer {
            status: body["status"].as_str().unwrap_or_default().to_owned(),
            model_ids: body["model_ids"].as_array().map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect()
            }),
            max_batch_size: body["max_batch_size"].as_u64(),
        })
    }

    /// ADR 0039 §10: the native line's `/metrics` work gauges, read in one
    /// scrape. `NotYet` only for a refused connection; a body without
    /// either gauge, or any status other than success, is an error: a
    /// missing gauge is unknown work, never zero (SPEC §10 step 4).
    pub(crate) async fn native_work(&self) -> Result<Read<NativeWork>, AdapterError> {
        let Some((status, bytes)) = self.get("/metrics", MAX_METRICS_BODY).await? else {
            return Ok(Read::NotYet);
        };
        if !status.is_success() {
            return Err(AdapterError::Uncertain(format!(
                "/metrics answered {status}"
            )));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| AdapterError::Uncertain("/metrics is not text".into()))?;
        let gauge = crate::llamacpp::http::gauge;
        match (gauge(text, REQUESTS_RUNNING), gauge(text, REQUESTS_WAITING)) {
            (Some(running), Some(waiting)) => Ok(Read::Answer(NativeWork { running, waiting })),
            _ => Err(AdapterError::Uncertain(
                "/metrics has no work gauges".into(),
            )),
        }
    }

    /// ADR 0028 §9 (decided 2026-10-06): the completion probe in TensorFold's
    /// OpenAI form, `POST /v1/completions` with `return_token_ids`, on its
    /// loopback endpoint (TensorFold takes no key, ADR 0023 §3). Bounded by
    /// `bound`, not the short read bound. Answers the token ids TensorFold
    /// puts in its `tensorfold` block and `choices[0].text`
    /// ([`crate::completion_probe::tensorfold_answer`]).
    pub(crate) async fn complete_probe(
        &self,
        served: &str,
        max_tokens: u32,
        bound: Duration,
    ) -> Result<crate::completion_probe::ProbeAnswer, AdapterError> {
        use crate::completion_probe as probe;
        let url = self
            .base
            .join(probe::OPENAI_PATH)
            .map_err(|_| AdapterError::Uncertain("bad engine URL".into()))?;
        let answer = probe::post(
            &self.client,
            url,
            None,
            &probe::openai_request(served, max_tokens),
            bound,
        )
        .await?;
        probe::tensorfold_answer(&answer)
    }
}
