//! SPEC §10, ADR 0013 §10 (owner decision D9): host engine load reporting.
//!
//! Once per tick the host scrapes each Ready scope's engine `/metrics` on
//! loopback, with that launch's native key, and reports the gauges with the
//! ingress in-flight count over the control session as W3 `ReportLoad`. An
//! SGLang launch's resolved running limit is read once from its loopback
//! `/v1/loads` and carried on its samples (owner decision 2026-10-08). The
//! standalone role samples its embedded engines with the same reporter over
//! its own [`LoadSource`] (`capyctl_controller::embedded_load`).
//!
//! A sample is a routing hint, never readiness or admission evidence and never
//! journaled, except as W12 and SPEC §10 (amended 2026-10-01) quiescence
//! evidence after a restart or a hang-up. A TensorFold sample folds in the
//! engine's unkeyed `/health`, so it reads idle only when both agree. A
//! llama.cpp sample takes its KV usage from one bounded `/slots` read (ADR 0029
//! §11), since llama.cpp exports no ratio. Metric names are pinned to the recorded engine
//! sources; a missing, malformed or ambiguous gauge makes the sample
//! `scrape_ok = false` rather than a guess. Metrics are read on loopback only
//! and are never reachable through ingress or the router (SPEC §13.3, M08).
//! ADR 0038 §6: a TensorRT-LLM `/metrics` body is a JSON array of iteration
//! entries, not an exposition; the newest entry's figures are its load, and
//! a read with no entry (or one lacking a figure) carries none: unknown,
//! never 0 and never idle (the controller never settles its cancelling
//! requests on a sample either way, since it reports no idle signal).
//!
//! ADR 0031 §4, §6: a custom engine has no load reports. Its `/metrics` is
//! never read; with a manifest idle signal, its sample carries 0 running and
//! 0 waiting only when that signal reads idle (the SPEC §10 cancellation
//! evidence of a remote host), and no engine figures otherwise. The signal is
//! the one frozen with the launch ([`LoadTarget::signal`]); a launch whose
//! frozen profile is no longer held is never read and never reads idle.
use crate::ingress::{Ingress, IngressScope, LoadTarget};
use capyctl_adapters::tensorfold::http::HealthReport;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::latency::{Histogram, MAX_BUCKETS};
use capyctl_protocol::{
    pb,
    reports::{
        EngineLoad, LmcacheSample, LoadReport, LoadSample, SampleLatency, KV_USAGE_PPM_FULL,
        MAX_LOAD_GAUGE, MAX_LOAD_REPORT_BYTES, MAX_LOAD_SAMPLES,
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
/// TensorFold 0.6.0 to 0.6.5 `tensorfold/server/metrics.py`. The KV ratio has one
/// series per stream pool; the most pressured pool counts.
const TENSORFOLD: Family = Family {
    running: "tensorfold:requests_running",
    waiting: "tensorfold:requests_waiting",
    kv_usage: "tensorfold:kv_cache_usage_ratio",
};

/// ADR 0029 §11: llama.cpp v0.6.0 `tools/server/server-task.cpp`
/// (`server_task_result_metrics`, exported only with `--metrics`). It has no
/// KV usage gauge; [`parse_llamacpp_slots`] derives one.
const LLAMACPP_RUNNING: &str = "llamacpp:requests_processing";
const LLAMACPP_WAITING: &str = "llamacpp:requests_deferred";
/// ADR 0029 §11: llama.cpp exports no latency histograms; its
/// `*_tokens_seconds` gauges are throughputs averaged between two scrapes (each
/// scrape resets them), so no latency series is forwarded.
const LLAMACPP_HISTOGRAMS: &[(&str, &str)] = &[];

/// One family's gauges from `/metrics`. The KV usage is `None` only for a
/// family that does not export it there (llama.cpp).
#[derive(Clone, Copy)]
struct Gauges {
    running: u32,
    waiting: u32,
    kv_usage_ppm: Option<u32>,
}

impl Gauges {
    fn with_kv(self, kv_usage_ppm: Option<u32>) -> Option<EngineLoad> {
        Some(EngineLoad {
            running: self.running,
            waiting: self.waiting,
            kv_usage_ppm: self.kv_usage_ppm.or(kv_usage_ppm)?,
        })
    }
}

fn family_load(text: &str, family: &Family) -> Option<Gauges> {
    Some(Gauges {
        running: count(text, family.running)?,
        waiting: count(text, family.waiting)?,
        kv_usage_ppm: Some(usage_ppm(text, family.kv_usage)?),
    })
}

fn llamacpp_load(text: &str) -> Option<Gauges> {
    Some(Gauges {
        running: count(text, LLAMACPP_RUNNING)?,
        waiting: count(text, LLAMACPP_WAITING)?,
        kv_usage_ppm: None,
    })
}

type HistogramTable = &'static [(&'static str, &'static str)];

/// The one family whose gauges parse, with its name and histograms.
/// TensorFold 0.6.1 to 0.6.5 also mirror their values under vLLM names; their
/// own `tensorfold:` families win so the mirrors are never read or double counted.
fn family_of(text: &str) -> Option<(&'static str, Gauges, HistogramTable)> {
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
    .chain(llamacpp_load(text).map(|l| ("llamacpp", l, LLAMACPP_HISTOGRAMS)))
    .collect();
    match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// Engine gauges from one `/metrics` body. `None` unless exactly one engine
/// family's three gauges are all present and well formed. A llama.cpp body
/// alone is never a whole sample: its KV usage comes from `/slots`
/// ([`parse_llamacpp_load`]).
pub fn parse_engine_load(text: &str) -> Option<EngineLoad> {
    family_of(text).and_then(|(_, gauges, _)| gauges.with_kv(None))
}

/// ADR 0029 §11: a llama.cpp sample from its `/metrics` body and the KV usage
/// its `/slots` read gave ([`parse_llamacpp_slots`]). `None` unless the body is
/// llama.cpp's alone and the usage is known.
pub fn parse_llamacpp_load(text: &str, kv_usage_ppm: Option<u32>) -> Option<EngineLoad> {
    match family_of(text)? {
        ("llamacpp", gauges, _) => gauges.with_kv(kv_usage_ppm),
        _ => None,
    }
}

/// ADR 0038 §6: a TensorRT-LLM sample from one `/metrics` body (a read
/// consumes the entries; the host is its only reader): running is the
/// newest entry's `numActiveRequests`, waiting its `numQueuedRequests`, KV
/// usage `usedNumBlocks / maxNumBlocks`. `None` unless the body is an array
/// of iteration entries whose newest carries all three within the gauge
/// bound: a read with no entry is unknown, never 0.
pub fn parse_tensorrt_llm_load(body: &[u8]) -> Option<EngineLoad> {
    let entries = capyctl_adapters::tensorrt_llm::parse_iteration_stats(body)?;
    let load = capyctl_adapters::tensorrt_llm::newest_load(&entries)?;
    (load.running <= MAX_LOAD_GAUGE && load.waiting <= MAX_LOAD_GAUGE).then_some(EngineLoad {
        running: load.running,
        waiting: load.waiting,
        kv_usage_ppm: load.kv_usage_ppm.min(KV_USAGE_PPM_FULL),
    })
}

/// ADR 0029 §11: where llama.cpp lists its slots (on by default; CapyCTL never
/// renders `--no-slots`).
pub const LLAMACPP_SLOTS_PATH: &str = "/slots";

/// ADR 0029 §11: the KV usage of one llama.cpp v0.6.0 `/slots` body
/// (`server_slot::to_json`): the tokens held by processing slots
/// (`n_prompt_tokens`, which grows with every generated token) over the sum of
/// every slot's `n_ctx`, in parts per million. An idle slot's leftover cache is
/// reusable, not pressure, so it does not count. `None` when the body is not a
/// non-empty slot list with a positive `n_ctx` on each slot and a token count on
/// each processing one: unknown, never zero.
pub fn parse_llamacpp_slots(body: &[u8]) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let slots = value.as_array().filter(|slots| !slots.is_empty())?;
    let mut held: u64 = 0;
    let mut capacity: u64 = 0;
    for slot in slots {
        let n_ctx = slot.get("n_ctx")?.as_u64().filter(|n| *n > 0)?;
        capacity = capacity.checked_add(n_ctx)?;
        if slot.get("is_processing")?.as_bool()? {
            held = held.checked_add(slot.get("n_prompt_tokens")?.as_u64()?)?;
        }
    }
    let ppm = held as f64 / capacity as f64 * f64::from(KV_USAGE_PPM_FULL);
    Some((ppm.round() as u32).min(KV_USAGE_PPM_FULL))
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
/// SGLang 0.5.20 `sglang/srt/observability/metrics_collector.py`; the names
/// are unchanged in 0.5.21. SGLang has
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
/// TensorFold 0.6.0 to 0.6.2 have no queue, prefill, decode or inter-token histogram.
/// 0.6.3 adds a decode histogram (`tensorfold:request_decode_seconds`), not read here.
/// 0.6.4 adds `tensorfold:process_footprint_bytes` (macOS only) and 0.6.5
/// `tensorfold:requests_total` (with API keys only); neither is read here.
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
/// whose load gauges parse; `None` when neither or both do, and for a family
/// that exports no latency histograms (llama.cpp, ADR 0029 §11), which names no
/// engine in the sample.
pub fn parse_engine_histograms(text: &str) -> Option<(&'static str, Vec<(String, Histogram)>)> {
    let (engine, _, table) = family_of(text).filter(|(_, _, table)| !table.is_empty())?;
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

/// SPEC §§10, 17 (owner decision 2026-10-08): where SGLang reports the
/// running-request limit it resolved for the launch: `max_running_requests`
/// per data-parallel rank in `/v1/loads` (SGLang 0.5.20
/// `sglang/srt/entrypoints/v1_loads.py`, `managers/load_snapshot.py`). The
/// `core` section carries no request content and no server arguments.
pub const SGLANG_LOADS_PATH: &str = "/v1/loads?include=core";

/// The running limit in one SGLang `/v1/loads` body: the sum over its ranks.
/// `None` unless every rank names a positive limit and the sum is a bounded
/// gauge; a missing figure is unknown, never zero.
pub fn parse_sglang_max_running(body: &[u8]) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let loads = value.get("loads")?.as_array()?;
    if loads.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    for load in loads {
        let limit = load
            .get("max_running_requests")?
            .as_u64()
            .filter(|limit| *limit > 0)?;
        total = total.checked_add(limit)?;
    }
    u32::try_from(total)
        .ok()
        .filter(|total| *total <= MAX_LOAD_GAUGE)
}

/// What one engine's sample needs beside its `/metrics` body.
enum SideRead {
    None,
    /// TensorFold's `/health`, `None` when unreadable.
    Health(Option<HealthReport>),
    /// llama.cpp's KV usage from `/slots`, `None` when unreadable.
    KvUsage(Option<u32>),
}

/// Previous cumulative engine histograms, keyed by scope and series, so each
/// report carries only what was observed since the last one.
type EngineBaselines = HashMap<(String, u32, i64, String), Histogram>;

/// One launch of one scope: its deployment, instance, generation and owned
/// handle.
type LaunchKey = (String, u32, i64, String);

fn launch_key(target: &LoadTarget) -> LaunchKey {
    (
        target.scope.deployment_id.clone(),
        target.scope.instance_index,
        target.scope.generation,
        target.owned_handle.clone(),
    )
}

/// ADR 0033 §12 (lifecycle design §8): one launch's private LMCache server to
/// read, Ready or parked (a parked launch keeps its server).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LmcacheTarget {
    pub deployment_id: String,
    pub instance_index: u32,
    pub generation: i64,
    pub owned_handle: String,
    /// The server's loopback admin HTTP address (`/metrics`).
    pub http: std::net::SocketAddr,
    /// The server group's recorded processes, for its measured memory.
    pub server: Vec<ProcessIdentity>,
    /// The server's L1 pool.
    pub pool_bytes: i64,
    /// ADR 0033 §6: the server group's GPU memory as the host's per-process
    /// sampler last saw it; `None` when unknown.
    pub device_bytes: Option<u64>,
}

/// SPEC §10, D9: what a reporter samples: the Ready scopes of one host, each
/// with its loopback engine target and launch key, and the forwarding
/// timings recorded for a scope since the last tick.
pub trait LoadSource: Send + Sync {
    /// Every Ready scope to sample now. Empty when none is, or when the
    /// source cannot be read.
    fn targets(&self) -> Vec<LoadTarget>;
    /// SPEC §17 (M80): the scope's forwarding timings since the last call.
    fn drain_latency(&self, _scope: &IngressScope) -> Vec<(String, Histogram)> {
        Vec::new()
    }
    /// ADR 0033 §12: every launch with a private LMCache server to read now,
    /// Ready and parked. Empty for a source without any.
    fn lmcache_targets(&self) -> Vec<LmcacheTarget> {
        Vec::new()
    }
}

/// ADR 0033 §12: the Prometheus names LMCache 0.5.6's MP server exports on
/// its HTTP `/metrics` (`docs/source/mp/observability/metrics.rst`: dots
/// become underscores, counters gain an optional unit and `_total`).
const LMCACHE_L1_USED: &str = "lmcache_mp_l1_memory_usage_bytes";
const LMCACHE_L1_RATIO: &str = "lmcache_mp_l1_usage_ratio";
const LMCACHE_INFLIGHT: &str = "lmcache_mp_inflight_load_memory_usage_bytes";
const LMCACHE_L2_USED: &str = "lmcache_mp_l2_usage_bytes";
const LMCACHE_ALLOC_FAILURES: &str = "lmcache_mp_l1_allocation_failure";
/// Tokens submitted for lookup and found (`lookup_requested`, `lookup_hit`),
/// labelled by model and salt.
const LMCACHE_LOOKUPS: &str = "lmcache_mp_lookup_requested";
const LMCACHE_HITS: &str = "lmcache_mp_lookup_hit";
/// The unit suffixes a counter may carry before `_total`.
const COUNTER_UNITS: &[&str] = &["tokens", "requests", "failures", "allocations", "events"];

/// ADR 0033 §12: the cache figures of one LMCache `/metrics` body. Each is
/// `None` when absent or unparseable: unknown, never zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LmcacheFigures {
    pub used_bytes: Option<u64>,
    pub usage_ppm: Option<u32>,
    pub inflight_bytes: Option<u64>,
    pub alloc_failures: Option<u64>,
    pub lookups: Option<u64>,
    pub hits: Option<u64>,
    pub l2_bytes: Option<u64>,
}

/// The sum of every series of the exact metric `name` (one per adapter, per
/// model, per salt: every label set is summed and none is kept).
fn summed(text: &str, name: &str) -> Option<u64> {
    let series = labelled(text, name).ok()?;
    if series.is_empty() {
        return None;
    }
    series
        .into_iter()
        .try_fold(0_u64, |sum, (_, value)| sum.checked_add(whole(value)?))
}

/// One counter `base`, exported as `<base>_total` or `<base>_<unit>_total`,
/// summed over every label set. `None` when no such series is there or one
/// does not parse.
fn counter(text: &str, base: &str) -> Option<u64> {
    let mut names: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split(['{', ' ', '\t']).next())
        .filter(|name| {
            name.strip_prefix(base)
                .and_then(|rest| rest.strip_suffix("_total"))
                .is_some_and(|unit| {
                    unit.is_empty()
                        || unit
                            .strip_prefix('_')
                            .is_some_and(|unit| COUNTER_UNITS.contains(&unit))
                })
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return None;
    }
    names
        .into_iter()
        .try_fold(0_u64, |sum, name| sum.checked_add(summed(text, name)?))
}

/// ADR 0033 §12: the figures of one LMCache `/metrics` body: L1 use (bytes
/// and ratio), in-flight load reservations, allocation failures, the lookup
/// and hit token counters summed over their model and salt labels (no label
/// is kept), and L2 use (expected absent or zero). A figure LMCache does not
/// export yet (`/metrics` is empty before the first store) or renamed is
/// unknown.
pub fn parse_lmcache_metrics(text: &str) -> LmcacheFigures {
    LmcacheFigures {
        used_bytes: summed(text, LMCACHE_L1_USED),
        usage_ppm: series(text, LMCACHE_L1_RATIO)
            .ok()
            .and_then(|values| values.into_iter().reduce(f64::max))
            .filter(|ratio| *ratio <= 1.0)
            .map(|ratio| {
                ((ratio * f64::from(KV_USAGE_PPM_FULL)).round() as u32).min(KV_USAGE_PPM_FULL)
            }),
        inflight_bytes: summed(text, LMCACHE_INFLIGHT),
        alloc_failures: counter(text, LMCACHE_ALLOC_FAILURES),
        lookups: counter(text, LMCACHE_LOOKUPS),
        hits: counter(text, LMCACHE_HITS),
        l2_bytes: summed(text, LMCACHE_L2_USED),
    }
}

/// A host agent samples the scopes its ingress holds open.
impl LoadSource for Ingress {
    fn targets(&self) -> Vec<LoadTarget> {
        self.load_targets().unwrap_or_default()
    }
    fn drain_latency(&self, scope: &IngressScope) -> Vec<(String, Histogram)> {
        Ingress::drain_latency(self, scope).unwrap_or_default()
    }
}

/// ADR 0033 §12: a host agent's load source: the scopes its ingress holds
/// open, and the LMCache servers of the launches its journal holds Ready or
/// parked.
pub struct HostLoad {
    ingress: Arc<Ingress>,
    journal: Arc<crate::journal::HostJournal>,
    /// ADR 0033 §6: the per-process GPU sampler, once the role names one.
    residency: Mutex<Option<Arc<crate::process_residency::ResidencySampler>>>,
}

impl HostLoad {
    pub fn new(ingress: Arc<Ingress>, journal: Arc<crate::journal::HostJournal>) -> Arc<Self> {
        Arc::new(Self {
            ingress,
            journal,
            residency: Mutex::new(None),
        })
    }

    /// Read the server groups' GPU memory from `sampler`.
    pub fn set_residency(&self, sampler: Arc<crate::process_residency::ResidencySampler>) {
        *self.residency.lock().unwrap_or_else(|p| p.into_inner()) = Some(sampler);
    }
}

impl LoadSource for HostLoad {
    fn targets(&self) -> Vec<LoadTarget> {
        self.ingress.targets()
    }
    fn drain_latency(&self, scope: &IngressScope) -> Vec<(String, Histogram)> {
        LoadSource::drain_latency(self.ingress.as_ref(), scope)
    }
    fn lmcache_targets(&self) -> Vec<LmcacheTarget> {
        let Ok(launches) = self.journal.ready_launches() else {
            return Vec::new();
        };
        let sample = self
            .residency
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .map(|sampler| sampler.current())
            .unwrap_or_default();
        launches
            .into_iter()
            .filter_map(|(command, group)| {
                let grant = command.action.launch_plan()?.lmcache?;
                let server = capyctl_domain::completion::lmcache_members(&group);
                if server.is_empty() {
                    return None;
                }
                Some(LmcacheTarget {
                    device_bytes: crate::process_residency::group_device_bytes(&sample, &server),
                    deployment_id: command.identity.deployment_id.clone(),
                    instance_index: command.identity.instance_index,
                    generation: command.identity.generation,
                    owned_handle: command.identity.command_id.clone(),
                    http: (std::net::Ipv4Addr::LOCALHOST, grant.http_port).into(),
                    server,
                    pool_bytes: grant.pool_bytes,
                })
            })
            .collect()
    }
}

/// ADR 0031 §4, §6: how one launch's engine is sampled. A custom engine has
/// no load gauges and is never matched against a built-in engine's metric
/// families: with an idle signal its sample carries 0 running and 0 waiting
/// only on a real idle reading of that signal (the SPEC §10 cancellation
/// evidence a remote host gives), and no engine figures otherwise; without
/// one it never carries engine figures, so it never reads quiescent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchSignal {
    /// A built-in engine: its family is read from `/metrics`.
    Builtin,
    /// A custom engine, with its manifest's idle signal when it has one.
    Custom(Option<capyctl_config::custom::IdleSignal>),
    /// ADR 0031 §6, AGENTS.md (uncertainty must retain accounting): the
    /// profile the launch was approved under is no longer held, so its
    /// engine is not known. Nothing is read and no engine figures are
    /// carried: its idleness is unknown, so the controller never settles its
    /// cancelling requests on a sample, only at the verified stop.
    Unknown,
}

/// Scrapes Ready scopes and builds bounded load reports for one host.
pub struct LoadReporter {
    source: Arc<dyn LoadSource>,
    client: reqwest::Client,
    host_id: String,
    interval: Duration,
    /// SPEC §17: bounded by the Ready scopes of this tick (pruned each tick).
    baselines: Mutex<EngineBaselines>,
    /// SPEC §§10, 17: the running limit each launch's engine reported. It is
    /// fixed for the launch's life, so it is read once; bounded and pruned as
    /// the baselines are.
    limits: Mutex<HashMap<LaunchKey, u32>>,
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

/// A loopback read of `path` on the target, with the launch's key when its
/// engine takes one (an embedded llama.cpp launch takes none, ADR 0029 §4; a
/// custom engine only when its manifest uses `{api_key}`, ADR 0031 §7).
fn keyed_get(client: &reqwest::Client, target: &LoadTarget, path: &str) -> reqwest::RequestBuilder {
    let request = client.get(format!("http://{}{path}", target.target));
    match target.native {
        Some(key) => request.bearer_auth(hex::encode(key)),
        None => request,
    }
}

impl LoadReporter {
    pub fn new(source: Arc<dyn LoadSource>, host_id: String) -> Result<Self, LoadError> {
        Ok(Self {
            source,
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
            limits: Mutex::new(HashMap::new()),
        })
    }
    pub fn with_interval(mut self, period: Duration) -> Result<Self, LoadError> {
        self.interval = load_interval(period)?;
        Ok(self)
    }
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// ADR 0031 §6: one read of a custom engine's idle signal on the same
    /// loopback target, with the launch's key, within the scrape bound.
    /// `Some(true)` only on a 200 answer that reads idle; anything missing,
    /// malformed, unreachable or non-200 is unknown (`None`), never idle.
    async fn idle_reading(
        &self,
        target: &LoadTarget,
        signal: &capyctl_config::custom::IdleSignal,
    ) -> Option<bool> {
        if !target.target.ip().is_loopback() {
            return None;
        }
        let read = async {
            let response = keyed_get(&self.client, target, &signal.url)
                .send()
                .await
                .ok()?;
            if response.status() != reqwest::StatusCode::OK {
                return None;
            }
            capyctl_adapters::custom::idle_reading(signal, &bounded_body(response).await?)
        };
        tokio::time::timeout(SCRAPE_TIMEOUT, read)
            .await
            .ok()
            .flatten()
    }

    async fn scrape(&self, target: &LoadTarget) -> Option<String> {
        if !target.target.ip().is_loopback() {
            return None;
        }
        let read = async {
            let response = keyed_get(&self.client, target, "/metrics")
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

    /// ADR 0029 §11: llama.cpp's KV usage from its `/slots`, on the same
    /// loopback target with the launch's key, within the scrape bound.
    async fn llamacpp_kv_usage(&self, target: &LoadTarget) -> Option<u32> {
        if !target.target.ip().is_loopback() {
            return None;
        }
        let read = async {
            let response = keyed_get(&self.client, target, LLAMACPP_SLOTS_PATH)
                .send()
                .await
                .ok()?;
            if response.status() != reqwest::StatusCode::OK {
                return None;
            }
            parse_llamacpp_slots(&bounded_body(response).await?)
        };
        tokio::time::timeout(SCRAPE_TIMEOUT, read)
            .await
            .ok()
            .flatten()
    }

    /// SPEC §§10, 17 (owner decision 2026-10-08): SGLang's resolved running
    /// limit, on the same loopback target with the launch's key, within the
    /// scrape bound.
    async fn sglang_max_running(&self, target: &LoadTarget) -> Option<u32> {
        if !target.target.ip().is_loopback() {
            return None;
        }
        let read = async {
            let response = keyed_get(&self.client, target, SGLANG_LOADS_PATH)
                .send()
                .await
                .ok()?;
            if response.status() != reqwest::StatusCode::OK {
                return None;
            }
            parse_sglang_max_running(&bounded_body(response).await?)
        };
        tokio::time::timeout(SCRAPE_TIMEOUT, read)
            .await
            .ok()
            .flatten()
    }

    /// The running limit of each target's launch: known from an earlier tick,
    /// else read now from an SGLang engine (the only one that reports it).
    async fn max_running(
        &self,
        targets: &[LoadTarget],
        families: &[Option<&'static str>],
    ) -> Vec<Option<u32>> {
        let known: Vec<Option<u32>> = {
            let mut limits = self.limits.lock().unwrap_or_else(|p| p.into_inner());
            // SPEC §17: limits of launches no longer Ready are dropped.
            let live: std::collections::HashSet<LaunchKey> =
                targets.iter().map(launch_key).collect();
            limits.retain(|key, _| live.contains(key));
            targets
                .iter()
                .map(|target| limits.get(&launch_key(target)).copied())
                .collect()
        };
        let read = futures::future::join_all(targets.iter().zip(families).zip(&known).map(
            |((target, family), known)| async move {
                match (known, family) {
                    (Some(_), _) => None,
                    (None, Some("sglang")) => self.sglang_max_running(target).await,
                    (None, _) => None,
                }
            },
        ))
        .await;
        let mut limits = self.limits.lock().unwrap_or_else(|p| p.into_inner());
        targets
            .iter()
            .zip(known)
            .zip(read)
            .map(|((target, known), read)| {
                if let Some(read) = read {
                    limits.insert(launch_key(target), read);
                }
                known.or(read)
            })
            .collect()
    }

    /// One tick: a sample per Ready scope, split into reports that each pass
    /// the W3 bounds. Empty when no scope is Ready.
    pub async fn reports(&self) -> Vec<pb::ReportLoad> {
        // ADR 0033 §12: each Ready or parked launch's LMCache server.
        let lmcache = self.lmcache_samples().await;
        let targets: Vec<_> = self
            .source
            .targets()
            .into_iter()
            .filter(|t| t.scope.host_id == self.host_id)
            .collect();
        // ADR 0031 §6: each launch is sampled by the signal frozen with it.
        let signals: Vec<LaunchSignal> = targets.iter().map(|t| t.signal.clone()).collect();
        // ADR 0031 §6: a custom engine's `/metrics` is never read, so no
        // built-in family is ever matched on it; nor is that of an engine
        // whose frozen profile is no longer held.
        let scraped =
            futures::future::join_all(targets.iter().zip(&signals).map(|(t, s)| async move {
                match s {
                    LaunchSignal::Builtin => self.scrape(t).await,
                    LaunchSignal::Custom(_) | LaunchSignal::Unknown => None,
                }
            }))
            .await;
        let idle =
            futures::future::join_all(targets.iter().zip(&signals).map(|(t, s)| async move {
                match s {
                    LaunchSignal::Custom(Some(signal)) => self.idle_reading(t, signal).await,
                    _ => None,
                }
            }))
            .await;
        let families: Vec<Option<&'static str>> = scraped
            .iter()
            .map(|body| body.as_deref().and_then(family_of).map(|(name, ..)| name))
            .collect();
        // Only a TensorFold scrape is folded with its health, and only a
        // llama.cpp scrape reads `/slots` for its KV usage.
        let side_reads = futures::future::join_all(targets.iter().zip(&families).map(
            |(target, family)| async move {
                match family {
                    Some("tensorfold") => SideRead::Health(self.health(target).await),
                    Some("llamacpp") => SideRead::KvUsage(self.llamacpp_kv_usage(target).await),
                    _ => SideRead::None,
                }
            },
        ))
        .await;
        let limits = self.max_running(&targets, &families).await;
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
            .zip(side_reads)
            .zip(limits)
            .zip(signals.iter().zip(idle))
            .map(
                |((((target, body), side_read), max_running), (signal, idle))| {
                    // A failed `/slots` read leaves the engine load out, as a
                    // failed scrape does.
                    let engine = match signal {
                        // ADR 0031 §6: nothing running, nothing waiting and
                        // nothing held, read from the idle signal itself.
                        LaunchSignal::Custom(_) => (idle == Some(true)).then_some(EngineLoad {
                            running: 0,
                            waiting: 0,
                            kv_usage_ppm: 0,
                        }),
                        // ADR 0031 §6, AGENTS.md: unknown idleness carries no
                        // engine figures, so it never reads quiescent.
                        LaunchSignal::Unknown => None,
                        LaunchSignal::Builtin => body.as_deref().and_then(|body| match side_read {
                            SideRead::Health(health) => parse_engine_load(body)
                                .map(|load| fold_tensorfold_health(load, health.as_ref())),
                            SideRead::KvUsage(kv) => parse_llamacpp_load(body, kv),
                            // ADR 0038 §6: no exposition family matched; a
                            // TensorRT-LLM body is its iteration entries.
                            SideRead::None => parse_engine_load(body)
                                .or_else(|| parse_tensorrt_llm_load(body.as_bytes())),
                        }),
                    };
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
                            max_running,
                        },
                    )
                },
            )
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
        let mut reports = batch(&self.host_id, samples.into_iter().map(|(_, s)| s).collect());
        reports.extend(batch_lmcache(&self.host_id, lmcache));
        reports
    }
}

impl LoadReporter {
    /// ADR 0033 §12: one loopback `/metrics` read of an LMCache server's
    /// admin port, unkeyed (the server has no key), within the scrape bound.
    async fn scrape_lmcache(&self, target: &LmcacheTarget) -> Option<String> {
        if !target.http.ip().is_loopback() {
            return None;
        }
        let read = async {
            let response = self
                .client
                .get(format!("http://{}/metrics", target.http))
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

    /// ADR 0033 §§6, 12: one sample per launch's LMCache server: its cache
    /// figures (unknown when the scrape failed) and its group's measured
    /// memory. Never readiness or release evidence.
    async fn lmcache_samples(&self) -> Vec<LmcacheSample> {
        let targets = self.source.lmcache_targets();
        let bodies =
            futures::future::join_all(targets.iter().map(|t| self.scrape_lmcache(t))).await;
        let mut samples: Vec<LmcacheSample> = targets
            .into_iter()
            .zip(bodies)
            .map(|(target, body)| {
                let figures = body
                    .as_deref()
                    .map(parse_lmcache_metrics)
                    .unwrap_or_default();
                LmcacheSample {
                    rss_bytes: crate::process_residency::group_resident_bytes(&target.server),
                    device_bytes: target.device_bytes,
                    deployment_id: target.deployment_id,
                    generation: target.generation,
                    owned_handle: target.owned_handle,
                    sampled_at_ms: capyctl_protocol::now_unix_ms(),
                    scrape_ok: body.is_some(),
                    used_bytes: figures.used_bytes,
                    usage_ppm: figures.usage_ppm,
                    inflight_bytes: figures.inflight_bytes,
                    alloc_failures: figures.alloc_failures,
                    // A hit count above the lookups read with it is not a
                    // figure LMCache could report; both stay unknown.
                    lookups: figures
                        .lookups
                        .filter(|lookups| figures.hits.is_none_or(|hits| hits <= *lookups)),
                    hits: figures
                        .hits
                        .filter(|hits| figures.lookups.is_none_or(|lookups| *hits <= lookups)),
                    l2_bytes: figures.l2_bytes,
                }
            })
            .collect();
        // One sample per scope and generation per report.
        samples.sort_by(|a, b| {
            (&a.deployment_id, a.generation).cmp(&(&b.deployment_id, b.generation))
        });
        samples.dedup_by(|a, b| a.deployment_id == b.deployment_id && a.generation == b.generation);
        samples
    }

    /// SPEC §17 (M80): this scope's ingress timings since the last tick and
    /// its engine histograms' growth since the last scrape.
    fn latency(
        &self,
        target: &LoadTarget,
        body: Option<&str>,
        baselines: &mut EngineBaselines,
    ) -> Option<SampleLatency> {
        let mut histograms = self.source.drain_latency(&target.scope);
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
    split(samples, |samples| {
        LoadReport {
            host_id: host_id.to_owned(),
            samples: samples.to_vec(),
            lmcache: Vec::new(),
        }
        .to_wire()
    })
}

/// ADR 0033 §12: LMCache samples in their own reports (a parked launch has
/// no load sample to ride beside), within the same bounds.
pub fn batch_lmcache(host_id: &str, samples: Vec<LmcacheSample>) -> Vec<pb::ReportLoad> {
    split(samples, |samples| {
        LoadReport {
            host_id: host_id.to_owned(),
            samples: Vec::new(),
            lmcache: samples.to_vec(),
        }
        .to_wire()
    })
}

fn split<T>(items: Vec<T>, wire: impl Fn(&[T]) -> pb::ReportLoad) -> Vec<pb::ReportLoad> {
    use prost::Message;
    let mut reports = Vec::new();
    let mut current: Vec<T> = Vec::new();
    for item in items {
        current.push(item);
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

#[cfg(test)]
mod lmcache_tests {
    use super::*;

    // T45 (ADR 0033 §12): lookups and hits are summed over every model and
    // salt label set and no label is kept; an absent figure stays unknown,
    // never zero; an L2 above zero is carried, flagged degraded.
    #[test]
    fn lmcache_metrics_sum_labels_and_keep_unknowns_unknown() {
        let text = "\
# TYPE lmcache_mp_lookup_requested_tokens_total counter
lmcache_mp_lookup_requested_tokens_total{model_name=\"m\",cache_salt=\"aa\"} 200
lmcache_mp_lookup_requested_tokens_total{model_name=\"m\",cache_salt=\"bb\"} 60
lmcache_mp_lookup_hit_tokens_total{model_name=\"m\",cache_salt=\"aa\"} 200
lmcache_mp_lookup_hit_tokens_total{model_name=\"m\",cache_salt=\"bb\"} 12
lmcache_mp_lookup_hit_l1_tokens_total{model_name=\"m\",cache_salt=\"aa\"} 999
lmcache_mp_l1_memory_usage_bytes 1395864371
lmcache_mp_l1_usage_ratio 0.325
lmcache_mp_l2_usage_bytes{l2_name=\"fs\"} 4096
";
        let figures = parse_lmcache_metrics(text);
        assert_eq!(figures.lookups, Some(260));
        assert_eq!(figures.hits, Some(212));
        assert_eq!(figures.used_bytes, Some(1_395_864_371));
        assert_eq!(figures.usage_ppm, Some(325_000));
        assert_eq!(figures.l2_bytes, Some(4096));
        // Not exported (yet): unknown, never zero.
        assert_eq!(figures.inflight_bytes, None);
        assert_eq!(figures.alloc_failures, None);
        assert_eq!(parse_lmcache_metrics(""), LmcacheFigures::default());
        // A malformed series leaves its figure unknown.
        let broken = parse_lmcache_metrics("lmcache_mp_l1_memory_usage_bytes nope\n");
        assert_eq!(broken.used_bytes, None);
        let sample = LmcacheSample {
            deployment_id: "d".into(),
            generation: 1,
            owned_handle: "h".into(),
            sampled_at_ms: 1,
            scrape_ok: true,
            used_bytes: figures.used_bytes,
            usage_ppm: figures.usage_ppm,
            inflight_bytes: None,
            alloc_failures: None,
            lookups: figures.lookups,
            hits: figures.hits,
            l2_bytes: figures.l2_bytes,
            rss_bytes: None,
            device_bytes: None,
        };
        assert!(sample.l2_degraded());
        // The salt never leaves the host: no label is in the report.
        let reports = batch_lmcache("host", vec![sample]);
        assert_eq!(reports.len(), 1);
        assert!(!format!("{:?}", reports[0]).contains("aa"));
    }
}
