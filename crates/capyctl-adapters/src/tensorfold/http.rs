//! ADR 0023 §4, §6: TensorFold's `/health` and `/v1/models`, bounded reads.
use std::time::Duration;

use crate::traits::AdapterError;

/// A read of TensorFold's own surfaces must finish within this.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest body read (a model list or a health object is small).
const MAX_BODY: usize = 256 * 1024;

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
}
