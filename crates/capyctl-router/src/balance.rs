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
//! That is also why a request admitted without waiting is ranked on the view
//! admission read to find its deployment servable ([`InstanceView`]) rather
//! than on a second read of the same rows.
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
//! Every selection is logged with its inputs, one JSON line on stderr, with
//! the request's IDs (SPEC §10, owner decision 2026-10-19). Since CapyCTL's
//! ID is the first granted lease's, the lines of an offer are written once
//! the offer has been decided, when that ID is known.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::Json;
use capyctl_adapters::traits::ChatForward;
use capyctl_controller::load_table::EngineGauges;
use capyctl_controller::{LeaseEnd, LeaseRefused, LifecyclePort, RequestLease, ServingInstance};

use crate::admission::InFlight;
use crate::request_id::RequestIds;
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
    // A remote instance's sample comes from its host's session; an embedded
    // one's from the role's own sampling, under the host it is placed on.
    let serving_host = instance
        .remote_host
        .as_deref()
        .or(instance.host_id.as_deref());
    let same_host = serving_host == Some(view.host_id.as_str());
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
    /// ADR 0035 §7: the number of GPUs of one host it runs on; above 1 its
    /// requests are watched for their first token too.
    pub local_tensor_parallel: u32,
    /// ADR 0032 §2: served through its host's ingress, not by an engine the
    /// router reaches directly.
    pub remote: bool,
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
            local_tensor_parallel: instance.local_tensor_parallel,
            remote: instance.remote_host.is_some(),
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
    /// SPEC §10: the request's IDs, for the lease's role-log lines.
    ids: Option<Arc<RequestIds>>,
    /// ADR 0032 §1–§3: set when this attempt's lane carries CapyCTL's request
    /// ID and the authority can ask its engine to abort a request by it.
    abort: Option<AbortLane>,
    _slot: Option<InstanceSlot>,
}

/// ADR 0032 §3: the instance incarnation an abort is sent to.
struct AbortLane {
    deployment: String,
    generation: i64,
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
            ids: None,
            abort: None,
            _slot: None,
        }
    }

    /// ADR 0032 §1: CapyCTL's request ID (the first granted lease's, never the
    /// caller's), for a sink to hand the forwarder, on a capable lane with a
    /// durable lease only; `None` everywhere else.
    pub(crate) fn request_id(&self) -> Option<String> {
        self.abort.as_ref()?;
        self.lease.as_ref()?;
        Some(self.ids.as_ref()?.capyctl().to_owned())
    }

    /// ADR 0032 §3–§4: what reading a cut request to its own end needs, taken
    /// before [`Attempt::settle`] consumes the lease: the authority, the
    /// incarnation, the request ID and this attempt's own lease ID (a later
    /// failover lease is never named). `None` when the lane is not capable,
    /// which keeps today's path.
    pub(crate) fn after_cut(&self) -> Option<crate::stream::AfterCut> {
        let lane = self.abort.as_ref()?;
        let (authority, lease) = self.lease.as_ref()?;
        let ids = self.ids.as_ref()?;
        Some(crate::stream::AfterCut {
            authority: authority.clone(),
            deployment: lane.deployment.clone(),
            generation: lane.generation,
            request_id: ids.capyctl().to_owned(),
            lease_id: lease.id().to_owned(),
            ids: ids.clone(),
        })
    }

    /// Close or retain the lease on the evidence (SPEC §10) and drop the
    /// routing count. A failed write leaves the lease charged. Returns whether
    /// a durable lease accounted for this attempt: when it did, an uncertain
    /// end stays charged in the ledger, so the caller's in-memory slot is not
    /// the only record of it.
    pub async fn settle(self, end: LeaseEnd) -> bool {
        match self.lease {
            Some((controller, lease)) => {
                crate::chat::close_lease(
                    controller.as_ref(),
                    Some(lease),
                    end,
                    self.ids.as_deref(),
                )
                .await;
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

/// The serving instances a request was admitted on (`None` from an authority
/// with no instance view), handed to [`Plan::new`] so the store is not read
/// twice for one request.
pub struct InstanceView(pub(crate) Option<Vec<ServingInstance>>);

/// The offers one request may make, in order.
pub struct Plan {
    deps: RouterDeps,
    deployment: String,
    route: Route,
    last: Option<Refusal>,
    /// SPEC §10 (owner decision 2026-10-19): the request's IDs.
    ids: Arc<RequestIds>,
    /// Role-log lines written before CapyCTL's request ID was decided, held
    /// until it is (at most one selection and [`MAX_ATTEMPTS`] failovers).
    pending: Vec<serde_json::Value>,
    /// SPEC §10: the request's queue deadline (activation included), which
    /// also bounds waiting for a restart (ADR 0032 §7).
    deadline: tokio::time::Instant,
}

/// Why an offer produced no attempt.
enum Unoffered {
    /// The refusal that ends the request.
    Refused(Refusal),
    /// ADR 0032 §7: the deployment's durable lease bound refused the grant
    /// as full; the request asks for a restart before it is refused.
    Full,
}

impl From<Refusal> for Unoffered {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// ADR 0032 §7: the pause before asking again for a restart that left the
/// bound full, so a request waiting through one does not spin on the ledger.
/// It paces the retries only; a timer never triggers a restart.
const RESTART_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

impl Plan {
    /// Rank the deployment's serving instances: `view` when admission read
    /// them for this request, otherwise a fresh read. Logs the selection with
    /// its inputs and the request's `ids`. `deadline` is the request's queue
    /// deadline (SPEC §10).
    pub fn new(
        deps: &RouterDeps,
        deployment: &str,
        view: Option<InstanceView>,
        ids: Arc<RequestIds>,
        deadline: tokio::time::Instant,
    ) -> Result<Self, Refusal> {
        let mut plan = Self {
            deps: deps.clone(),
            deployment: deployment.to_owned(),
            route: Route::Whole { offered: false },
            last: None,
            ids,
            pending: Vec::new(),
            deadline,
        };
        plan.set_route(view)?;
        Ok(plan)
    }

    /// Set the route from `view`, or from a fresh read of the deployment's
    /// serving instances, and log the selection.
    fn set_route(&mut self, view: Option<InstanceView>) -> Result<(), Refusal> {
        let deployment = self.deployment.clone();
        let instances = match view {
            Some(InstanceView(instances)) => instances,
            None => self
                .deps
                .controller
                .serving_instances(&deployment)
                .map_err(crate::chat::map_controller)?,
        };
        let Some(instances) = instances else {
            self.route = Route::Whole { offered: false };
            self.last = None;
            return Ok(());
        };
        let ranking = self.deps.inflight.instances.choose(&deployment, &instances);
        let selection = serde_json::json!({
            "event": "router_selection",
            "deployment": deployment,
            "chosen": ranking.order.first().map(|c| c.generation),
            "candidates": ranking.order.iter().map(Scored::log).collect::<Vec<_>>(),
            "skipped": ranking.skipped.iter().map(|s| serde_json::json!({
                "instance": s.instance_index,
                "generation": s.generation,
                "reason": s.reason,
            })).collect::<Vec<_>>(),
        });
        self.last = if ranking.skipped.is_empty() {
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
        self.route = Route::Instances {
            order: ranking.order,
            next: 0,
            first_counted: true,
        };
        self.log(selection);
        Ok(())
    }

    /// Write one role-log line with the request's IDs, or hold it until
    /// CapyCTL's is decided.
    fn log(&mut self, event: serde_json::Value) {
        log_for(&self.ids, &mut self.pending, event);
    }

    /// The next offer, or the refusal that ends the request. Every refusal here
    /// is decided before anything reaches an engine. Once it returns, the
    /// request's CapyCTL ID is decided: the first granted lease's, or a fresh
    /// one when none was granted (SPEC §10, owner decision 2026-10-19).
    ///
    /// ADR 0032 §7: a grant refused because the deployment's durable bound is
    /// full asks the authority to restart the instances holding requests that
    /// settle only at their next stop. While one restarts, the request waits
    /// for it as for an activation, within its deadline, and is offered again
    /// on a fresh read of the instances; otherwise it is answered `queue_full`
    /// as before.
    pub async fn next(&mut self) -> Result<Attempt, Refusal> {
        let mut restarts = 0u32;
        let next = loop {
            match self.offer().await {
                Ok(attempt) => break Ok(attempt),
                Err(Unoffered::Refused(refusal)) => break Err(refusal),
                Err(Unoffered::Full) => {
                    if restarts > 0 {
                        let pause = tokio::time::Instant::now() + RESTART_RETRY_PAUSE;
                        tokio::time::sleep_until(pause.min(self.deadline)).await;
                    }
                    restarts += 1;
                    if let Err(refusal) =
                        crate::chat::restart_when_full(&self.deps, &self.deployment, self.deadline)
                            .await
                    {
                        break Err(refusal);
                    }
                    if let Err(refusal) = self.set_route(None) {
                        break Err(refusal);
                    }
                }
            }
        };
        if let Ok(Attempt {
            lease: Some((_, lease)),
            ..
        }) = &next
        {
            self.ids.granted(lease.id());
        }
        // Decided either way from here on: a lease names the request, else a
        // fresh ID does.
        let _ = self.ids.capyctl();
        for event in std::mem::take(&mut self.pending) {
            self.log(event);
        }
        next
    }

    async fn offer(&mut self) -> Result<Attempt, Unoffered> {
        let deps = self.deps.clone();
        let deployment = self.deployment.clone();
        match &mut self.route {
            Route::Whole { offered } => {
                if std::mem::replace(offered, true) {
                    return Err(self
                        .last
                        .take()
                        .unwrap_or_else(|| {
                            crate::chat::refusal("unavailable", "no further engine to offer")
                        })
                        .into());
                }
                let forward = deps
                    .forwards
                    .forwarder(&deployment)
                    .map_err(crate::chat::map_forwarder)?;
                let lease = match deps
                    .controller
                    .open_request_lease(&deployment, deps.limits.max_requests_per_deployment)
                    .await
                {
                    Ok(lease) => lease,
                    // ADR 0032 §7: the caller asks for a restart first, then
                    // routes the request afresh.
                    Err(LeaseRefused::Full) => return Err(Unoffered::Full),
                    Err(refused) => {
                        return Err(crate::chat::lease_refusal(&deployment, refused).into())
                    }
                };
                Ok(Attempt {
                    forward,
                    lease: lease.map(|lease| (deps.controller.clone(), lease)),
                    generation: None,
                    instance: None,
                    stall: None,
                    ids: Some(self.ids.clone()),
                    abort: None,
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
                            log_for(
                                &self.ids,
                                &mut self.pending,
                                failover(&deployment, &candidate, "lease_refused_gate_closed"),
                            );
                            self.last = Some(crate::chat::refusal(
                                "unavailable",
                                &format!(
                                    "dispatch to deployment {deployment} is closed; retry shortly"
                                ),
                            ));
                            continue;
                        }
                        // ADR 0032 §7: the caller asks for a restart before it
                        // answers `queue_full`.
                        Err(LeaseRefused::Full) => return Err(Unoffered::Full),
                        Err(LeaseRefused::Unavailable(reason)) => {
                            return Err(crate::chat::refusal("unavailable", &reason).into())
                        }
                    };
                    // ADR 0013 §10: resolve exactly the incarnation the lease
                    // names. If it moved on or its gate closed since, nothing was
                    // sent: close the lease as not accepted and offer the next.
                    match deps.forwards.forwarder_for(
                        &deployment,
                        candidate.generation,
                        candidate.remote,
                    ) {
                        Ok(lane) => {
                            // ADR 0028 §11 (decided 2026-10-06): a request to a
                            // group's head is watched for its first token; ADR
                            // 0035 §7: so is one to an instance on several GPUs
                            // of one host (the same stall timeout). A
                            // single-GPU instance's never is.
                            let watched = candidate.group || candidate.local_tensor_parallel > 1;
                            let stall = watched.then(|| crate::stream::StallWatch {
                                authority: deps.controller.clone(),
                                deployment: deployment.clone(),
                                instance: candidate.instance_index,
                                generation: candidate.generation,
                                after: deps.controller.group_stall_timeout(),
                            });
                            // ADR 0032 §1–§3: a capable lane carries the
                            // request ID and is aborted by it after a cut.
                            let abort = lane.request_abort.then(|| AbortLane {
                                deployment: deployment.clone(),
                                generation: candidate.generation,
                            });
                            return Ok(Attempt {
                                forward: lane.forward,
                                lease: lease.map(|lease| (deps.controller.clone(), lease)),
                                generation: Some(candidate.generation),
                                instance: Some(candidate.instance_index),
                                stall,
                                ids: Some(self.ids.clone()),
                                abort,
                                _slot: Some(slot),
                            });
                        }
                        Err(error) => {
                            crate::chat::close_lease(
                                deps.controller.as_ref(),
                                lease,
                                LeaseEnd::NotAccepted,
                                Some(&self.ids),
                            )
                            .await;
                            log_for(
                                &self.ids,
                                &mut self.pending,
                                failover(&deployment, &candidate, "forwarder_unavailable"),
                            );
                            self.last = Some(crate::chat::map_forwarder(error));
                        }
                    }
                }
                Err(self
                    .last
                    .take()
                    .unwrap_or_else(|| {
                        crate::chat::map_forwarder(crate::forwarders::ForwarderError::NoRuntime(
                            deployment.clone(),
                        ))
                    })
                    .into())
            }
        }
    }

    /// Record that the last offer was refused before forwarding, so the final
    /// answer, if no other offer succeeds, says so.
    pub fn refused(&mut self, generation: Option<i64>, reason: &str) {
        if let Some(generation) = generation {
            self.log(serde_json::json!({
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

fn failover(deployment: &str, candidate: &Scored, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "event": "router_failover",
        "deployment": deployment,
        "generation": candidate.generation,
        "instance": candidate.instance_index,
        "reason": reason,
    })
}

/// See [`Plan::log`]: by field, so it can be called while the route is borrowed.
fn log_for(ids: &RequestIds, pending: &mut Vec<serde_json::Value>, mut event: serde_json::Value) {
    if ids.settled() {
        ids.log_fields(&mut event);
        capyctl_domain::role_log::event(event);
    } else {
        pending.push(event);
    }
}

#[cfg(test)]
mod tests;
