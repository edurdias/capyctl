//! SPEC §10, ADR 0013 §10 (owner decision D9): host engine load reporting.
//!
//! Once per tick the host scrapes each Ready scope's engine `/metrics` on
//! loopback, with that launch's native key, and reports the gauges with the
//! ingress in-flight count over the control session as W3 `ReportLoad`.
//!
//! A sample is a routing hint, never readiness or admission evidence and never
//! journaled, except as W12 and SPEC §10 (amended 2026-10-01) quiescence
//! evidence after a restart or a hang-up. A TensorFold sample folds in the
//! engine's unkeyed `/health`, so it reads idle only when both agree. Metric names are pinned to the recorded engine
//! sources; a missing, malformed or ambiguous gauge makes the sample
//! `scrape_ok = false` rather than a guess. Metrics are read on loopback only
//! and are never reachable through ingress or the router (SPEC §13.3, M08).
use crate::ingress::{Ingress, LoadTarget};
use capyctl_adapters::tensorfold::http::HealthReport;
use capyctl_domain::latency::{Histogram, MAX_BUCKETS};
use capyctl_protocol::{
    pb,
    reports::{
        EngineLoad, LoadReport, LoadSample, SampleLatency, KV_USAGE_PPM_FULL, MAX_LOAD_GAUGE,
        MAX_LOAD_REPORT_BYTES, MAX_LOAD_SAMPLES,
    },
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

/// D9: the default reporting period.
pub const DEFAULT_LOAD_INTERVAL: Duration = Duration::from_secs(1);
/// Bounds on the reporting period.
pub const MIN_LOAD_INTERVAL: Duration = Duration::from_millis(250);
pub const MAX_LOAD_INTERVAL: Duration = Duration::from_secs(5);
/// One scrape, connect through body, must finish within this bound.
pub const SCRAPE_TIMEOUT: Duration = Duration::from_millis(300);
/// Bound on one `/metrics` body; an engine exposition is far smaller.
pub const MAX_METRICS_BYTES: usize = 4 * 1024 * 1024;

/// Engine gauge names, pinned to the recorded engine sources.
struct Family {
    running: &'static str,
    waiting: &'static str,
    /// A 0..=1 fraction of the KV pool in use.
    kv_usage: &'static str,
}
/// vLLM `vllm/v1/metrics/loggers.py` at 98dff2a81d747d1dba01a47f939f48c3526d4206
/// (labels `model_name`, `engine`; one series per engine core).
const VLLM: Family = Family {
    running: "vllm:num_requests_running",
    waiting: "vllm:num_requests_waiting",
    kv_usage: "vllm:kv_cache_usage_perc",
};
/// SGLang `python/sglang/srt/observability/metrics_collector.py` at
/// fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1. `token_usage` is the bottleneck
/// pool usage ratio (0.0–1.0). SGLang exports metrics only with
/// `enable_metrics` (WE2); without it the scrape fails and reports so.
const SGLANG: Family = Family {
    running: "sglang:num_running_reqs",
    waiting: "sglang:num_queue_reqs",
    kv_usage: "sglang:token_usage",
};

#[derive(Debug, thiserror::Error)]
#[error("host load reporting unavailable")]
pub struct LoadError;

/// Validate a configured reporting period against the D9 bounds.
pub fn load_interval(period: Duration) -> Result<Duration, LoadError> {
    (MIN_LOAD_INTERVAL..=MAX_LOAD_INTERVAL)
        .contains(&period)
        .then_some(period)
        .ok_or(LoadError)
}

/// Every sample value of the exact metric `name` in a Prometheus text
/// exposition. `Err` when a line of that metric cannot be parsed.
fn series(text: &str, name: &str) -> Result<Vec<f64>, ()> {
    let mut values = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        // Exact name only: `vllm:num_requests_waiting_by_reason` is not ours.
        let rest = match rest.chars().next() {
            Some('{') => {
                // Skip the label set, honoring quoted values and escapes.
                let mut quoted = false;
                let mut escaped = false;
                let mut end = None;
                for (i, c) in rest.char_indices().skip(1) {
                    match c {
                        _ if escaped => escaped = false,
                        '\\' if quoted => escaped = true,
                        '"' => quoted = !quoted,
                        '}' if !quoted => {
                            end = Some(i);
                            break;
                        }
                        _ => {}
                    }
                }
                &rest[end.ok_or(())? + 1..]
            }
            Some(c) if c.is_whitespace() => rest,
            _ => continue,
        };
        let value: f64 = rest
            .split_whitespace()
            .next()
            .ok_or(())?
            .parse()
            .map_err(|_| ())?;
        if !value.is_finite() || value < 0.0 {
            return Err(());
        }
        values.push(value);
    }
    Ok(values)
}

fn count(text: &str, name: &str) -> Option<u32> {
    let values = series(text, name).ok()?;
    if values.is_empty() {
        return None;
    }
    // One series per engine core or data-parallel rank: the host's total.
    let total: f64 = values.iter().sum();
    (total <= f64::from(MAX_LOAD_GAUGE)).then(|| total.round() as u32)
}

fn usage_ppm(text: &str, name: &str) -> Option<u32> {
    let values = series(text, name).ok()?;
    // The most pressured pool is the one that refuses work first.
    let usage = values.into_iter().reduce(f64::max)?;
    (usage <= 1.0)
        .then(|| ((usage * f64::from(KV_USAGE_PPM_FULL)).round() as u32).min(KV_USAGE_PPM_FULL))
}
/// TensorFold 0.6.0 and 0.6.1 `tensorfold/server/metrics.py`. The KV ratio has one
/// series per stream pool; the most pressured pool counts.
const TENSORFOLD: Family = Family {
    running: "tensorfold:requests_running",
    waiting: "tensorfold:requests_waiting",
    kv_usage: "tensorfold:kv_cache_usage_ratio",
};

fn family_load(text: &str, family: &Family) -> Option<EngineLoad> {
    Some(EngineLoad {
        running: count(text, family.running)?,
        waiting: count(text, family.waiting)?,
        kv_usage_ppm: usage_ppm(text, family.kv_usage)?,
    })
}

type HistogramTable = &'static [(&'static str, &'static str)];

/// The one family whose three gauges parse, with its name and histograms.
/// TensorFold 0.6.1 also mirrors its values under vLLM names; its own
/// `tensorfold:` families win so the mirrors are never read or double counted.
fn family_of(text: &str) -> Option<(&'static str, EngineLoad, HistogramTable)> {
    if let Some(load) = family_load(text, &TENSORFOLD) {
        return Some(("tensorfold", load, TENSORFOLD_HISTOGRAMS));
    }
    let found: Vec<_> = [
        ("vllm", &VLLM, VLLM_HISTOGRAMS),
        ("sglang", &SGLANG, SGLANG_HISTOGRAMS),
        ("tensorfold", &TENSORFOLD, TENSORFOLD_HISTOGRAMS),
    ]
    .into_iter()
    .filter_map(|(name, family, table)| family_load(text, family).map(|l| (name, l, table)))
    .collect();
    match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// Engine gauges from one `/metrics` body. `None` unless exactly one engine
/// family's three gauges are all present and well formed.
pub fn parse_engine_load(text: &str) -> Option<EngineLoad> {
    family_of(text).map(|(_, load, _)| load)
}

/// SPEC §17 (owner decision 2026-09-23, M80): the engine latency histograms a
/// host forwards, as `(reported series, engine metric)`. Pinned to the
/// installed engine sources, read on host-a on 2026-09-23:
/// vLLM 0.29.0 `vllm/v1/metrics/loggers.py` (labels `model_name`, `engine`);
/// the names are unchanged in 0.30.0.
const VLLM_HISTOGRAMS: &[(&str, &str)] = &[
    (
        "engine_time_to_first_token",
        "vllm:time_to_first_token_seconds",
    ),
    (
        "engine_e2e_request_latency",
        "vllm:e2e_request_latency_seconds",
    ),
    ("engine_queue_time", "vllm:request_queue_time_seconds"),
    ("engine_prefill_time", "vllm:request_prefill_time_seconds"),
    ("engine_decode_time", "vllm:request_decode_time_seconds"),
    (
        "engine_inter_token_latency",
        "vllm:inter_token_latency_seconds",
    ),
];
/// SGLang 0.5.20 `sglang/srt/observability/metrics_collector.py`. SGLang has
/// no prefill or decode phase histogram; its TTFT and end-to-end series carry
/// an `is_streaming` label, summed here. Exported only with `enable_metrics`.
const SGLANG_HISTOGRAMS: &[(&str, &str)] = &[
    (
        "engine_time_to_first_token",
        "sglang:time_to_first_token_seconds",
    ),
    (
        "engine_e2e_request_latency",
        "sglang:e2e_request_latency_seconds",
    ),
    ("engine_queue_time", "sglang:queue_time_seconds"),
    (
        "engine_inter_token_latency",
        "sglang:inter_token_latency_seconds",
    ),
];
/// TensorFold 0.6.0 and 0.6.1 have no queue, prefill, decode or inter-token histogram.
const TENSORFOLD_HISTOGRAMS: &[(&str, &str)] = &[
    (
        "engine_time_to_first_token",
        "tensorfold:time_to_first_token_seconds",
    ),
    (
        "engine_e2e_request_latency",
        "tensorfold:request_latency_seconds",
    ),
];

/// Every sample of the exact metric `name`, as (label set, value). `Err` when
/// a line of that metric cannot be parsed.
fn labelled(text: &str, name: &str) -> Result<Vec<(String, f64)>, ()> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let (labels, rest) = match rest.chars().next() {
            Some('{') => {
                let mut quoted = false;
                let mut escaped = false;
                let mut end = None;
                for (i, c) in rest.char_indices().skip(1) {
                    match c {
                        _ if escaped => escaped = false,
                        '\\' if quoted => escaped = true,
                        '"' => quoted = !quoted,
                        '}' if !quoted => {
                            end = Some(i);
                            break;
                        }
                        _ => {}
                    }
                }
                let end = end.ok_or(())?;
                (rest[1..end].to_owned(), &rest[end + 1..])
            }
            Some(c) if c.is_whitespace() => (String::new(), rest),
            _ => continue,
        };
        let value: f64 = rest
            .split_whitespace()
            .next()
            .ok_or(())?
            .parse()
            .map_err(|_| ())?;
        out.push((labels, value));
    }
    Ok(out)
}

/// The value of label `key` in a Prometheus label set.
fn label<'a>(labels: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = labels;
    loop {
        rest = rest.trim_start_matches([',', ' ']);
        if rest.is_empty() {
            return None;
        }
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start().strip_prefix('"')?;
        let mut escaped = false;
        let mut close = None;
        for (i, c) in after.char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => {
                    close = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let close = close?;
        if name == key {
            return Some(&after[..close]);
        }
        rest = &after[close + 1..];
    }
}

/// An exposition count: finite, non-negative and exactly representable.
fn whole(value: f64) -> Option<u64> {
    (value.is_finite() && (0.0..=9.0e15).contains(&value)).then(|| value.round() as u64)
}

/// One Prometheus histogram `metric`, summed over every label set except `le`
/// (model, engine core, `is_streaming`). `None` when absent, malformed,
/// inconsistent or over [`MAX_BUCKETS`].
pub fn parse_histogram(text: &str, metric: &str) -> Option<Histogram> {
    let buckets = labelled(text, &format!("{metric}_bucket")).ok()?;
    if buckets.is_empty() {
        return None;
    }
    let mut by_le: Vec<(f64, u64)> = Vec::new();
    let mut infinite = 0u64;
    for (labels, value) in buckets {
        let le = label(&labels, "le")?;
        let count = whole(value)?;
        if matches!(le, "+Inf" | "Inf" | "inf") {
            infinite = infinite.checked_add(count)?;
            continue;
        }
        let bound: f64 = le.parse().ok()?;
        if !bound.is_finite() {
            return None;
        }
        match by_le.iter_mut().find(|(b, _)| *b == bound) {
            Some((_, total)) => *total = total.checked_add(count)?,
            None => {
                if by_le.len() >= MAX_BUCKETS {
                    return None;
                }
                by_le.push((bound, count));
            }
        }
    }
    by_le.sort_by(|a, b| a.0.total_cmp(&b.0));
    let sum: f64 = labelled(text, &format!("{metric}_sum"))
        .ok()?
        .iter()
        .map(|(_, v)| v)
        .sum();
    let total = labelled(text, &format!("{metric}_count"))
        .ok()?
        .iter()
        .try_fold(0u64, |acc, (_, v)| acc.checked_add(whole(*v)?))?;
    if total != infinite {
        return None;
    }
    let (bounds, cumulative): (Vec<f64>, Vec<u64>) = by_le.into_iter().unzip();
    Histogram::from_cumulative(bounds, &cumulative, total, sum.max(0.0)).ok()
}

/// SPEC §17 (M80): the engine family of one `/metrics` body and its latency
/// histograms (cumulative since the engine started). The family is the one
/// whose load gauges parse; `None` when neither or both do.
pub fn parse_engine_histograms(text: &str) -> Option<(&'static str, Vec<(String, Histogram)>)> {
    let (engine, _, table) = family_of(text)?;
    let histograms = table
        .iter()
        .filter_map(|(series, metric)| {
            parse_histogram(text, metric).map(|h| ((*series).to_owned(), h))
        })
        .collect();
    Some((engine, histograms))
}

/// T41, ADR 0023 §6 (2026-10-01): TensorFold's gauges read idle only when
/// its `/health` agrees. Anything but an idle health (busy, disagreeing
/// counters, unreadable) keeps at least one request running in the sample.
pub fn fold_tensorfold_health(load: EngineLoad, health: Option<&HealthReport>) -> EngineLoad {
    if health.and_then(HealthReport::idle) == Some(true) {
        load
    } else {
        EngineLoad {
            running: load.running.max(1),
            ..load
        }
    }
}

/// Previous cumulative engine histograms, keyed by scope and series, so each
/// report carries only what was observed since the last one.
type EngineBaselines = HashMap<(String, u32, i64, String), Histogram>;

/// Scrapes Ready scopes and builds bounded load reports for one host.
pub struct LoadReporter {
    ingress: Arc<Ingress>,
    client: reqwest::Client,
    host_id: String,
    interval: Duration,
    /// SPEC §17: bounded by the Ready scopes of this tick (pruned each tick).
    baselines: Mutex<EngineBaselines>,
}

/// A response body, refused as soon as it passes the scrape size bound.
async fn bounded_body(mut response: reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_METRICS_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

impl LoadReporter {
    pub fn new(ingress: Arc<Ingress>, host_id: String) -> Result<Self, LoadError> {
        Ok(Self {
            ingress,
            // Loopback only: no proxy, no redirect can move the scrape elsewhere.
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(SCRAPE_TIMEOUT)
                .build()
                .map_err(|_| LoadError)?,
            host_id,
            interval: DEFAULT_LOAD_INTERVAL,
            baselines: Mutex::new(HashMap::new()),
        })
    }
    pub fn with_interval(mut self, period: Duration) -> Result<Self, LoadError> {
        self.interval = load_interval(period)?;
        Ok(self)
    }
    pub fn interval(&self) -> Duration {
        self.interval
    }

    async fn scrape(&self, target: &LoadTarget) -> Option<String> {
        if !target.target.ip().is_loopback() {
            return None;
        }
        let read = async {
            let response = self
                .client
                .get(format!("http://{}/metrics", target.target))
                .bearer_auth(hex::encode(target.native))
                .send()
                .await
                .ok()?;
            if response.status() != reqwest::StatusCode::OK {
                return None;
            }
            String::from_utf8(bounded_body(response).await?).ok()
        };
        tokio::time::timeout(SCRAPE_TIMEOUT, read)
            .await
            .ok()
            .flatten()
    }

    /// TensorFold's `/health` on the same loopback target, unkeyed (the
    /// engine has no key there), within the scrape bound.
    async fn health(&self, target: &LoadTarget) -> Option<HealthReport> {
        if !target.target.ip().is_loopback() {
            return None;
        }
        let read = async {
            let response = self
                .client
                .get(format!("http://{}/health", target.target))
                .send()
                .await
                .ok()?;
            if response.status() != reqwest::StatusCode::OK {
                return None;
            }
            let body = bounded_body(response).await?;
            let body: serde_json::Value = serde_json::from_slice(&body).ok()?;
            Some(HealthReport {
                ok: body["ok"] == true,
                busy: body["busy"].as_bool()?,
                requests_running: body["requests_running"].as_u64()?,
            })
        };
        tokio::time::timeout(SCRAPE_TIMEOUT, read)
            .await
            .ok()
            .flatten()
    }

    /// One tick: a sample per Ready scope, split into reports that each pass
    /// the W3 bounds. Empty when no scope is Ready.
    pub async fn reports(&self) -> Vec<pb::ReportLoad> {
        let Ok(targets) = self.ingress.load_targets() else {
            return vec![];
        };
        let targets: Vec<_> = targets
            .into_iter()
            .filter(|t| t.scope.host_id == self.host_id)
            .collect();
        let scraped = futures::future::join_all(targets.iter().map(|t| self.scrape(t))).await;
        // Only a TensorFold scrape is folded with its health.
        let healths = futures::future::join_all(targets.iter().zip(&scraped).map(
            |(target, body)| async move {
                match body.as_deref().and_then(family_of) {
                    Some(("tensorfold", ..)) => Some(self.health(target).await),
                    _ => None,
                }
            },
        ))
        .await;
        let mut baselines = self.baselines.lock().unwrap_or_else(|p| p.into_inner());
        // SPEC §17: baselines of scopes no longer Ready are dropped.
        baselines.retain(|(deployment, instance, generation, _), _| {
            targets.iter().any(|t| {
                t.scope.deployment_id == *deployment
                    && t.scope.instance_index == *instance
                    && t.scope.generation == *generation
            })
        });
        let mut samples: Vec<(u32, LoadSample)> = targets
            .into_iter()
            .zip(scraped)
            .zip(healths)
            .map(|((target, body), health)| {
                let engine =
                    body.as_deref()
                        .and_then(parse_engine_load)
                        .map(|load| match &health {
                            Some(health) => fold_tensorfold_health(load, health.as_ref()),
                            None => load,
                        });
                let latency = self.latency(&target, body.as_deref(), &mut baselines);
                (
                    target.scope.instance_index,
                    LoadSample {
                        deployment_id: target.scope.deployment_id,
                        generation: target.scope.generation,
                        owned_handle: target.owned_handle,
                        sampled_at_ms: capyctl_protocol::now_unix_ms(),
                        ingress_in_flight: u32::try_from(target.in_flight)
                            .unwrap_or(MAX_LOAD_GAUGE)
                            .min(MAX_LOAD_GAUGE),
                        engine,
                        latency,
                    },
                )
            })
            .collect();
        drop(baselines);
        // Two members of one instance (deployment, instance and generation)
        // on one host are one launch: report it once. ADR 0013 §5: two
        // instances of one deployment on one host are reported apart. The
        // dropped member's ingress timings are merged into the kept one.
        let key =
            |(instance, s): &(u32, LoadSample)| (s.deployment_id.clone(), *instance, s.generation);
        samples.sort_by_key(key);
        samples.dedup_by(|a, b| {
            if key(a) != key(b) {
                return false;
            }
            merge_latency(&mut b.1.latency, a.1.latency.take());
            true
        });
        batch(&self.host_id, samples.into_iter().map(|(_, s)| s).collect())
    }
}

impl LoadReporter {
    /// SPEC §17 (M80): this scope's ingress timings since the last tick and
    /// its engine histograms' growth since the last scrape.
    fn latency(
        &self,
        target: &LoadTarget,
        body: Option<&str>,
        baselines: &mut EngineBaselines,
    ) -> Option<SampleLatency> {
        let mut histograms = self
            .ingress
            .drain_latency(&target.scope)
            .unwrap_or_default();
        let mut engine = None;
        if let Some((family, current)) = body.and_then(parse_engine_histograms) {
            engine = Some(family.to_owned());
            for (series, histogram) in current {
                let key = (
                    target.scope.deployment_id.clone(),
                    target.scope.instance_index,
                    target.scope.generation,
                    series.clone(),
                );
                // First sight of a launch: everything since the engine started,
                // which for a fresh generation is this launch's whole history.
                let delta = match baselines.get(&key) {
                    Some(previous) => histogram.since(previous),
                    None => (!histogram.is_empty()).then(|| histogram.clone()),
                };
                baselines.insert(key, histogram);
                if let Some(delta) = delta {
                    histograms.push((series, delta));
                }
            }
        }
        (engine.is_some() || !histograms.is_empty()).then_some(SampleLatency { engine, histograms })
    }
}

/// Merge `extra` into `into`, series by series (same layout only).
fn merge_latency(into: &mut Option<SampleLatency>, extra: Option<SampleLatency>) {
    let Some(extra) = extra else { return };
    let Some(into) = into.as_mut() else {
        *into = Some(extra);
        return;
    };
    if into.engine.is_none() {
        into.engine = extra.engine;
    }
    for (series, histogram) in extra.histograms {
        match into.histograms.iter_mut().find(|(s, _)| *s == series) {
            Some((_, kept)) => {
                kept.merge(&histogram);
            }
            None => into.histograms.push((series, histogram)),
        }
    }
}

/// Split samples into reports within the W3 count and size bounds. A report
/// the receiver would refuse is never sent.
pub fn batch(host_id: &str, samples: Vec<LoadSample>) -> Vec<pb::ReportLoad> {
    use prost::Message;
    let mut reports = Vec::new();
    let mut current: Vec<LoadSample> = Vec::new();
    let wire = |samples: &[LoadSample]| {
        LoadReport {
            host_id: host_id.to_owned(),
            samples: samples.to_vec(),
        }
        .to_wire()
    };
    for sample in samples {
        current.push(sample);
        if current.len() > MAX_LOAD_SAMPLES || wire(&current).encoded_len() > MAX_LOAD_REPORT_BYTES
        {
            let overflow = current.pop();
            if !current.is_empty() {
                reports.push(wire(&current));
            }
            current = overflow.into_iter().collect();
        }
    }
    if !current.is_empty() {
        reports.push(wire(&current));
    }
    reports
        .into_iter()
        .filter(|r| LoadReport::try_from(r.clone()).is_ok())
        .collect()
}
