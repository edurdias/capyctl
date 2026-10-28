//! ADR 0038 §5, §6: `trtllm-serve`'s `/health`, `/version`, `/v1/models` and
//! `/metrics`, bounded reads on its loopback listener (no key, ADR 0038 §9).
//! `/health` answers an empty 200 while the executor runs and 503 otherwise;
//! the socket is bound before the model is built, so a refused or unanswered
//! connection, like a 503, reads as not yet.
//!
//! `/metrics` is iteration statistics, not a gauge (ADR 0038 evidence §4): a
//! JSON array of the entries recorded since the previous read, which the
//! read empties. An idle engine records none, so an empty read is unknown
//! work, never idle, and no entry ever reads 0 running and 0 waiting.
use std::time::Duration;

use serde::Deserialize;

use crate::traits::AdapterError;

/// A read of `trtllm-serve`'s own surfaces must finish within this. The
/// engine bounds its own stats fetch at 2 s.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest body read: up to `iter_stats_max_iterations` (1000) entries.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// KV usage in parts per million of the pool.
pub const KV_USAGE_PPM_FULL: u32 = 1_000_000;

/// ADR 0038 §4, §6: an iteration entry's `kvCacheStats`
/// (`cpp/tensorrt_llm/executor/jsonSerialization.cpp`); other fields (the
/// reuse counters) are tolerated and not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvCacheStats {
    pub max_num_blocks: Option<u64>,
    pub free_num_blocks: Option<u64>,
    pub used_num_blocks: Option<u64>,
    pub tokens_per_block: Option<u64>,
}

impl KvCacheStats {
    /// ADR 0038 §4: the pool the engine built, `maxNumBlocks ×
    /// tokensPerBlock`, in tokens.
    pub fn pool_tokens(&self) -> Option<u64> {
        self.max_num_blocks?.checked_mul(self.tokens_per_block?)
    }

    /// ADR 0038 §6: `usedNumBlocks / maxNumBlocks`, in parts per million.
    pub fn usage_ppm(&self) -> Option<u32> {
        let max = self.max_num_blocks.filter(|max| *max > 0)?;
        let used = self.used_num_blocks?;
        let ppm = used as f64 / max as f64 * f64::from(KV_USAGE_PPM_FULL);
        Some((ppm.round() as u32).min(KV_USAGE_PPM_FULL))
    }
}

/// ADR 0038 §6: one iteration entry of `/metrics`. Unknown fields
/// (`timestamp`, `inflightBatchingStats`, `gpuMemUsage`, ...) are tolerated
/// and not read; CapyCTL never uses `gpuMemUsage` (ADR 0038 §4).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IterationStats {
    pub iter: Option<u64>,
    pub num_active_requests: Option<u64>,
    pub num_queued_requests: Option<u64>,
    pub kv_cache_stats: Option<KvCacheStats>,
}

/// ADR 0038 §6: the load one `/metrics` read reports, from its newest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IterationLoad {
    /// `numActiveRequests`.
    pub running: u32,
    /// `numQueuedRequests` (the input queue only).
    pub waiting: u32,
    /// `usedNumBlocks / maxNumBlocks`.
    pub kv_usage_ppm: u32,
}

/// One `/metrics` body: a JSON array of iteration entries, oldest first.
/// `None` when it is not one.
pub fn parse_iteration_stats(body: &[u8]) -> Option<Vec<IterationStats>> {
    serde_json::from_slice(body).ok()
}

/// ADR 0038 §6: the load of the newest entry of one read. `None`, unknown and
/// never zero, when the read returned no entry or the newest lacks a figure.
pub fn newest_load(entries: &[IterationStats]) -> Option<IterationLoad> {
    let newest = entries.last()?;
    Some(IterationLoad {
        running: u32::try_from(newest.num_active_requests?).ok()?,
        waiting: u32::try_from(newest.num_queued_requests?).ok()?,
        kv_usage_ppm: newest.kv_cache_stats?.usage_ppm()?,
    })
}

pub(crate) struct Http {
    base: reqwest::Url,
    client: reqwest::Client,
}

pub(crate) enum Read<T> {
    /// Not listening yet, not answering yet, or answering 503 before the
    /// executor runs.
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
            // ADR 0038 evidence §3: the socket is bound before the model is
            // built and served only after, so a connection that is refused or
            // never answered is a start in progress.
            Err(error) if error.is_connect() || error.is_timeout() => return Ok(Read::NotYet),
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

    /// `GET /health`: an empty 200 while the executor runs.
    pub(crate) async fn health(&self) -> Result<Read<()>, AdapterError> {
        Ok(match self.body("/health").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(_) => Read::Answer(()),
        })
    }

    /// `GET /version`: `{"version": "1.2.1"}`; `None` without a string.
    pub(crate) async fn version(&self) -> Result<Read<Option<String>>, AdapterError> {
        Ok(match self.json("/version").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(body["version"].as_str().map(str::to_owned)),
        })
    }

    /// `GET /v1/models`: every listed model's `id`.
    pub(crate) async fn models(&self) -> Result<Read<Vec<String>>, AdapterError> {
        Ok(match self.json("/v1/models").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(
                body["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|model| model["id"].as_str().unwrap_or_default().to_owned())
                    .collect(),
            ),
        })
    }

    /// `GET /metrics`: the iteration entries recorded since the previous
    /// read, which this read consumes. A body that is not an array of
    /// entries is an error, never an empty read.
    pub(crate) async fn metrics(&self) -> Result<Read<Vec<IterationStats>>, AdapterError> {
        Ok(match self.body("/metrics").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(bytes) => {
                Read::Answer(parse_iteration_stats(&bytes).ok_or_else(|| {
                    AdapterError::Uncertain("/metrics is not an array of iteration entries".into())
                })?)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T49 (ADR 0038 §6): entries as `trtllm-serve` 1.2.1 writes them; unknown
    // fields are tolerated, the newest entry is read, and an empty read or a
    // missing figure is unknown.
    #[test]
    fn the_newest_entry_is_the_load() {
        let body = br#"[
            {"timestamp": "10-28-2026 10:00:00", "iter": 7, "numActiveRequests": 1,
             "numQueuedRequests": 0, "numCompletedRequests": 0, "gpuMemUsage": 1,
             "inflightBatchingStats": {"numScheduledRequests": 1},
             "kvCacheStats": {"maxNumBlocks": 910, "freeNumBlocks": 900,
                "usedNumBlocks": 10, "tokensPerBlock": 32, "reusedBlocks": 0}},
            {"iter": 8, "numActiveRequests": 3, "numQueuedRequests": 2,
             "kvCacheStats": {"maxNumBlocks": 1000, "freeNumBlocks": 750,
                "usedNumBlocks": 250, "tokensPerBlock": 32}}
        ]"#;
        let entries = parse_iteration_stats(body).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].kv_cache_stats.unwrap().pool_tokens(),
            Some(29_120)
        );
        assert_eq!(
            newest_load(&entries),
            Some(IterationLoad {
                running: 3,
                waiting: 2,
                kv_usage_ppm: 250_000
            })
        );
        assert_eq!(newest_load(&[]), None);
        assert_eq!(newest_load(&[IterationStats::default()]), None);
        assert_eq!(parse_iteration_stats(b"{}"), None);
        assert_eq!(parse_iteration_stats(b"[]"), Some(Vec::new()));
    }
}
