//! ADR 0029 §10: llama-server's `/health`, `/v1/models`, `/props` and
//! `/metrics`, bounded reads on its loopback listener (no key, ADR 0029 §4).
//! Until the model is loaded every route answers 503 `Loading model`
//! (`tools/server/server-http.cpp`, v0.6.0), which reads as not yet.
use std::time::Duration;

use crate::traits::AdapterError;

/// A read of llama-server's own surfaces must finish within this.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest body read. `/props` carries the chat templates, which run to
/// tens of kilobytes; the other answers are small.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// ADR 0029 §10: the gauges `/metrics` reports for the work in flight.
pub const REQUESTS_PROCESSING: &str = "llamacpp:requests_processing";
pub const REQUESTS_DEFERRED: &str = "llamacpp:requests_deferred";

/// The served model's entry in `/v1/models`: `meta.n_ctx` is one slot's
/// window, capped at the training context `meta.n_ctx_train`
/// (`server-context.cpp`, `n_ctx_slot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedModel {
    pub n_ctx: Option<u64>,
    pub n_ctx_train: Option<u64>,
}

/// What `/props` reports of the settings CapyCTL rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Props {
    pub total_slots: Option<u64>,
    pub endpoint_metrics: Option<bool>,
    pub endpoint_slots: Option<bool>,
}

/// ADR 0029 §10: the two work gauges of `/metrics`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkGauges {
    pub processing: f64,
    pub deferred: f64,
}

impl WorkGauges {
    pub fn idle(&self) -> bool {
        self.processing == 0.0 && self.deferred == 0.0
    }
}

pub(crate) struct Http {
    base: reqwest::Url,
    client: reqwest::Client,
}

pub(crate) enum Read<T> {
    /// Not listening yet, or answering 503 before the model is loaded.
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

    async fn body(&self, path: &str) -> Result<Read<Vec<u8>>, AdapterError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| AdapterError::Uncertain("bad engine URL".into()))?;
        let mut response = match self.client.get(url).send().await {
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
        let too_large = || AdapterError::Uncertain(format!("{path} body too large"));
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY as u64)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AdapterError::Uncertain(format!("{path} body unreadable")))?
        {
            if bytes.len() + chunk.len() > MAX_BODY {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Read::Answer(bytes))
    }

    async fn json(&self, path: &str) -> Result<Read<serde_json::Value>, AdapterError> {
        Ok(match self.body(path).await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(bytes) => Read::Answer(
                serde_json::from_slice(&bytes)
                    .map_err(|_| AdapterError::Uncertain(format!("{path} is not JSON")))?,
            ),
        })
    }

    /// `GET /health`: `{"status": "ok"}` once the model is loaded.
    pub(crate) async fn health(&self) -> Result<Read<bool>, AdapterError> {
        Ok(match self.json("/health").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(body["status"] == "ok"),
        })
    }

    /// `GET /v1/models`: the entry whose `id` is `served`, `None` when the
    /// list does not name it.
    pub(crate) async fn served_model(
        &self,
        served: &str,
    ) -> Result<Read<Option<ServedModel>>, AdapterError> {
        Ok(match self.json("/v1/models").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(
                body["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|model| model["id"].as_str() == Some(served))
                    .map(|model| ServedModel {
                        n_ctx: model["meta"]["n_ctx"].as_u64(),
                        n_ctx_train: model["meta"]["n_ctx_train"].as_u64(),
                    }),
            ),
        })
    }

    /// `GET /props`: the slot count and the two endpoint switches.
    pub(crate) async fn props(&self) -> Result<Read<Props>, AdapterError> {
        Ok(match self.json("/props").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(Props {
                total_slots: body["total_slots"].as_u64(),
                endpoint_metrics: body["endpoint_metrics"].as_bool(),
                endpoint_slots: body["endpoint_slots"].as_bool(),
            }),
        })
    }

    /// `GET /metrics`: the two work gauges. A body without either is an
    /// error: a missing gauge is unknown work, never zero (SPEC §10 step 4).
    pub(crate) async fn work(&self) -> Result<Read<WorkGauges>, AdapterError> {
        Ok(match self.body("/metrics").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| AdapterError::Uncertain("/metrics is not text".into()))?;
                match (
                    gauge(text, REQUESTS_PROCESSING),
                    gauge(text, REQUESTS_DEFERRED),
                ) {
                    (Some(processing), Some(deferred)) => Read::Answer(WorkGauges {
                        processing,
                        deferred,
                    }),
                    _ => {
                        return Err(AdapterError::Uncertain(
                            "/metrics has no work gauges".into(),
                        ))
                    }
                }
            }
        })
    }

    /// ADR 0028 §9 (decided 2026-10-06): the completion probe in the OpenAI
    /// form, `POST /v1/completions`, on the loopback endpoint (llama-server
    /// takes no key, ADR 0029 §4). llama-server ignores `return_token_ids`,
    /// so the answer is the generated text (owner decision 2026-10-09).
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
        probe::openai_answer(&answer)
    }
}

/// One unlabelled sample of `name` in a Prometheus text exposition, as
/// llama-server writes its gauges (`llamacpp:<name> <value>`). `None` when
/// it is absent, appears twice, or is not a finite non-negative number.
pub fn gauge(text: &str, name: &str) -> Option<f64> {
    let mut found = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        // A longer metric name that shares this prefix, or a labelled sample
        // llama-server does not write.
        if !rest.starts_with(' ') {
            continue;
        }
        let value: f64 = rest.split_whitespace().next()?.parse().ok()?;
        if !value.is_finite() || value < 0.0 || found.is_some() {
            return None;
        }
        found = Some(value);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    // T42 (ADR 0029 §10): the gauges as llama-server 0.6.0 writes them
    // (`server_task_result_metrics::to_metrics`).
    #[test]
    fn gauges_are_read_from_the_exposition() {
        let text = "# HELP llamacpp:requests_processing Number of requests processing\n\
                    # TYPE llamacpp:requests_processing gauge\n\
                    llamacpp:requests_processing 2\n\
                    # TYPE llamacpp:requests_deferred gauge\n\
                    llamacpp:requests_deferred 0\n\
                    llamacpp:requests_processing_total 9\n";
        assert_eq!(gauge(text, REQUESTS_PROCESSING), Some(2.0));
        assert_eq!(gauge(text, REQUESTS_DEFERRED), Some(0.0));
        assert_eq!(gauge("", REQUESTS_PROCESSING), None);
        assert_eq!(
            gauge("llamacpp:requests_processing -1\n", REQUESTS_PROCESSING),
            None
        );
        assert_eq!(
            gauge("llamacpp:requests_processing nan\n", REQUESTS_PROCESSING),
            None
        );
        assert_eq!(
            gauge(
                "llamacpp:requests_processing 0\nllamacpp:requests_processing 0\n",
                REQUESTS_PROCESSING
            ),
            None,
            "a gauge written twice is not one reading"
        );
    }
}
