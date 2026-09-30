//! ADR 0018 §4: retiring a runtime profile on one host. The durable
//! retirement is written with the reference check (`Store::
//! begin_profile_retirement`); a drained retirement stops each instance
//! through the ordinary stop path (`OwnedActionSource::drain_stop`: drain up
//! to `switching.drain_timeout`, then terminate, gone evidence required) and
//! confirms only on that evidence. Nothing here releases accounting.
use crate::actions::OwnedActionSource;
use crate::configuration::ConfigurationFailure;
use capyctl_controller::profile_retirement::{ProfileRetirements, RetirementStep};
use capyctl_store::profile_retirement::{RetirementProgress, RetirementStart};
use std::sync::Arc;
use std::time::Duration;

/// ADR 0018 §4 (owner decision 2026-09-25): the same bound as `capyctl drain host`.
pub const RETIREMENT_WINDOW: Duration = Duration::from_secs(900);

/// Wall-clock milliseconds since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct StoreRetirements {
    source: Arc<OwnedActionSource>,
    window: Duration,
    clock: Clock,
    /// Each retirement's bound as its `begin` read it, so a poll whose store
    /// read fails still ends holding once the bound passes.
    deadlines: std::sync::Mutex<std::collections::HashMap<(String, String, String), i64>>,
    /// review decision I1: the key each request's retirement stands under,
    /// when the request resumed one first written under another key.
    standing: std::sync::Mutex<std::collections::HashMap<(String, String, String), String>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// The store could not be read or written; nothing was decided.
fn store_failure() -> ConfigurationFailure {
    ConfigurationFailure::ReconciliationRequired
}

impl StoreRetirements {
    pub fn new(source: Arc<OwnedActionSource>) -> Self {
        Self {
            source,
            window: RETIREMENT_WINDOW,
            clock: Arc::new(now_ms),
            deadlines: Default::default(),
            standing: Default::default(),
        }
    }
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }
    /// The wall clock the retirement's bound is judged by (tests).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

impl ProfileRetirements for StoreRetirements {
    fn begin(&self, host: &str, profile: &str, request: &str, drain: bool) -> RetirementStep {
        let commands = self.source.commands();
        let now = (self.clock)();
        let window = i64::try_from(self.window.as_millis()).unwrap_or(i64::MAX);
        let deadline = now.saturating_add(window);
        // review decision I1: a retirement already standing for (host,
        // profile) is resumed under the key it was first written with, so a
        // retried remove never conflicts and its stops replay their receipts.
        let (key, started) = match commands.read(|store| {
            store.begin_profile_retirement_keyed(host, profile, request, now, deadline, drain)
        }) {
            Ok(started) => started,
            Err(_) => {
                return RetirementStep::Refused(
                    "the server's store is unavailable; nothing was removed".into(),
                )
            }
        };
        if key != request {
            if let Ok(mut standing) = self.standing.lock() {
                standing.insert((host.into(), profile.into(), request.into()), key.clone());
            }
        }
        let key = key.as_str();
        let named = match started {
            RetirementStart::Clear => return RetirementStep::Confirmed,
            RetirementStart::InUse(named) => {
                return RetirementStep::InUse(named.into_iter().map(|c| c.name).collect())
            }
            RetirementStart::Draining(named) => named,
        };
        // A retried retirement reuses its row, and with it the time and
        // deadline first written: each stop's request (and so its receipt)
        // stays identical.
        let (recorded, deadline) = match commands
            .read(|store| store.profile_retirement_span(host, profile, key))
        {
            Ok(Some(span)) => span,
            _ => {
                return RetirementStep::Refused(
                    "the retirement ended before its stops were issued; nothing was removed".into(),
                )
            }
        };
        if let Ok(mut known) = self.deadlines.lock() {
            known.insert((host.into(), profile.into(), key.into()), deadline);
        }
        let names: Vec<String> = named.iter().map(|c| c.name.clone()).collect();
        let mut first = Some(named.iter().map(|c| c.drain()).collect::<Vec<_>>());
        let rounds = crate::drain::drain_rounds(
            &mut || match first.take() {
                Some(named) => Ok(named),
                None => commands
                    .read(|store| store.profile_candidates(host, profile))
                    .map(|found| found.iter().map(|c| c.drain()).collect())
                    .map_err(|_| store_failure()),
            },
            // One key per instance under the retirement's key: a retried
            // retirement replays each stop's receipt instead of issuing another.
            // The stop is the ordinary one (drain, then terminate; cleanup only
            // on gone evidence).
            &mut |candidate| {
                // The ordinary stop refuses a deadline further than the
                // launch's request deadline from its acceptance; bound it by
                // that span from the retirement's start, which a retry reads
                // back unchanged. The retirement itself still waits to its own
                // bound.
                let span = commands.owner_for_read().ok().and_then(|owner| {
                    owner
                        .store()
                        .instance_stop_window_ms(&candidate.deployment_id, candidate.instance)
                        .ok()
                        .flatten()
                });
                let stop_deadline =
                    span.map_or(deadline, |span| deadline.min(recorded.saturating_add(span)));
                self.source.drain_stop(
                    &candidate.deployment_id,
                    candidate.instance,
                    candidate.revision,
                    &format!(
                        "retire:{key}:{}:{}",
                        candidate.deployment_id, candidate.instance
                    ),
                    stop_deadline,
                )
            },
            &mut |issued| {
                commands
                    .read(|store| store.record_profile_retirement_stops(host, profile, key, issued))
                    .map_err(|_| store_failure())
            },
        );
        match rounds {
            Ok(rounds) if rounds.issued.is_empty() && !rounds.refused.is_empty() => {
                // Nothing could be stopped: end the retirement unconfirmed.
                let _ = commands.read(|store| store.cancel_profile_retirement(host, profile, key));
                RetirementStep::Holding(names)
            }
            // A stop refused beside others issued keeps its instance named, so
            // the retirement waits and ends holding at its bound, never
            // confirmed on a guess.
            Ok(_) => RetirementStep::Draining(names),
            Err(_) => {
                // Stops already issued stay durable and complete through the
                // ordinary cleanup path; only the retirement is withdrawn.
                let _ = commands.read(|store| store.cancel_profile_retirement(host, profile, key));
                RetirementStep::Refused(
                    "the stops could not all be recorded; the profile was not removed".into(),
                )
            }
        }
    }

    fn poll(&self, host: &str, profile: &str, request: &str) -> Option<RetirementStep> {
        let request_entry = (host.to_owned(), profile.to_owned(), request.to_owned());
        let resumed = self
            .standing
            .lock()
            .ok()
            .and_then(|standing| standing.get(&request_entry).cloned());
        let key = resumed.as_deref().unwrap_or(request);
        // ADR 0018 §4: past the retirement's bound an unsettled retirement ends
        // unconfirmed (`Expired`), so this poll answers `Holding` rather than
        // waiting without end.
        let now = (self.clock)();
        let progress = self
            .source
            .commands()
            .read(|store| store.profile_retirement_progress(host, profile, key, now));
        let entry = (host.to_owned(), profile.to_owned(), key.to_owned());
        let step = match progress {
            Ok(RetirementProgress::Waiting(_)) => None,
            Ok(RetirementProgress::Settled) => Some(RetirementStep::Confirmed),
            Ok(RetirementProgress::Holding(names)) | Ok(RetirementProgress::Expired(names)) => {
                Some(RetirementStep::Holding(names))
            }
            Ok(RetirementProgress::Gone) => Some(RetirementStep::Refused(
                "the retirement ended before it was confirmed".into(),
            )),
            // A store that cannot be read confirms nothing: try again while
            // the bound stands, then end holding, unconfirmed.
            Err(_) => {
                let bound = self
                    .deadlines
                    .lock()
                    .ok()
                    .and_then(|known| known.get(&entry).copied());
                bound
                    .is_some_and(|deadline| now >= deadline)
                    .then(|| RetirementStep::Holding(Vec::new()))
            }
        };
        if step.is_some() {
            if let Ok(mut known) = self.deadlines.lock() {
                known.remove(&entry);
            }
            if let Ok(mut standing) = self.standing.lock() {
                standing.remove(&request_entry);
            }
        }
        step
    }
}
