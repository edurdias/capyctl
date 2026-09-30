//! SPEC §17 (owner decision 2026-09-23, M80): host-reported latency
//! distributions, accumulated in memory.
//!
//! Host agents attach latency deltas to their W8 load reports: the host
//! ingress's own clock (`source: capyctl`) and the engine's own histograms where
//! the engine exposes them (`source: engine`). This table adds each delta into
//! one histogram per instance incarnation (deployment, generation) and series.
//!
//! Observability only: never readiness, admission, release or routing
//! evidence, never journaled or persisted. A controller restart starts empty.
//! Unlike load samples, a host's histograms outlive its session, so what was
//! measured before a disconnect or a switch can still be read; the table is
//! bounded by entry count instead, evicting the least recently updated entry.
use capyctl_domain::latency::Histogram;
use capyctl_protocol::reports::{host_series_source, LoadReport};
use std::{collections::BTreeMap, sync::Mutex};

/// SPEC §17: the most (instance incarnation, series) histograms retained. At
/// nine host series per instance this is several hundred incarnations.
pub const MAX_LATENCY_ENTRIES: usize = 4096;

#[derive(Clone, Debug)]
struct Entry {
    host_id: String,
    engine: Option<String>,
    histogram: Histogram,
    updated_ms: i64,
}

/// One accumulated host-side series, as the management API reads it.
#[derive(Clone, Debug)]
pub struct HostLatencyView {
    pub deployment_id: String,
    pub generation: i64,
    pub host_id: String,
    pub engine: Option<String>,
    pub series: String,
    /// `engine` or `capyctl`.
    pub source: &'static str,
    pub updated_ms: i64,
    pub histogram: Histogram,
}

#[derive(Default)]
pub struct LatencyTable {
    entries: Mutex<BTreeMap<(String, i64, String), Entry>>,
}

impl LatencyTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the latency deltas of one validated report from `session_host`'s
    /// authenticated session. A report naming another host adds nothing (the
    /// load table refuses it and ends the session). Returns the series added.
    pub fn accept(&self, session_host: &str, report: &LoadReport, now_ms: i64) -> usize {
        if report.host_id != session_host {
            return 0;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut added = 0;
        for sample in &report.samples {
            let Some(latency) = &sample.latency else {
                continue;
            };
            for (series, delta) in &latency.histograms {
                if host_series_source(series).is_none() {
                    continue;
                }
                let key = (
                    sample.deployment_id.clone(),
                    sample.generation,
                    series.clone(),
                );
                match entries.get_mut(&key) {
                    // One incarnation lives on one host: another host's claim on
                    // it is not merged into the first host's distribution.
                    Some(entry) if entry.host_id != session_host => continue,
                    Some(entry) => {
                        // A changed engine layout starts the series again.
                        if !entry.histogram.merge(delta) {
                            entry.histogram = delta.clone();
                        }
                        entry.updated_ms = now_ms;
                        if latency.engine.is_some() {
                            entry.engine = latency.engine.clone();
                        }
                    }
                    None => {
                        if entries.len() >= MAX_LATENCY_ENTRIES {
                            let oldest = entries
                                .iter()
                                .min_by_key(|(_, e)| e.updated_ms)
                                .map(|(k, _)| k.clone());
                            if let Some(oldest) = oldest {
                                entries.remove(&oldest);
                            }
                        }
                        entries.insert(
                            key,
                            Entry {
                                host_id: session_host.to_owned(),
                                engine: latency.engine.clone(),
                                histogram: delta.clone(),
                                updated_ms: now_ms,
                            },
                        );
                    }
                }
                added += 1;
            }
        }
        added
    }

    /// Every retained series, optionally for one deployment.
    pub fn snapshot(&self, deployment: Option<&str>) -> Vec<HostLatencyView> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|((d, _, _), _)| deployment.is_none_or(|want| want == d))
            .map(
                |((deployment_id, generation, series), entry)| HostLatencyView {
                    deployment_id: deployment_id.clone(),
                    generation: *generation,
                    host_id: entry.host_id.clone(),
                    engine: entry.engine.clone(),
                    series: series.clone(),
                    source: host_series_source(series).map_or("capyctl", |s| s.as_str()),
                    updated_ms: entry.updated_ms,
                    histogram: entry.histogram.clone(),
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_protocol::reports::{LoadSample, SampleLatency};

    fn report(host: &str, generation: i64, series: &str, seconds: &[f64]) -> LoadReport {
        let mut h = Histogram::capyctl();
        for s in seconds {
            h.observe(*s);
        }
        LoadReport {
            host_id: host.into(),
            samples: vec![LoadSample {
                deployment_id: "d".into(),
                generation,
                owned_handle: "launch".into(),
                sampled_at_ms: 1,
                ingress_in_flight: 0,
                engine: None,
                latency: Some(SampleLatency {
                    engine: Some("vllm".into()),
                    histograms: vec![(series.into(), h)],
                }),
            }],
        }
    }

    // SPEC §17 T38: deltas accumulate per incarnation and series; another
    // generation is kept apart; a report naming another host adds nothing.
    #[test]
    fn deltas_accumulate_per_incarnation_and_series() {
        let table = LatencyTable::new();
        assert_eq!(
            table.accept(
                "h1",
                &report("h1", 1, "ingress_time_to_first_byte", &[0.01, 0.02]),
                5
            ),
            1
        );
        table.accept(
            "h1",
            &report("h1", 1, "ingress_time_to_first_byte", &[0.03]),
            6,
        );
        table.accept(
            "h1",
            &report("h1", 2, "engine_time_to_first_token", &[0.5]),
            7,
        );
        assert_eq!(
            table.accept(
                "h1",
                &report("h2", 1, "ingress_time_to_first_byte", &[9.0]),
                8
            ),
            0
        );
        // Another host's claim on an incarnation is not merged.
        table.accept(
            "h2",
            &report("h2", 1, "ingress_time_to_first_byte", &[9.0]),
            9,
        );
        let all = table.snapshot(Some("d"));
        assert_eq!(all.len(), 2);
        let g1 = all.iter().find(|v| v.generation == 1).unwrap();
        assert_eq!(
            (g1.histogram.count(), g1.source, g1.host_id.as_str()),
            (3, "capyctl", "h1")
        );
        let g2 = all.iter().find(|v| v.generation == 2).unwrap();
        assert_eq!((g2.source, g2.engine.as_deref()), ("engine", Some("vllm")));
        assert!(table.snapshot(Some("other")).is_empty());
    }
}
