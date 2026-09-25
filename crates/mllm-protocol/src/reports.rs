//! Typed, fail-closed shapes for the unsolicited host reports added in session
//! protocol version 2: engine load samples and owned-process exits. Like the
//! command boundary, this validates shape only. The receiver must still bind
//! the report to its authenticated session host, fence generations against
//! its own records and judge freshness.
use crate::pb;
use mllm_domain::completion::ProcessIdentity;
use mllm_domain::group::GroupIdentityError;

/// SPEC §17: the most load samples one report may carry. A host reports one
/// sample per Ready scope, so this bounds cardinality well above any real host.
pub const MAX_LOAD_SAMPLES: usize = 64;
/// Encoded size bound for one load report, half the 64 KiB session
/// message limit so a report can never crowd out command results.
pub const MAX_LOAD_REPORT_BYTES: usize = 32 * 1024;
/// Bound on engine queue gauges. Larger values are a parse error, not load.
pub const MAX_LOAD_GAUGE: u32 = 1 << 20;
/// KV usage is reported in parts per million of the engine's KV pool.
pub const KV_USAGE_PPM_FULL: u32 = 1_000_000;

/// SPEC §17 (M80): the latency series a host may report, with where each is
/// measured. `mllm` series are the host ingress's own clock; `engine` series
/// are the engine's own histograms, forwarded from its `/metrics`. A fixed
/// set, so a report's cardinality is bounded by the protocol.
pub const HOST_LATENCY_SERIES: &[(&str, LatencySource)] = &[
    // Ingress receive to the engine's response headers.
    ("ingress_time_to_headers", LatencySource::Mllm),
    // Ingress receive to the engine's first body byte.
    ("ingress_time_to_first_byte", LatencySource::Mllm),
    // Ingress receive to the engine's last body byte.
    ("ingress_time_to_last_byte", LatencySource::Mllm),
    ("engine_time_to_first_token", LatencySource::Engine),
    ("engine_e2e_request_latency", LatencySource::Engine),
    ("engine_queue_time", LatencySource::Engine),
    ("engine_prefill_time", LatencySource::Engine),
    ("engine_decode_time", LatencySource::Engine),
    ("engine_inter_token_latency", LatencySource::Engine),
];
/// Engine families a latency report may name.
pub const LATENCY_ENGINES: &[&str] = &["vllm", "sglang"];

/// Where a latency series is measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencySource {
    /// The engine's own histogram.
    Engine,
    /// mllm's own clock (router or host ingress).
    Mllm,
}

impl LatencySource {
    /// The name the management API reports (`source: engine | mllm`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Mllm => "mllm",
        }
    }
}

/// The source of a known host latency series.
pub fn host_series_source(series: &str) -> Option<LatencySource> {
    HOST_LATENCY_SERIES
        .iter()
        .find(|(name, _)| *name == series)
        .map(|(_, source)| *source)
}

/// SPEC §17: latency a host observed for one scope since its previous report.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleLatency {
    /// The engine family whose histograms were read, when known.
    pub engine: Option<String>,
    /// Non-empty deltas, one per series, names from [`HOST_LATENCY_SERIES`].
    pub histograms: Vec<(String, mllm_domain::latency::Histogram)>,
}

impl SampleLatency {
    fn try_from_wire(wire: pb::SampleLatency) -> Result<Option<Self>, GroupIdentityError> {
        let engine = match wire.engine.as_str() {
            "" => None,
            e if LATENCY_ENGINES.contains(&e) => Some(wire.engine),
            _ => return Err(GroupIdentityError),
        };
        if wire.histograms.len() > HOST_LATENCY_SERIES.len() {
            return Err(GroupIdentityError);
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut histograms = Vec::with_capacity(wire.histograms.len());
        for h in wire.histograms {
            if host_series_source(&h.series).is_none() || !seen.insert(h.series.clone()) {
                return Err(GroupIdentityError);
            }
            let histogram = mllm_domain::latency::Histogram::from_parts(
                h.bounds,
                h.counts,
                h.sum_seconds,
                h.count,
            )
            .map_err(|_| GroupIdentityError)?;
            // Only observations travel: an empty delta is never sent.
            if histogram.is_empty() {
                return Err(GroupIdentityError);
            }
            histograms.push((h.series, histogram));
        }
        Ok((engine.is_some() || !histograms.is_empty()).then_some(Self { engine, histograms }))
    }

    fn to_wire(&self) -> pb::SampleLatency {
        pb::SampleLatency {
            engine: self.engine.clone().unwrap_or_default(),
            histograms: self
                .histograms
                .iter()
                .map(|(series, h)| pb::LatencyHistogram {
                    series: series.clone(),
                    bounds: h.bounds().to_vec(),
                    counts: h.counts().to_vec(),
                    sum_seconds: h.sum(),
                    count: h.count(),
                })
                .collect(),
        }
    }
}

fn bounded_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128
}
fn bounded_handle(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 4096
}

/// Engine gauges from one successful loopback scrape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineLoad {
    pub running: u32,
    pub waiting: u32,
    pub kv_usage_ppm: u32,
}

/// SPEC §10: a routing hint for one Ready scope. It is never readiness,
/// admission or release evidence, and it is never journaled or persisted.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadSample {
    pub deployment_id: String,
    pub generation: i64,
    pub owned_handle: String,
    pub sampled_at_ms: i64,
    pub ingress_in_flight: u32,
    /// `None` when the engine scrape failed or timed out.
    pub engine: Option<EngineLoad>,
    /// SPEC §17 (M80): latency observed since the previous report, if any.
    pub latency: Option<SampleLatency>,
}

impl LoadSample {
    /// T18/T34: a sample counts only for the generation the receiver currently
    /// serves; an older or newer generation's sample is dropped, never merged.
    pub fn is_for_generation(&self, current_generation: i64) -> bool {
        self.generation == current_generation
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadReport {
    pub host_id: String,
    pub samples: Vec<LoadSample>,
}

impl TryFrom<pb::ReportLoad> for LoadReport {
    type Error = GroupIdentityError;
    fn try_from(report: pb::ReportLoad) -> Result<Self, Self::Error> {
        use prost::Message;
        if !bounded_id(&report.host_id)
            || report.samples.is_empty()
            || report.samples.len() > MAX_LOAD_SAMPLES
            || report.encoded_len() > MAX_LOAD_REPORT_BYTES
        {
            return Err(GroupIdentityError);
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut samples = Vec::with_capacity(report.samples.len());
        for sample in report.samples {
            if !bounded_id(&sample.deployment_id)
                || sample.generation <= 0
                || !bounded_handle(&sample.owned_handle)
                || sample.sampled_at_unix_ms < 0
                || sample.ingress_in_flight > MAX_LOAD_GAUGE
                || sample.running > MAX_LOAD_GAUGE
                || sample.waiting > MAX_LOAD_GAUGE
                || sample.kv_usage_ppm > KV_USAGE_PPM_FULL
                // A failed scrape carries no engine numbers at all.
                || (!sample.scrape_ok
                    && (sample.running != 0 || sample.waiting != 0 || sample.kv_usage_ppm != 0))
                // One sample per scope and generation per report.
                || !seen.insert((sample.deployment_id.clone(), sample.generation))
            {
                return Err(GroupIdentityError);
            }
            let latency = match sample.latency {
                Some(latency) => SampleLatency::try_from_wire(latency)?,
                None => None,
            };
            samples.push(LoadSample {
                latency,
                engine: sample.scrape_ok.then_some(EngineLoad {
                    running: sample.running,
                    waiting: sample.waiting,
                    kv_usage_ppm: sample.kv_usage_ppm,
                }),
                deployment_id: sample.deployment_id,
                generation: sample.generation,
                owned_handle: sample.owned_handle,
                sampled_at_ms: sample.sampled_at_unix_ms,
                ingress_in_flight: sample.ingress_in_flight,
            });
        }
        Ok(Self {
            host_id: report.host_id,
            samples,
        })
    }
}

impl LoadReport {
    pub fn to_wire(&self) -> pb::ReportLoad {
        pb::ReportLoad {
            host_id: self.host_id.clone(),
            samples: self
                .samples
                .iter()
                .map(|s| {
                    let engine = s.engine.unwrap_or(EngineLoad {
                        running: 0,
                        waiting: 0,
                        kv_usage_ppm: 0,
                    });
                    pb::LoadSample {
                        deployment_id: s.deployment_id.clone(),
                        generation: s.generation,
                        owned_handle: s.owned_handle.clone(),
                        sampled_at_unix_ms: s.sampled_at_ms,
                        ingress_in_flight: s.ingress_in_flight,
                        running: engine.running,
                        waiting: engine.waiting,
                        kv_usage_ppm: engine.kv_usage_ppm,
                        scrape_ok: s.engine.is_some(),
                        latency: s.latency.as_ref().map(SampleLatency::to_wire),
                    }
                })
                .collect(),
        }
    }
}

/// How an owned process ended, as far as the host observed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),
    Signal(i32),
    /// The agent saw the process gone but could not collect its status, for
    /// example a retained process it did not spawn itself.
    Unobserved,
}

/// SPEC §13.2: one owned process exited without a Terminate. The receiver closes
/// admission and keeps the reservation until Terminate returns absence evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberExit {
    pub host_id: String,
    pub deployment_id: String,
    pub generation: i64,
    pub owned_handle: String,
    pub process: ProcessIdentity,
    pub status: ExitStatus,
    pub observed_at_ms: i64,
}

impl TryFrom<pb::MemberExit> for MemberExit {
    type Error = GroupIdentityError;
    fn try_from(exit: pb::MemberExit) -> Result<Self, Self::Error> {
        let process = exit.process.ok_or(GroupIdentityError)?;
        let status = match (exit.exit_code, exit.exit_signal) {
            (Some(code), None) => ExitStatus::Code(code),
            (None, Some(signal)) if (1..=64).contains(&signal) => ExitStatus::Signal(signal),
            (None, None) => ExitStatus::Unobserved,
            _ => return Err(GroupIdentityError),
        };
        if !bounded_id(&exit.host_id)
            || !bounded_id(&exit.deployment_id)
            || exit.generation <= 0
            || !bounded_handle(&exit.owned_handle)
            || exit.observed_at_unix_ms < 0
            // PID plus start identity on one boot, never a PID alone.
            || process.pid == 0
            || process.start_ticks == 0
            || !bounded_id(&process.role)
            || !bounded_id(&process.boot_id)
            || process.presence != "gone"
        {
            return Err(GroupIdentityError);
        }
        Ok(Self {
            host_id: exit.host_id,
            deployment_id: exit.deployment_id,
            generation: exit.generation,
            owned_handle: exit.owned_handle,
            process: ProcessIdentity {
                role: process.role,
                pid: process.pid,
                boot_id: process.boot_id,
                start_ticks: process.start_ticks,
            },
            status,
            observed_at_ms: exit.observed_at_unix_ms,
        })
    }
}

impl MemberExit {
    pub fn to_wire(&self) -> pb::MemberExit {
        let (exit_code, exit_signal) = match self.status {
            ExitStatus::Code(code) => (Some(code), None),
            ExitStatus::Signal(signal) => (None, Some(signal)),
            ExitStatus::Unobserved => (None, None),
        };
        pb::MemberExit {
            host_id: self.host_id.clone(),
            deployment_id: self.deployment_id.clone(),
            generation: self.generation,
            owned_handle: self.owned_handle.clone(),
            process: Some(pb::OwnedProcessObservation {
                role: self.process.role.clone(),
                pid: self.process.pid,
                boot_id: self.process.boot_id.clone(),
                start_ticks: self.process.start_ticks,
                presence: "gone".into(),
            }),
            exit_code,
            exit_signal,
            observed_at_unix_ms: self.observed_at_ms,
        }
    }

    /// T34: an exit report for another generation than the one the receiver
    /// holds is stale and never closes the current generation's admission.
    pub fn is_for_generation(&self, current_generation: i64) -> bool {
        self.generation == current_generation
    }
}
