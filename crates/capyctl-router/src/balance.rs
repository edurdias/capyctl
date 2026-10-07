//! Instance selection and failover (ADR 0013 §10, owner decision D9, unit I3).
//!
//! A deployment may run several instances, on one host or across hosts. The
//! router chooses one per request among the candidates the lifecycle authority
//! reports, and fails over to the next only while nothing has reached an engine.
//!
//! # Candidates
//!
//! An instance is a candidate when it holds a runtime, its dispatch gate is open
//! (Ready, admission and dispatch enabled, deployment not suspended) and, for a
//! remote instance, its host has a live control session (SPEC §13.2). The gate
//! is re-checked by the lease grant in its own transaction, so a stale view
//! costs a failover, never a dispatch to a closed instance (SPEC §10, T18).
//!
//! # Score
//!
//! For each candidate, with `f` the router's own outstanding requests on that
//! instance incarnation:
//!
//! ```text
//! load    = max(f, running + waiting)   when a usable engine sample exists
//!         = f                           otherwise
//! penalty = ceil((kv - 0.80) / 0.20 * 8) when kv usage exceeds 80 %, else 0
//! score   = load + penalty
//! ```
//!
//! A sample is usable only when it is fresh (at most
//! [`capyctl_controller::load_table::LOAD_STALE_AFTER_MS`] old), was reported by the
//! host that serves the instance, names this generation, names this instance's
//! launch command, and carries engine gauges (a failed scrape is unknown load,
//! never zero). The lowest score wins. Equal scores are ordered by instance
//! index and then rotated by a per-deployment counter, so ties are spread
//! deterministically. Taking `max` with the router's own count, which changes
//! the moment a request is chosen, is what stops a burst from herding onto the
//! instance whose last sample looked idle; there is no smoothing state to
//! oscillate. The choice and the router's count for it are taken under one
//! lock, so concurrent requests see each other's choices.
//!
//! # Failover
//!
//! Only before upstream acceptance (SPEC §10, T38): the chosen instance's lease
//! is refused because its gate closed, its forwarder cannot be resolved for
//! exactly the generation the lease names, or its ingress refuses before
//! forwarding (a shutting-down refusal or a refused connection). Each is proof
//! nothing reached an engine; its lease is closed as not accepted and the next
//! candidate is tried, at most [`MAX_ATTEMPTS`] in all. A request that may have
//! been accepted is never replayed: its lease stays uncertain and charged.
//!
//! Every selection is logged with its inputs, one JSON line on stderr.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::Json;
use capyctl_adapters::traits::ChatForward;
use capyctl_controller::load_table::EngineGauges;
use capyctl_controller::{LeaseEnd, LeaseRefused, LifecyclePort, RequestLease, ServingInstance};

use crate::admission::InFlight;
use crate::RouterDeps;

/// KV usage (parts per million) above which a candidate is penalised.
pub const KV_PRESSURE_THRESHOLD_PPM: u32 = 800_000;
/// The penalty at 100 % KV usage, in requests.
pub const KV_PRESSURE_WEIGHT: u64 = 8;
/// The most instances one request is offered to (the first choice plus
/// failovers). SPEC §17: bounded work per request.
pub const MAX_ATTEMPTS: usize = 4;

type Refusal = (StatusCode, Json<serde_json::Value>);

/// The KV-pressure penalty for a usage in parts per million.
pub fn kv_penalty(kv_usage_ppm: u32) -> u64 {
    let usage = kv_usage_ppm.min(1_000_000);
    if usage <= KV_PRESSURE_THRESHOLD_PPM {
        return 0;
    }
    let over = u64::from(usage - KV_PRESSURE_THRESHOLD_PPM);
    let span = u64::from(1_000_000 - KV_PRESSURE_THRESHOLD_PPM);
    (over * KV_PRESSURE_WEIGHT).div_ceil(span)
}

/// The score of one candidate (see the module documentation).
pub fn score(router_in_flight: u64, engine: Option<EngineGauges>) -> u64 {
    match engine {
        Some(gauges) => {
            let queue = u64::from(gauges.running) + u64::from(gauges.waiting);
            router_in_flight
                .max(queue)
                .saturating_add(kv_penalty(gauges.kv_usage_ppm))
        }
        None => router_in_flight,
    }
}

/// The engine gauges of a sample the score may use, with its age.
pub fn usable_load(instance: &ServingInstance) -> Option<(EngineGauges, i64)> {
    let view = instance.load.as_ref()?;
    let same_host = instance.remote_host.as_deref() == Some(view.host_id.as_str());
    let same_launch = instance.launch_command_id.as_deref() == Some(view.owned_handle.as_str());
    if !view.fresh || !same_host || !same_launch || view.generation != instance.generation {
        return None;
    }
    view.engine.map(|gauges| (gauges, view.age_ms))
}

/// One ranked candidate with the inputs of its score.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scored {
    pub instance_index: u32,
    pub generation: i64,
    pub host: Option<String>,
    pub router_in_flight: u64,
    pub engine: Option<EngineGauges>,
    pub sample_age_ms: Option<i64>,
    pub score: u64,
    /// ADR 0028 §11: a multi-node group's head; its requests are watched
    /// for their first token.
    pub group: bool,
}

impl Scored {
    fn log(&self) -> serde_json::Value {
        serde_json::json!({
            "instance": self.instance_index,
            "generation": self.generation,
            "host": self.host,
            "router_in_flight": self.router_in_flight,
            "engine_running": self.engine.map(|e| e.running),
            "engine_waiting": self.engine.map(|e| e.waiting),
            "kv_usage_ppm": self.engine.map(|e| e.kv_usage_ppm),
            "sample_age_ms": self.sample_age_ms,
            "load_source": if self.engine.is_some() { "engine" } else { "router_in_flight" },
            "score": self.score,
        })
    }
}

/// Why an instance was not a candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub instance_index: u32,
    pub generation: i64,
    pub reason: &'static str,
}

/// The candidates of one request, best first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranking {
    pub order: Vec<Scored>,
    pub skipped: Vec<Skipped>,
}

/// Rank `instances` given the router's own counts and a tie rotation. Pure, so
/// the formula and its tie-break are tested directly.
pub fn rank(
    instances: &[ServingInstance],
    in_flight: impl Fn(i64) -> u64,
    rotation: u64,
) -> Ranking {
    let mut ranking = Ranking::default();
    for instance in instances {
        // Owner decision 2026-09-23: a frozen host is named as such first; its
        // dispatch is suspended too, so it would otherwise read as closed.
        let reason = if instance.host_unresponsive {
            Some("host_unresponsive")
        } else if instance.engine_exited {
            // SPEC §13.2 (W13): named for what closed it; its dispatch is
            // closed too and its cleanup is being settled.
            Some("engine_exited")
        } else if !instance.dispatch_open {
            Some("dispatch_closed")
        } else if !instance.host_live {
            Some("host_session_lost")
        } else {
            None
        };
        if let Some(reason) = reason {
            ranking.skipped.push(Skipped {
                instance_index: instance.instance_index,
                generation: instance.generation,
                reason,
            });
            continue;
        }
        let router_in_flight = in_flight(instance.generation);
        let load = usable_load(instance);
        let engine = load.map(|(gauges, _)| gauges);
        ranking.order.push(Scored {
            instance_index: instance.instance_index,
            generation: instance.generation,
            host: instance
                .remote_host
                .clone()
                .or_else(|| instance.host_id.clone()),
            router_in_flight,
            engine,
            sample_age_ms: load.map(|(_, age)| age),
            score: score(router_in_flight, engine),
            group: instance.group,
        });
    }
    ranking.order.sort_by_key(|c| (c.score, c.instance_index));
    // Ties rotate: each run of equal scores is rotated by the same counter.
    let mut start = 0;
    while start < ranking.order.len() {
        let score = ranking.order[start].score;
        let end = ranking.order[start..]
            .iter()
            .position(|c| c.score != score)
            .map_or(ranking.order.len(), |offset| start + offset);
        let run = end - start;
        if run > 1 {
            ranking.order[start..end].rotate_left((rotation % run as u64) as usize);
        }
        start = end;
    }
    ranking
}

/// ADR 0013 §10: the router's outstanding requests per instance incarnation,
/// and the per-deployment tie rotation. Entries are removed at zero.
#[derive(Default)]
pub struct InstanceCounts {
    inner: Mutex<CountsInner>,
}

#[derive(Default)]
struct CountsInner {
    counts: HashMap<(String, i64), u64>,
    rotation: HashMap<String, u64>,
}

impl InstanceCounts {
    fn lock(&self) -> std::sync::MutexGuard<'_, CountsInner> {
        // Plain counters: a panic elsewhere leaves nothing half-written.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn current(&self, deployment: &str, generation: i64) -> usize {
        self.lock()
            .counts
            .get(&(deployment.to_owned(), generation))
            .map_or(0, |n| *n as usize)
    }

    /// Rank and count the first choice under one lock, so concurrent requests
    /// see each other's choices.
    fn choose(&self, deployment: &str, instances: &[ServingInstance]) -> Ranking {
        let mut inner = self.lock();
        let rotation = {
            let counter = inner.rotation.entry(deployment.to_owned()).or_default();
            let value = *counter;
            *counter = counter.wrapping_add(1);
            value
        };
        let ranking = rank(
            instances,
            |generation| {
                inner
                    .counts
                    .get(&(deployment.to_owned(), generation))
                    .copied()
                    .unwrap_or(0)
            },
            rotation,
        );
        if let Some(first) = ranking.order.first() {
            *inner
                .counts
                .entry((deployment.to_owned(), first.generation))
                .or_default() += 1;
        }
        ranking
    }

    fn take(&self, deployment: &str, generation: i64) {
        *self
            .lock()
            .counts
            .entry((deployment.to_owned(), generation))
            .or_default() += 1;
    }

    fn release(&self, deployment: &str, generation: i64) {
        let mut inner = self.lock();
        let key = (deployment.to_owned(), generation);
        if let Some(count) = inner.counts.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                inner.counts.remove(&key);
            }
        }
    }
}

/// One counted request on one instance; the count drops with it.
struct InstanceSlot {
    inflight: Arc<InFlight>,
    deployment: String,
    generation: i64,
}

impl Drop for InstanceSlot {
    fn drop(&mut self) {
        self.inflight
            .instances
            .release(&self.deployment, self.generation);
    }
}

/// One offer of a request to one engine: its forwarder, its durable lease and
/// the authority that closes it, and its routing count.
pub struct Attempt {
    pub forward: Arc<dyn ChatForward>,
    lease: Option<(Arc<dyn LifecyclePort>, RequestLease)>,
    /// The instance incarnation, when one was chosen.
    pub generation: Option<i64>,
    /// SPEC §17: the chosen instance's index, for its timings.
    pub instance: Option<u32>,
    /// ADR 0028 §11: the first-token watch, for a group's head only.
    pub(crate) stall: Option<crate::stream::StallWatch>,
    _slot: Option<InstanceSlot>,
}

impl Attempt {
    /// An attempt at a forwarder the caller resolved itself.
    pub fn direct(
        forward: Arc<dyn ChatForward>,
        lease: Option<(Arc<dyn LifecyclePort>, RequestLease)>,
    ) -> Self {
        Self {
            forward,
            lease,
            generation: None,
            instance: None,
            stall: None,
            _slot: None,
        }
    }

    /// Close or retain the lease on the evidence (SPEC §10) and drop the
    /// routing count. A failed write leaves the lease charged. Returns whether
    /// a durable lease accounted for this attempt: when it did, an uncertain
    /// end stays charged in the ledger, so the caller's in-memory slot is not
    /// the only record of it.
    pub async fn settle(self, end: LeaseEnd) -> bool {
        match self.lease {
            Some((controller, lease)) => {
                crate::chat::close_lease(controller.as_ref(), Some(lease), end).await;
                true
            }
            None => false,
        }
    }
}

enum Route {
    /// The authority has no instance view: dispatch to the deployment whole.
    Whole { offered: bool },
    /// ADR 0013 §10: ranked instances, the next one to offer, and whether the
    /// first choice's count was already taken with the ranking.
    Instances {
        order: Vec<Scored>,
        next: usize,
        first_counted: bool,
    },
}

/// The offers one request may make, in order.
pub struct Plan {
    deps: RouterDeps,
    deployment: String,
    route: Route,
    last: Option<Refusal>,
}

impl Plan {
    /// Read the deployment's serving instances and rank them. Logs the
    /// selection with its inputs.
    pub fn new(deps: &RouterDeps, deployment: &str) -> Result<Self, Refusal> {
        let instances = deps
            .controller
            .serving_instances(deployment)
            .map_err(crate::chat::map_controller)?;
        let route = match instances {
            None => Route::Whole { offered: false },
            Some(instances) => {
                let ranking = deps.inflight.instances.choose(deployment, &instances);
                capyctl_domain::role_log::event(serde_json::json!({
                    "event": "router_selection",
                    "deployment": deployment,
                    "chosen": ranking.order.first().map(|c| c.generation),
                    "candidates": ranking.order.iter().map(Scored::log).collect::<Vec<_>>(),
                    "skipped": ranking.skipped.iter().map(|s| serde_json::json!({
                        "instance": s.instance_index,
                        "generation": s.generation,
                        "reason": s.reason,
                    })).collect::<Vec<_>>(),
                }));
                let last = if ranking.skipped.is_empty() {
                    None
                } else {
                    Some(crate::chat::refusal(
                        "unavailable",
                        &format!(
                            "no instance of deployment {deployment} is open for dispatch; \
                             retry shortly"
                        ),
                    ))
                };
                return Ok(Self {
                    deps: deps.clone(),
                    deployment: deployment.to_owned(),
                    route: Route::Instances {
                        order: ranking.order,
                        next: 0,
                        first_counted: true,
                    },
                    last,
                });
            }
        };
        Ok(Self {
            deps: deps.clone(),
            deployment: deployment.to_owned(),
            route,
            last: None,
        })
    }

    /// The next offer, or the refusal that ends the request. Every refusal here
    /// is decided before anything reaches an engine.
    pub async fn next(&mut self) -> Result<Attempt, Refusal> {
        let deps = self.deps.clone();
        let deployment = self.deployment.clone();
        match &mut self.route {
            Route::Whole { offered } => {
                if std::mem::replace(offered, true) {
                    return Err(self.last.take().unwrap_or_else(|| {
                        crate::chat::refusal("unavailable", "no further engine to offer")
                    }));
                }
                let forward = deps
                    .forwards
                    .forwarder(&deployment)
                    .map_err(crate::chat::map_forwarder)?;
                let lease = crate::chat::open_lease(&deps, &deployment).await?;
                Ok(Attempt {
                    forward,
                    lease: lease.map(|lease| (deps.controller.clone(), lease)),
                    generation: None,
                    instance: None,
                    stall: None,
                    _slot: None,
                })
            }
            Route::Instances {
                order,
                next,
                first_counted,
            } => {
                while *next < order.len().min(MAX_ATTEMPTS) {
                    let candidate = order[*next].clone();
                    let counted = *next == 0 && std::mem::replace(first_counted, false);
                    *next += 1;
                    if !counted {
                        deps.inflight
                            .instances
                            .take(&deployment, candidate.generation);
                    }
                    let slot = InstanceSlot {
                        inflight: deps.inflight.clone(),
                        deployment: deployment.clone(),
                        generation: candidate.generation,
                    };
                    // SPEC §10: the lease names this incarnation, and is granted
                    // only while its gate is open.
                    let lease = match deps
                        .controller
                        .open_instance_lease(
                            &deployment,
                            candidate.generation,
                            deps.limits.max_requests_per_deployment,
                        )
                        .await
                    {
                        Ok(lease) => lease,
                        Err(LeaseRefused::Closed) => {
                            failover(&deployment, &candidate, "lease_refused_gate_closed");
                            self.last = Some(crate::chat::refusal(
                                "unavailable",
                                &format!(
                                    "dispatch to deployment {deployment} is closed; retry shortly"
                                ),
                            ));
                            continue;
                        }
                        Err(LeaseRefused::Full) => {
                            return Err(crate::chat::refusal(
                                "queue_full",
                                "outstanding request bound reached",
                            ))
                        }
                        Err(LeaseRefused::Unavailable(reason)) => {
                            return Err(crate::chat::refusal("unavailable", &reason))
                        }
                    };
                    // ADR 0013 §10: resolve exactly the incarnation the lease
                    // names. If it moved on or its gate closed since, nothing was
                    // sent: close the lease as not accepted and offer the next.
                    match deps
                        .forwards
                        .forwarder_for(&deployment, candidate.generation)
                    {
                        Ok(forward) => {
                            // ADR 0028 §11 (decided 2026-10-06): a request to a
                            // group's head is watched for its first token; a
                            // single-host instance's never is.
                            let stall = candidate.group.then(|| crate::stream::StallWatch {
                                authority: deps.controller.clone(),
                                deployment: deployment.clone(),
                                instance: candidate.instance_index,
                                generation: candidate.generation,
                                after: deps.controller.group_stall_timeout(),
                            });
                            return Ok(Attempt {
                                forward,
                                lease: lease.map(|lease| (deps.controller.clone(), lease)),
                                generation: Some(candidate.generation),
                                instance: Some(candidate.instance_index),
                                stall,
                                _slot: Some(slot),
                            });
                        }
                        Err(error) => {
                            crate::chat::close_lease(
                                deps.controller.as_ref(),
                                lease,
                                LeaseEnd::NotAccepted,
                            )
                            .await;
                            failover(&deployment, &candidate, "forwarder_unavailable");
                            self.last = Some(crate::chat::map_forwarder(error));
                        }
                    }
                }
                Err(self.last.take().unwrap_or_else(|| {
                    crate::chat::map_forwarder(crate::forwarders::ForwarderError::NoRuntime(
                        deployment.clone(),
                    ))
                }))
            }
        }
    }

    /// Record that the last offer was refused before forwarding, so the final
    /// answer, if no other offer succeeds, says so.
    pub fn refused(&mut self, generation: Option<i64>, reason: &str) {
        if let Some(generation) = generation {
            capyctl_domain::role_log::event(serde_json::json!({
                "event": "router_failover",
                "deployment": self.deployment,
                "generation": generation,
                "reason": "not_accepted",
                "detail": reason,
            }));
        }
        let code = if reason.contains("shutting down") {
            "shutting_down"
        } else {
            "unavailable"
        };
        self.last = Some(crate::chat::refusal(code, reason));
    }

    /// The refusal to answer with once no further offer succeeds.
    pub fn exhausted(&mut self) -> Refusal {
        self.last
            .take()
            .unwrap_or_else(|| crate::chat::refusal("unavailable", "no further engine to offer"))
    }
}

fn failover(deployment: &str, candidate: &Scored, reason: &str) {
    capyctl_domain::role_log::event(serde_json::json!({
        "event": "router_failover",
        "deployment": deployment,
        "generation": candidate.generation,
        "instance": candidate.instance_index,
        "reason": reason,
    }));
}

#[cfg(test)]
mod tests;
