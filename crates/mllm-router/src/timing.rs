//! SPEC §17 (owner decision 2026-09-23, M80): the router's own per-request
//! timings.
//!
//! Every inference request is timed on the router's clock from the moment its
//! handler starts (`received`). The phases:
//!
//! | series | measured |
//! |---|---|
//! | `router_queue_wait` | held in the W10 waiting queue (0 when the deployment served at once) |
//! | `router_activation_wait` | awaiting the joined activation; only requests that waited for one |
//! | `router_selection` | reading and ranking the deployment's instances |
//! | `router_lease_grant` | opening request leases and resolving forwarders, failovers included |
//! | `router_pre_forward` | `received` to the start of the forward that was accepted |
//! | `router_upstream_first_byte` | that forward's start to its first upstream chunk (streaming) |
//! | `router_time_to_first_byte` | `received` to the first upstream chunk (streaming) |
//! | `router_time_to_first_content` | `received` to the first chunk carrying generated text (streaming) |
//! | `router_time_to_last_chunk` | `received` to the upstream's last chunk, or its whole response |
//! | `router_total` | `received` to the end of the response |
//!
//! Only requests the backend completed are recorded, so a refusal or an
//! uncertain end never shortens a distribution. Durations only: no prompt,
//! response or credential is stored. Distributions are kept per (deployment,
//! instance, generation, engine) in bounded histograms ([`MAX_TIMING_KEYS`]
//! keys, the mllm bucket layout per series).
//!
//! When the server enables `observability.timing_header`, each response also
//! carries its own timings: a non-streaming response in the `x-mllm-timing`
//! header; a streaming response carries the phases known before its first
//! byte in that header and the complete set as one SSE comment line
//! (`: x-mllm-timing {...}`) before `data: [DONE]`, which SSE clients ignore.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mllm_domain::latency::Histogram;
use serde::Serialize;

/// The response header and SSE comment name of a request's own timings.
pub const TIMING_HEADER: &str = "x-mllm-timing";
/// SPEC §17: the most (deployment, instance, generation, engine) keys kept.
/// Past it the least recently updated key is dropped.
pub const MAX_TIMING_KEYS: usize = 1024;

/// Every router series, in report order.
pub const ROUTER_SERIES: &[&str] = &[
    "router_queue_wait",
    "router_activation_wait",
    "router_selection",
    "router_lease_grant",
    "router_pre_forward",
    "router_upstream_first_byte",
    "router_time_to_first_byte",
    "router_time_to_first_content",
    "router_time_to_last_chunk",
    "router_total",
];

/// Which instance served a request, as far as the router knows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct TimingKey {
    pub deployment_id: String,
    /// `None` when the lifecycle authority reports no instance view.
    pub instance: Option<u32>,
    pub generation: Option<i64>,
    /// The deployment's engine family (`vllm`, `sglang`).
    pub engine: String,
}

struct Book {
    series: Vec<Histogram>,
    updated: Instant,
}

/// The router's bounded latency distributions and its timing-header switch.
#[derive(Default)]
pub struct LatencyRecorder {
    header: AtomicBool,
    book: Mutex<HashMap<TimingKey, Book>>,
    /// SPEC §17: the host-reported latency table, whose entries carry the
    /// engine family each host runs (the same source [`latency_report`] reads).
    hosts: std::sync::OnceLock<std::sync::Arc<mllm_controller::latency_table::LatencyTable>>,
}

/// One accumulated router series.
#[derive(Clone, Debug)]
pub struct RouterLatencyView {
    pub key: TimingKey,
    pub series: &'static str,
    pub histogram: Histogram,
}

impl LatencyRecorder {
    /// SPEC §17 (M80): `observability.timing_header`.
    pub fn set_timing_header(&self, enabled: bool) {
        self.header.store(enabled, Ordering::Relaxed);
    }
    pub fn timing_header(&self) -> bool {
        self.header.load(Ordering::Relaxed)
    }
    /// SPEC §17 (M80): the host latency table the timing header reads the
    /// engine family from. Set once; later calls are ignored.
    pub fn set_host_latency(
        &self,
        hosts: std::sync::Arc<mllm_controller::latency_table::LatencyTable>,
    ) {
        let _ = self.hosts.set(hosts);
    }
    /// The engine family the host running `deployment`'s incarnation
    /// `generation` reports, when it has reported one.
    fn host_engine(&self, deployment: &str, generation: i64) -> Option<String> {
        self.hosts
            .get()?
            .snapshot(Some(deployment))
            .into_iter()
            .find(|v| v.generation == generation && v.engine.is_some())
            .and_then(|v| v.engine)
    }

    fn record(&self, key: TimingKey, phases: &Phases) {
        let mut book = self.book.lock().unwrap_or_else(|p| p.into_inner());
        if !book.contains_key(&key) && book.len() >= MAX_TIMING_KEYS {
            let oldest = book
                .iter()
                .min_by_key(|(_, b)| b.updated)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                book.remove(&oldest);
            }
        }
        let entry = book.entry(key).or_insert_with(|| Book {
            series: ROUTER_SERIES.iter().map(|_| Histogram::mllm()).collect(),
            updated: Instant::now(),
        });
        entry.updated = Instant::now();
        for (histogram, value) in entry.series.iter_mut().zip(phases.values()) {
            if let Some(value) = value {
                histogram.observe(value.as_secs_f64());
            }
        }
    }

    /// Every non-empty series, optionally for one deployment.
    pub fn snapshot(&self, deployment: Option<&str>) -> Vec<RouterLatencyView> {
        let book = self.book.lock().unwrap_or_else(|p| p.into_inner());
        let mut out: Vec<RouterLatencyView> = book
            .iter()
            .filter(|(key, _)| deployment.is_none_or(|d| d == key.deployment_id))
            .flat_map(|(key, entry)| {
                ROUTER_SERIES
                    .iter()
                    .zip(&entry.series)
                    .filter(|(_, h)| !h.is_empty())
                    .map(|(series, histogram)| RouterLatencyView {
                        key: key.clone(),
                        series,
                        histogram: histogram.clone(),
                    })
            })
            .collect();
        out.sort_by(|a, b| (&a.key, a.series).cmp(&(&b.key, b.series)));
        out
    }
}

/// The measured phases of one request. `None` is not measured.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Phases {
    pub queue_wait: Option<Duration>,
    pub activation_wait: Option<Duration>,
    pub selection: Option<Duration>,
    pub lease_grant: Option<Duration>,
    pub pre_forward: Option<Duration>,
    pub upstream_first_byte: Option<Duration>,
    pub time_to_first_byte: Option<Duration>,
    pub time_to_first_content: Option<Duration>,
    pub time_to_last_chunk: Option<Duration>,
    pub total: Option<Duration>,
}

impl Phases {
    /// In [`ROUTER_SERIES`] order.
    fn values(&self) -> [Option<Duration>; 10] {
        [
            self.queue_wait,
            self.activation_wait,
            self.selection,
            self.lease_grant,
            self.pre_forward,
            self.upstream_first_byte,
            self.time_to_first_byte,
            self.time_to_first_content,
            self.time_to_last_chunk,
            self.total,
        ]
    }
}

/// One request's clock. Cheap to carry; records nothing until
/// [`RequestTiming::finish`] on a completed backend.
pub struct RequestTiming {
    recorder: Option<std::sync::Arc<LatencyRecorder>>,
    received: Instant,
    pub(crate) deployment: Option<String>,
    pub(crate) engine: Option<String>,
    pub(crate) instance: Option<u32>,
    pub(crate) generation: Option<i64>,
    forward_started: Option<Instant>,
    phases: Phases,
}

impl RequestTiming {
    /// Start the clock now, recording into `recorder`.
    pub fn start(recorder: std::sync::Arc<LatencyRecorder>) -> Self {
        Self {
            recorder: Some(recorder),
            ..Self::untracked()
        }
    }

    /// A clock that records nothing (callers that predate timing).
    pub fn untracked() -> Self {
        Self {
            recorder: None,
            received: Instant::now(),
            deployment: None,
            engine: None,
            instance: None,
            generation: None,
            forward_started: None,
            phases: Phases::default(),
        }
    }

    pub fn phases(&self) -> &Phases {
        &self.phases
    }

    pub(crate) fn resolved(&mut self, deployment: &str, engine: &str) {
        self.deployment = Some(deployment.to_owned());
        self.engine = Some(engine.to_owned());
    }
    pub(crate) fn queued(&mut self, queue: Duration, activation: Option<Duration>) {
        self.phases.queue_wait = Some(queue);
        self.phases.activation_wait = activation;
    }
    pub(crate) fn selected(&mut self, took: Duration) {
        self.phases.selection = Some(took);
    }
    pub(crate) fn leased(&mut self, took: Duration) {
        let so_far = self.phases.lease_grant.unwrap_or_default();
        self.phases.lease_grant = Some(so_far + took);
    }
    /// A forward to `instance`/`generation` starts now. A failover restarts
    /// the forward-relative marks.
    pub(crate) fn forwarding(&mut self, instance: Option<u32>, generation: Option<i64>) {
        let now = Instant::now();
        self.instance = instance;
        self.generation = generation;
        self.forward_started = Some(now);
        self.phases.pre_forward = Some(now - self.received);
        self.phases.upstream_first_byte = None;
        self.phases.time_to_first_byte = None;
        self.phases.time_to_first_content = None;
        self.phases.time_to_last_chunk = None;
    }
    /// One upstream chunk arrived; `content` when it carries generated text.
    pub(crate) fn chunk(&mut self, content: bool) {
        let now = Instant::now();
        if self.phases.time_to_first_byte.is_none() {
            self.phases.time_to_first_byte = Some(now - self.received);
            self.phases.upstream_first_byte = self.forward_started.map(|started| now - started);
        }
        if content && self.phases.time_to_first_content.is_none() {
            self.phases.time_to_first_content = Some(now - self.received);
        }
        self.phases.time_to_last_chunk = Some(now - self.received);
    }
    /// A non-streaming upstream response arrived whole.
    pub(crate) fn response(&mut self) {
        self.phases.time_to_last_chunk = Some(self.received.elapsed());
    }

    /// The backend completed: stamp the total and record the request.
    pub(crate) fn finish(&mut self) {
        self.phases.total = Some(self.received.elapsed());
        let (Some(recorder), Some(deployment)) = (&self.recorder, &self.deployment) else {
            return;
        };
        recorder.record(
            TimingKey {
                deployment_id: deployment.clone(),
                instance: self.instance,
                generation: self.generation,
                engine: self.engine.clone().unwrap_or_default(),
            },
            &self.phases,
        );
    }

    /// Whether this request's timings go on its response.
    pub(crate) fn header_enabled(&self) -> bool {
        self.recorder.as_ref().is_some_and(|r| r.timing_header())
    }

    /// The request's own timings as the header value: milliseconds per
    /// measured phase, with the instance that served it.
    pub fn header_value(&self) -> String {
        let ms = |d: Option<Duration>| d.map(|d| (d.as_secs_f64() * 1_000_000.0).round() / 1000.0);
        let p = &self.phases;
        // SPEC §17 (found live 2026-09-24, M80): the router resolves the
        // deployment's kind (`model`); the host that runs the instance reports
        // the engine family, as in `latency_report`. The kind stands only
        // until that host has reported.
        let host_engine = match (&self.recorder, &self.deployment, self.generation) {
            (Some(recorder), Some(deployment), Some(generation)) => {
                recorder.host_engine(deployment, generation)
            }
            _ => None,
        };
        serde_json::json!({
            "deployment": self.deployment,
            "engine": host_engine.or_else(|| self.engine.clone()),
            "instance": self.instance,
            "generation": self.generation,
            "queue_wait_ms": ms(p.queue_wait),
            "activation_wait_ms": ms(p.activation_wait),
            "selection_ms": ms(p.selection),
            "lease_grant_ms": ms(p.lease_grant),
            "pre_forward_ms": ms(p.pre_forward),
            "upstream_first_byte_ms": ms(p.upstream_first_byte),
            "time_to_first_byte_ms": ms(p.time_to_first_byte),
            "time_to_first_content_ms": ms(p.time_to_first_content),
            "time_to_last_chunk_ms": ms(p.time_to_last_chunk),
            "total_ms": ms(p.total),
        })
        .to_string()
    }
}

/// Whether one upstream SSE `data:` chunk carries generated text: any choice
/// whose delta has non-empty `content` or `reasoning_content`, or a
/// completion-style `text`. Parsed and dropped; nothing is kept.
pub(crate) fn carries_content(chunk: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(chunk) else {
        return false;
    };
    let non_empty = |v: &serde_json::Value| v.as_str().is_some_and(|s| !s.is_empty());
    value["choices"].as_array().is_some_and(|choices| {
        choices.iter().any(|choice| {
            let delta = &choice["delta"];
            non_empty(&delta["content"])
                || non_empty(&delta["reasoning_content"])
                || non_empty(&delta["reasoning"])
                || non_empty(&choice["text"])
        })
    })
}

/// One series as the management API reports it.
fn series_json(name: &str, tier: &str, source: &str, histogram: &Histogram) -> serde_json::Value {
    let summary = histogram.summary();
    serde_json::json!({
        "name": name,
        "tier": tier,
        "source": source,
        "count": summary.count,
        "sum_seconds": summary.sum_seconds,
        "mean_seconds": summary.mean_seconds,
        "p50_seconds": summary.p50_seconds,
        "p95_seconds": summary.p95_seconds,
        "p99_seconds": summary.p99_seconds,
        // Non-cumulative, non-empty buckets only; `le: null` is +Inf. Two
        // reads can be subtracted bucket by bucket to window a run.
        "buckets": histogram
            .buckets()
            .into_iter()
            .filter(|b| b.count > 0)
            .collect::<Vec<_>>(),
    })
}

/// SPEC §17 (M80): `GET /management/v1/metrics/latency`. Router series
/// (`tier: router`, `source: mllm`), host ingress series (`tier: ingress`,
/// `source: mllm`) and engine histograms (`tier: engine`, `source: engine`),
/// grouped per deployment and instance incarnation. Every value is
/// accumulated since this server started; percentiles are bucket estimates.
pub fn latency_report(
    router: &LatencyRecorder,
    hosts: &[mllm_controller::latency_table::HostLatencyView],
    deployment: Option<&str>,
) -> serde_json::Value {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Group {
        instance: Option<u32>,
        engine: Option<String>,
        host: Option<String>,
        series: Vec<serde_json::Value>,
    }
    // (deployment, generation) -> group; a router key with no generation is
    // a deployment served whole.
    let mut groups: BTreeMap<(String, Option<i64>), Group> = BTreeMap::new();
    for view in router.snapshot(deployment) {
        let group = groups
            .entry((view.key.deployment_id.clone(), view.key.generation))
            .or_default();
        group.instance = group.instance.or(view.key.instance);
        if !view.key.engine.is_empty() {
            group.engine.get_or_insert_with(|| view.key.engine.clone());
        }
        group
            .series
            .push(series_json(view.series, "router", "mllm", &view.histogram));
    }
    for view in hosts
        .iter()
        .filter(|v| deployment.is_none_or(|d| d == v.deployment_id))
    {
        let group = groups
            .entry((view.deployment_id.clone(), Some(view.generation)))
            .or_default();
        // The host runs the engine and reports its family; the router's key
        // carries the deployment's kind (found live 2026-09-24, M80).
        if view.engine.is_some() {
            group.engine = view.engine.clone();
        }
        group.host.get_or_insert_with(|| view.host_id.clone());
        let tier = if view.source == "engine" {
            "engine"
        } else {
            "ingress"
        };
        group.series.push(series_json(
            &view.series,
            tier,
            view.source,
            &view.histogram,
        ));
    }
    let mut deployments: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for ((deployment_id, generation), group) in groups {
        deployments
            .entry(deployment_id)
            .or_default()
            .push(serde_json::json!({
                "instance": group.instance,
                "generation": generation,
                "engine": group.engine,
                "host_id": group.host,
                "series": group.series,
            }));
    }
    serde_json::json!({
        "unit": "seconds",
        "percentiles": "bucket_estimate",
        "bucket_counts": "non_cumulative",
        "deployments": deployments
            .into_iter()
            .map(|(id, instances)| serde_json::json!({"deployment_id": id, "instances": instances}))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPEC §17: only text-bearing chunks count as first content.
    #[test]
    fn content_detection_ignores_role_only_chunks() {
        assert!(!carries_content(
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#
        ));
        assert!(!carries_content(
            r#"{"choices":[{"delta":{"content":""}}]}"#
        ));
        assert!(carries_content(
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#
        ));
        assert!(carries_content(
            r#"{"choices":[{"delta":{"reasoning_content":"so"}}]}"#
        ));
        assert!(!carries_content("not json"));
    }

    // SPEC §17 (found live 2026-09-24, M80): the router keyed requests by the
    // deployment's kind (`model`), so the report named every instance's
    // engine `model`. The host that runs the engine reports its family, and
    // that is the one the report carries.
    // T40
    #[test]
    fn the_report_names_the_engine_the_host_runs() {
        let recorder = LatencyRecorder::default();
        let phases = Phases {
            total: Some(Duration::from_millis(5)),
            ..Phases::default()
        };
        recorder.record(
            TimingKey {
                deployment_id: "d".into(),
                instance: Some(0),
                generation: Some(1),
                engine: "model".into(),
            },
            &phases,
        );
        let mut histogram = Histogram::mllm();
        histogram.observe(0.01);
        let host = mllm_controller::latency_table::HostLatencyView {
            deployment_id: "d".into(),
            generation: 1,
            host_id: "h".into(),
            engine: Some("sglang".into()),
            series: "time_to_first_token".into(),
            source: "engine",
            updated_ms: 0,
            histogram,
        };
        let report = latency_report(&recorder, &[host], Some("d"));
        assert_eq!(
            report["deployments"][0]["instances"][0]["engine"], "sglang",
            "{report}"
        );
    }

    /// A host latency table holding one report for `d`, generation 1, from a
    /// host running `engine`.
    fn host_running(engine: &str) -> std::sync::Arc<mllm_controller::latency_table::LatencyTable> {
        use mllm_protocol::reports::{LoadReport, LoadSample, SampleLatency};
        let mut histogram = Histogram::mllm();
        histogram.observe(0.01);
        let table = std::sync::Arc::new(mllm_controller::latency_table::LatencyTable::new());
        table.accept(
            "h",
            &LoadReport {
                host_id: "h".into(),
                samples: vec![LoadSample {
                    deployment_id: "d".into(),
                    generation: 1,
                    owned_handle: "launch".into(),
                    sampled_at_ms: 1,
                    ingress_in_flight: 0,
                    engine: None,
                    latency: Some(SampleLatency {
                        engine: Some(engine.into()),
                        histograms: vec![("ingress_time_to_last_byte".into(), histogram)],
                    }),
                }],
            },
            1,
        );
        table
    }

    fn header_engine(
        recorder: std::sync::Arc<LatencyRecorder>,
        generation: i64,
    ) -> serde_json::Value {
        let mut timing = RequestTiming::start(recorder);
        // The router resolves the deployment's kind, which live is `model`.
        timing.resolved("d", "model");
        timing.forwarding(Some(0), Some(generation));
        serde_json::from_str::<serde_json::Value>(&timing.header_value()).unwrap()["engine"].clone()
    }

    // SPEC §17 (found live 2026-09-24, M80): the `x-mllm-timing` header named
    // the engine by the deployment's kind (`model`). Like the latency report,
    // it carries the engine family the host that runs the instance reports.
    // T40
    #[test]
    fn the_timing_header_names_the_engine_the_host_runs() {
        for engine in ["vllm", "sglang"] {
            let recorder = std::sync::Arc::new(LatencyRecorder::default());
            recorder.set_host_latency(host_running(engine));
            assert_eq!(header_engine(recorder.clone(), 1), engine);
            // Another incarnation has no host report yet: the router's own
            // resolution stands.
            assert_eq!(header_engine(recorder, 2), "model");
        }
    }

    // SPEC §17: the key space is bounded.
    #[test]
    fn keys_are_bounded() {
        let recorder = LatencyRecorder::default();
        let phases = Phases {
            total: Some(Duration::from_millis(5)),
            ..Phases::default()
        };
        for i in 0..(MAX_TIMING_KEYS + 10) {
            recorder.record(
                TimingKey {
                    deployment_id: format!("d{i}"),
                    instance: Some(0),
                    generation: Some(1),
                    engine: "vllm".into(),
                },
                &phases,
            );
        }
        assert_eq!(recorder.snapshot(None).len(), MAX_TIMING_KEYS);
    }
}
