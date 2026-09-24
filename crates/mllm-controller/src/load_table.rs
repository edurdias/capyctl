//! SPEC §10, ADR 0013 §10 (owner decision D9): the controller's in-memory
//! table of engine load that host agents report over their control sessions.
//!
//! A sample is a routing hint only. It is never readiness, admission, release
//! or placement evidence. The one narrow exception (W12, SPEC §10): after a
//! fresh model probe, a quiescent sample of the adopted launch is the completion
//! observation that closes a crashed session's request leases, never a
//! reservation (`crate::remote_readiness`). It is never journaled or persisted: a controller
//! restart starts with an empty table and the router falls back to its own
//! in-flight counts until fresh samples arrive.
//!
//! Samples are keyed by (deployment, generation), which identifies exactly one
//! instance incarnation (ADR 0013 §5). A read names the instance the caller
//! currently serves, so a sample from an older or newer generation never
//! scores it (T18, T34). A sample older than [`LOAD_STALE_AFTER_MS`] is
//! reported as stale; one older than [`LOAD_RETAIN_MS`] is dropped outright,
//! and a host's samples are dropped when its control session ends.
use crate::agent_sessions::controller_time;
use mllm_protocol::reports::LoadReport;
use std::{collections::BTreeMap, sync::Mutex};

/// ADR 0013 §10: a sample older than this (on the controller clock) is
/// ignored for scoring and the router falls back to its own in-flight count.
pub const LOAD_STALE_AFTER_MS: i64 = 3_000;
/// Samples older than this are removed, so retired generations and hosts
/// that stopped reporting do not accumulate.
pub const LOAD_RETAIN_MS: i64 = 30_000;
/// SPEC §17: memory bounds. One host reports one sample per Ready scope, and
/// its agent ingress holds at most 128 scopes.
pub const MAX_LOAD_ENTRIES_PER_HOST: usize = 128;
pub const MAX_LOAD_ENTRIES: usize = 4096;

/// One instance incarnation: a deployment and the generation it drew.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct InstanceKey {
    pub deployment_id: String,
    pub generation: i64,
}

impl InstanceKey {
    pub fn new(deployment_id: impl Into<String>, generation: i64) -> Self {
        Self {
            deployment_id: deployment_id.into(),
            generation,
        }
    }
}

/// Engine gauges from one successful loopback scrape on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct EngineGauges {
    pub running: u32,
    pub waiting: u32,
    /// Parts per million of the engine's KV pool in use.
    pub kv_usage_ppm: u32,
}

/// The latest load sample for one instance, as the router reads it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LoadView {
    pub deployment_id: String,
    pub generation: i64,
    /// The authenticated host session the sample arrived on.
    pub host_id: String,
    /// The launch the host sampled (its launch command id).
    pub owned_handle: String,
    /// Sample time on the controller clock (a tolerated host clock lead is
    /// clamped to the receive time).
    pub sampled_at_ms: i64,
    pub age_ms: i64,
    /// `false` once the sample is older than [`LOAD_STALE_AFTER_MS`].
    pub fresh: bool,
    /// Requests the host ingress was forwarding for this instance.
    pub ingress_in_flight: u32,
    /// `None` when the host could not scrape the engine's metrics: missing
    /// metrics are unknown load, never zero.
    pub engine: Option<EngineGauges>,
}

impl LoadView {
    /// Engine running plus waiting requests, when the scrape succeeded.
    pub fn engine_queue(&self) -> Option<u32> {
        self.engine.map(|e| e.running.saturating_add(e.waiting))
    }
}

#[derive(Clone, Debug)]
struct Entry {
    host_id: String,
    owned_handle: String,
    sampled_at_ms: i64,
    ingress_in_flight: u32,
    engine: Option<EngineGauges>,
}

/// Why a whole report was refused. A refused report ends the host session;
/// individually unusable samples inside an accepted report are only skipped.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoadRejected {
    #[error("load report names another host than its session")]
    HostMismatch,
}

/// What one accepted report changed, for tests and diagnostics.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoadApplied {
    pub applied: usize,
    /// Future beyond the tolerated clock lead, already stale, out of order,
    /// claimed by another host, or over the table bounds.
    pub skipped: usize,
}

#[derive(Default)]
pub struct LoadTable {
    entries: Mutex<BTreeMap<InstanceKey, Entry>>,
}

impl LoadTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the samples of one validated report from `session_host`'s
    /// authenticated session. The report must name that host.
    pub fn accept(
        &self,
        session_host: &str,
        report: LoadReport,
        now_ms: i64,
    ) -> Result<LoadApplied, LoadRejected> {
        if report.host_id != session_host {
            return Err(LoadRejected::HostMismatch);
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| now_ms - entry.sampled_at_ms <= LOAD_RETAIN_MS);
        let mut outcome = LoadApplied::default();
        for sample in report.samples {
            let key = InstanceKey::new(sample.deployment_id, sample.generation);
            // SPEC §7: freshness is judged on the controller clock.
            let Some(sampled_at_ms) = controller_time(sample.sampled_at_ms, now_ms) else {
                outcome.skipped += 1;
                continue;
            };
            if now_ms - sampled_at_ms > LOAD_STALE_AFTER_MS {
                outcome.skipped += 1;
                continue;
            }
            let usable = match entries.get(&key) {
                // One incarnation lives on one host. While another host's
                // sample for it is fresh, a conflicting claim does not replace it.
                Some(old) if old.host_id != session_host => {
                    now_ms - old.sampled_at_ms > LOAD_STALE_AFTER_MS
                }
                // Out of order: an older sample never replaces a newer one.
                Some(old) => sampled_at_ms > old.sampled_at_ms,
                None => {
                    entries.len() < MAX_LOAD_ENTRIES
                        && entries
                            .values()
                            .filter(|e| e.host_id == session_host)
                            .count()
                            < MAX_LOAD_ENTRIES_PER_HOST
                }
            };
            if !usable {
                outcome.skipped += 1;
                continue;
            }
            entries.insert(
                key,
                Entry {
                    host_id: session_host.to_owned(),
                    owned_handle: sample.owned_handle,
                    sampled_at_ms,
                    ingress_in_flight: sample.ingress_in_flight,
                    engine: sample.engine.map(|e| EngineGauges {
                        running: e.running,
                        waiting: e.waiting,
                        kv_usage_ppm: e.kv_usage_ppm,
                    }),
                },
            );
            outcome.applied += 1;
        }
        Ok(outcome)
    }

    /// SPEC §13.2: a host whose control session ended reports nothing current.
    pub fn forget_host(&self, host_id: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| entry.host_id != host_id);
    }

    /// T34: drop every sample of `deployment_id` whose generation is not one
    /// the caller currently serves (a retired or replaced instance).
    pub fn retain_generations(&self, deployment_id: &str, live: &[i64]) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| key.deployment_id != deployment_id || live.contains(&key.generation));
    }

    /// The latest sample for exactly this instance, fresh or stale, within
    /// the retention bound.
    pub fn sample_at(&self, key: &InstanceKey, now_ms: i64) -> Option<LoadView> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key)?;
        let age_ms = now_ms.saturating_sub(entry.sampled_at_ms).max(0);
        (age_ms <= LOAD_RETAIN_MS).then(|| view(key, entry, age_ms))
    }

    /// ADR 0013 §10: the router's read. A fresh sample for exactly this
    /// instance, reported by the host the caller believes serves it. `None`
    /// means score on router in-flight alone.
    pub fn fresh_at(&self, key: &InstanceKey, host_id: &str, now_ms: i64) -> Option<LoadView> {
        self.sample_at(key, now_ms)
            .filter(|view| view.fresh && view.host_id == host_id)
    }

    /// As [`Self::fresh_at`] on the controller's current clock.
    pub fn fresh(&self, key: &InstanceKey, host_id: &str) -> Option<LoadView> {
        self.fresh_at(key, host_id, mllm_protocol::now_unix_ms())
    }

    /// Every retained sample, for status (sample age per instance).
    pub fn snapshot_at(&self, now_ms: i64) -> Vec<LoadView> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(key, entry)| {
                (
                    key,
                    entry,
                    now_ms.saturating_sub(entry.sampled_at_ms).max(0),
                )
            })
            .filter(|(_, _, age)| *age <= LOAD_RETAIN_MS)
            .map(|(key, entry, age)| view(key, entry, age))
            .collect()
    }
}

fn view(key: &InstanceKey, entry: &Entry, age_ms: i64) -> LoadView {
    LoadView {
        deployment_id: key.deployment_id.clone(),
        generation: key.generation,
        host_id: entry.host_id.clone(),
        owned_handle: entry.owned_handle.clone(),
        sampled_at_ms: entry.sampled_at_ms,
        age_ms,
        fresh: age_ms <= LOAD_STALE_AFTER_MS,
        ingress_in_flight: entry.ingress_in_flight,
        engine: entry.engine,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_protocol::reports::{EngineLoad, LoadSample};

    const NOW: i64 = 1_800_000_000_000;

    fn sample(deployment: &str, generation: i64, at: i64, running: u32) -> LoadSample {
        LoadSample {
            deployment_id: deployment.into(),
            generation,
            owned_handle: format!("launch-{deployment}-{generation}"),
            sampled_at_ms: at,
            ingress_in_flight: 1,
            engine: Some(EngineLoad {
                running,
                waiting: 2,
                kv_usage_ppm: 250_000,
            }),
            latency: None,
        }
    }
    fn report(host: &str, samples: Vec<LoadSample>) -> LoadReport {
        LoadReport {
            host_id: host.into(),
            samples,
        }
    }

    // T34 / ADR 0013 §10: samples are keyed by (deployment, generation); a read
    // for the current generation never sees another generation's load.
    #[test]
    fn a_read_names_one_generation_and_stale_generations_are_dropped() {
        let table = LoadTable::new();
        table
            .accept(
                "h1",
                report("h1", vec![sample("d", 3, NOW, 4), sample("d", 4, NOW, 9)]),
                NOW,
            )
            .unwrap();
        let g4 = table
            .fresh_at(&InstanceKey::new("d", 4), "h1", NOW)
            .unwrap();
        assert_eq!(g4.engine.unwrap().running, 9);
        assert_eq!(g4.engine_queue(), Some(11));
        assert!(table
            .fresh_at(&InstanceKey::new("d", 5), "h1", NOW)
            .is_none());
        table.retain_generations("d", &[4]);
        assert!(table.sample_at(&InstanceKey::new("d", 3), NOW).is_none());
        assert!(table.sample_at(&InstanceKey::new("d", 4), NOW).is_some());
    }

    // ADR 0013 §10: a sample older than 3 s is stale and not scored; one older
    // than the retention bound is gone.
    #[test]
    fn samples_age_into_stale_then_out_of_the_table() {
        let table = LoadTable::new();
        table
            .accept("h1", report("h1", vec![sample("d", 1, NOW, 1)]), NOW)
            .unwrap();
        let key = InstanceKey::new("d", 1);
        assert!(table
            .fresh_at(&key, "h1", NOW + LOAD_STALE_AFTER_MS)
            .is_some());
        let stale = table
            .sample_at(&key, NOW + LOAD_STALE_AFTER_MS + 1)
            .unwrap();
        assert!(!stale.fresh);
        assert!(table
            .fresh_at(&key, "h1", NOW + LOAD_STALE_AFTER_MS + 1)
            .is_none());
        assert!(table.sample_at(&key, NOW + LOAD_RETAIN_MS + 1).is_none());
        // A report that arrives already stale is never recorded.
        let late = table
            .accept(
                "h1",
                report("h1", vec![sample("e", 1, NOW - LOAD_STALE_AFTER_MS - 1, 1)]),
                NOW,
            )
            .unwrap();
        assert_eq!(
            late,
            LoadApplied {
                applied: 0,
                skipped: 1
            }
        );
    }

    // SPEC §7 / T34: host clocks are fenced; a report binds to its session host;
    // an older sample never replaces a newer one; a failed scrape is not zero.
    #[test]
    fn clock_host_and_order_fences() {
        let table = LoadTable::new();
        assert_eq!(
            table.accept("h1", report("h2", vec![sample("d", 1, NOW, 1)]), NOW),
            Err(LoadRejected::HostMismatch)
        );
        let future = table
            .accept(
                "h1",
                report("h1", vec![sample("d", 1, NOW + 10_000, 1)]),
                NOW,
            )
            .unwrap();
        assert_eq!(future.skipped, 1);
        // A tolerated lead is clamped to the controller clock.
        table
            .accept("h1", report("h1", vec![sample("d", 1, NOW + 200, 5)]), NOW)
            .unwrap();
        assert_eq!(
            table
                .sample_at(&InstanceKey::new("d", 1), NOW)
                .unwrap()
                .sampled_at_ms,
            NOW
        );
        let older = table
            .accept("h1", report("h1", vec![sample("d", 1, NOW - 100, 7)]), NOW)
            .unwrap();
        assert_eq!(older.skipped, 1);
        assert_eq!(
            table
                .sample_at(&InstanceKey::new("d", 1), NOW)
                .unwrap()
                .engine
                .unwrap()
                .running,
            5
        );
        // Another host cannot overwrite a fresh sample it does not own, and the
        // router's read refuses a sample from a host other than the expected one.
        let claimed = table
            .accept(
                "h2",
                report("h2", vec![sample("d", 1, NOW + 1, 0)]),
                NOW + 1,
            )
            .unwrap();
        assert_eq!(claimed.skipped, 1);
        assert!(table
            .fresh_at(&InstanceKey::new("d", 1), "h2", NOW)
            .is_none());
        let mut failed = sample("f", 1, NOW + 5, 0);
        failed.engine = None;
        table
            .accept("h1", report("h1", vec![failed]), NOW + 5)
            .unwrap();
        let view = table
            .fresh_at(&InstanceKey::new("f", 1), "h1", NOW + 5)
            .unwrap();
        assert_eq!(
            (view.engine, view.engine_queue(), view.ingress_in_flight),
            (None, None, 1)
        );
    }

    // SPEC §13.2 / T33: a lost host session leaves no current load behind.
    // SPEC §17: the table is bounded per host.
    #[test]
    fn session_loss_forgets_a_host_and_the_table_is_bounded() {
        let table = LoadTable::new();
        table
            .accept("h1", report("h1", vec![sample("d", 1, NOW, 1)]), NOW)
            .unwrap();
        table
            .accept("h2", report("h2", vec![sample("e", 1, NOW, 1)]), NOW)
            .unwrap();
        table.forget_host("h1");
        assert!(table.sample_at(&InstanceKey::new("d", 1), NOW).is_none());
        assert_eq!(table.snapshot_at(NOW).len(), 1);
        for chunk in 0..3 {
            let samples = (0..64)
                .map(|i| sample(&format!("x{chunk}-{i}"), 1, NOW, 0))
                .collect();
            table.accept("h3", report("h3", samples), NOW).unwrap();
        }
        let per_host = table
            .snapshot_at(NOW)
            .iter()
            .filter(|v| v.host_id == "h3")
            .count();
        assert_eq!(per_host, MAX_LOAD_ENTRIES_PER_HOST);
    }
}
