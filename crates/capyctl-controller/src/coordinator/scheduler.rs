//! ADR 0015: the coordinator drives each instance's lifecycle independently.
//!
//! Live M16 (2026-09-23) found the worker running one loop for every host and
//! deployment and waiting inline on each activation until Ready or its deadline,
//! so a slow or dead load on one host held every Stop and Start everywhere for up
//! to fifteen minutes. The owner decided the same day that lifecycle work is
//! concurrent per deployment: waiting on one deployment's load never blocks
//! another deployment's start or stop, while admission, reservations, placement
//! and the ledger stay serialized through the store's IMMEDIATE transactions.
//!
//! The scheduler here is the only discoverer. Each pass it does the store-only
//! bookkeeping inline (unarmed stops, instance reconciliation, drain re-issue,
//! expiry, the residency policy) and hands every effect — an Initialize, a
//! cleanup, a park or restore, a paused launch's settlement — to its own task on
//! the instance's lane. Invariants (ADR 0015):
//!
//! 1. One effect per instance. An instance with a task in flight is excluded
//!    from discovery (`BusyLanes`), so its steps are never armed, completed,
//!    expired or superseded by the scheduler behind the task's back; the task
//!    settles its own step exactly as the single loop did.
//! 2. The step state machine is unchanged. A task runs the same `drive`,
//!    `drive_cleanup` and residency code and classifies failures with the same
//!    rules; only waiting moved off the discovery path.
//! 3. Exact accounting stays in the store. Every arm, reservation, placement,
//!    release and epoch advance is one IMMEDIATE transaction under the owner
//!    mutex; concurrent tasks only interleave between transactions.
//! 4. Cleanups never wait behind activations: they have their own slots, and a
//!    retry cooldown or deferral is a hold on that one start, not a sleep.
//! 5. An uncertain launch still pauses new activations (all of them, as
//!    before) until that exact binding is settled on evidence; cleanups
//!    continue meanwhile.
//! 6. No effect future outlives the worker: shutdown, a halt or closed
//!    admission cancels in-flight tasks through their stop signal (which
//!    records uncertainty exactly as before) and joins every one of them before
//!    `run` returns.
use super::*;
use capyctl_store::ordinary_lifecycle::lanes::BusyLanes;
use capyctl_store::ordinary_lifecycle::startup::StartupGate;
use std::collections::BTreeSet;
use tokio::task::JoinSet;

/// The instance an effect runs for: `(deployment_id, instance_index)`.
pub(super) type Lane = (String, u32);

/// What a finished task asks the scheduler to do.
pub(super) enum Outcome {
    /// The work progressed or ended; nothing is held.
    Done,
    /// Hold this binding's planned Initialize back until `at` (a retry
    /// cooldown or a deferral). The step stays planned and stoppable.
    Hold {
        binding: String,
        at: tokio::time::Instant,
    },
    /// The launch is uncertain and the task already paused new activations
    /// on its binding (review finding: the pause is taken where the
    /// uncertainty is classified, never later when the scheduler gets to it).
    Paused,
    /// A cleanup could not be classified (its host dropped mid-Stop, or its
    /// runtime is not retained here). Its binding stays uncertain and the task
    /// already paused new activations on it; only this instance's cleanup
    /// discovery backs off. ADR 0011 decision 4: the failure is scoped, not
    /// worker-wide. `step` is the cleanup step, re-planned in this session
    /// after the backoff when it had been armed (SPEC §13.2).
    CleanupPaused { step: String },
    /// This binding's runtime was stopped or settled on evidence.
    Settled(String),
    /// A paused remote launch was asked again and is still unproven.
    SettleLater(String),
    /// A worker-wide condition: stop dispatching and return this status.
    Halt(WorkerStatus),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Activation,
    Cleanup,
    Settlement,
}

struct Finished {
    lane: Lane,
    binding: String,
    kind: Kind,
    outcome: Outcome,
}

/// One in-flight Initialize.
struct Launching {
    step: String,
    /// Set when another task ran on the same host during its startup; its
    /// startup peak is then not measured.
    contended: Arc<AtomicBool>,
}

struct Scheduler {
    shared: Arc<Shared>,
    observations: Arc<dyn ServiceObservation>,
    factory: DriverFactory,
    tasks: JoinSet<Finished>,
    /// Instances with a task in flight.
    busy: BTreeSet<Lane>,
    /// Bindings with a task in flight; excluded from Initialize discovery too.
    inflight: BTreeSet<String>,
    /// In-flight Initializes by lane (owner decision 2026-09-23: the
    /// per-host activation gate charges their startup peaks).
    launching: BTreeMap<Lane, Launching>,
    /// The host of every in-flight task whose host is known, by lane. An
    /// Initialize that shares its host with another task during its startup
    /// cannot attribute an availability drop to itself.
    hosts: BTreeMap<Lane, String>,
    /// Starts held back, by binding, until the instant named.
    held: BTreeMap<String, tokio::time::Instant>,
    /// Instances whose last cleanup could not be classified, excluded from
    /// cleanup discovery until the instant named so a cleanup that never
    /// armed is not re-driven on every pass.
    cleanup_held: BTreeMap<Lane, tokio::time::Instant>,
    /// ADR 0015 follow-up (SPEC §13.2): the unproven cleanup step of each held
    /// instance. When its backoff ends the step is re-planned in this session
    /// (if it had been armed), so the ordinary path re-arms it once its host is
    /// reachable instead of waiting for a restarted coordinator to adopt it.
    cleanup_retry: BTreeMap<Lane, String>,
    cleanups_in_flight: usize,
    activation_slots: Arc<Semaphore>,
    cleanup_slots: Arc<Semaphore>,
    /// The stop signal every task watches.
    cancel: watch::Sender<bool>,
    halt: Option<WorkerStatus>,
    /// SPEC §10 (amended 2026-10-01): quiescence questions in flight, off the
    /// loop, by task; at most one per binding.
    quiescence: JoinSet<QuiescenceAnswer>,
    asking: BTreeMap<tokio::task::Id, String>,
    /// Answers not yet applied.
    answers: Vec<QuiescenceAnswer>,
    /// Bindings whose engine left a question unanswered: when each may be
    /// asked again, and how many in a row went unanswered.
    quiescence_held: BTreeMap<String, (tokio::time::Instant, u32)>,
}

pub(super) async fn run(
    shared: Arc<Shared>,
    observations: Arc<dyn ServiceObservation>,
    factory: DriverFactory,
    mut stop: watch::Receiver<bool>,
    status_tx: &watch::Sender<WorkerStatus>,
) -> WorkerStatus {
    // SPEC §13.2: on server restart, reconcile before dispatch. A remote worker
    // first adopts the remote launches its retired session left behind.
    if shared.adopt_remote {
        match native_failure::adopt_retired_remote_launches(&shared, &factory).await {
            Ok(adopted) if !adopted.is_empty() => {
                for launch in adopted {
                    shared.pause(launch);
                }
                publish(&shared, status_tx);
                shared.changed.notify_waiters();
            }
            Ok(_) => {}
            Err(error) => return WorkerStatus::Failed(error.to_string()),
        }
    }
    // SPEC §4.3 (P3): a restarted embedded role re-attaches the Ready engines it
    // left running before it admits anything new.
    if let Some(adoption) = shared.local_adoption.clone() {
        if let Err(error) = local_adoption::adopt_retired_local_launches(&shared, &adoption).await {
            return WorkerStatus::Failed(error.to_string());
        }
    }
    // W12, SPEC §13.2: resume the Stops a retired session accepted but never
    // finished, so their engines can still be stopped and released.
    let remote_cleanups = shared.adopt_remote.then(|| factory.clone());
    let local_cleanups = shared.local_adoption.clone();
    if remote_cleanups.is_some() || local_cleanups.is_some() {
        if let Err(error) = cleanup_adoption::adopt_retired_cleanups(
            &shared,
            remote_cleanups.as_ref(),
            local_cleanups.as_ref(),
        )
        .await
        {
            return WorkerStatus::Failed(error.to_string());
        }
    }
    let slots = shared.options.max_concurrent_effects;
    let (cancel, _) = watch::channel(false);
    let mut scheduler = Scheduler {
        shared: shared.clone(),
        observations,
        factory,
        tasks: JoinSet::new(),
        busy: BTreeSet::new(),
        inflight: BTreeSet::new(),
        launching: BTreeMap::new(),
        hosts: BTreeMap::new(),
        held: BTreeMap::new(),
        cleanup_held: BTreeMap::new(),
        cleanup_retry: BTreeMap::new(),
        cleanups_in_flight: 0,
        activation_slots: Arc::new(Semaphore::new(slots)),
        cleanup_slots: Arc::new(Semaphore::new(slots)),
        cancel,
        halt: None,
        quiescence: JoinSet::new(),
        asking: BTreeMap::new(),
        answers: Vec::new(),
        quiescence_held: BTreeMap::new(),
    };
    let exit = loop {
        while let Some(joined) = scheduler.tasks.try_join_next() {
            scheduler.apply(joined, status_tx);
        }
        if let Some(status) = scheduler.halt.take() {
            break Exit::Halted(status);
        }
        if *stop.borrow() {
            break Exit::Other(shared.paused_status().unwrap_or(WorkerStatus::Stopped));
        }
        if !shared.accepting.load(Ordering::Acquire) {
            break Exit::Other(WorkerStatus::Failed(
                "service stopped accepting work".into(),
            ));
        }
        if let Err(status) = scheduler.pass(status_tx).await {
            scheduler.halt.get_or_insert(status);
            continue;
        }
        scheduler.wait(&mut stop, status_tx).await;
    };
    scheduler.finish(exit).await
}

enum Exit {
    /// A task reported a worker-wide condition; its status is the answer.
    Halted(WorkerStatus),
    /// Shutdown or closed admission; an in-flight task's own outcome, when it
    /// reports one, is the more precise answer (as when the single loop
    /// returned the outcome of the effect it was driving).
    Other(WorkerStatus),
}

impl Scheduler {
    /// The exclusions for one discovery. Held and in-flight bindings apply to
    /// Initialize discovery only.
    fn busy_lanes(&mut self, initialize: bool) -> BusyLanes {
        let mut held = BTreeSet::new();
        if initialize {
            let now = tokio::time::Instant::now();
            self.held.retain(|_, at| *at > now);
            held.extend(self.held.keys().cloned());
            held.extend(self.inflight.iter().cloned());
        }
        BusyLanes {
            instances: self.busy.clone(),
            held,
        }
    }

    fn spawn(
        &mut self,
        lane: Lane,
        binding: String,
        kind: Kind,
        permit: Option<OwnedSemaphorePermit>,
        effect: impl Future<Output = Outcome> + Send + 'static,
    ) {
        self.busy.insert(lane.clone());
        self.inflight.insert(binding.clone());
        if kind == Kind::Cleanup {
            self.cleanups_in_flight += 1;
        }
        self.tasks.spawn(async move {
            let outcome = effect.await;
            drop(permit);
            Finished {
                lane,
                binding,
                kind,
                outcome,
            }
        });
    }

    /// SPEC §10 (amended 2026-10-01): apply the answers that arrived, then ask
    /// each binding with a cancelling lease that has no question in flight and
    /// is not held back. Nothing here waits for an engine; an unanswered
    /// question leaves its leases charged. Returns whether any lease closed.
    async fn settle_cancellations(&mut self) -> Result<bool, CoordinatorError> {
        while let Some(joined) = self.quiescence.try_join_next_with_id() {
            self.answered(joined);
        }
        let mut changed = false;
        for answer in std::mem::take(&mut self.answers) {
            changed |= settle_quiescent(&self.shared, answer).await?;
        }
        let now = tokio::time::Instant::now();
        for (cancelling, driver) in cancelling_drivers(&self.shared).await? {
            let binding = cancelling.binding_id.clone();
            let held = self
                .quiescence_held
                .get(&binding)
                .is_some_and(|(at, _)| *at > now);
            if held || self.asking.values().any(|b| *b == binding) {
                continue;
            }
            let id = self
                .quiescence
                .spawn(ask_quiescence(cancelling, driver))
                .id();
            self.asking.insert(id, binding);
        }
        Ok(changed)
    }

    fn answered(
        &mut self,
        joined: Result<(tokio::task::Id, QuiescenceAnswer), tokio::task::JoinError>,
    ) {
        let (id, answer) = match joined {
            Ok((id, answer)) => (id, Some(answer)),
            Err(error) => (error.id(), None),
        };
        let Some(binding) = self.asking.remove(&id) else {
            return;
        };
        match answer {
            Some(answer) if answer.quiescent.is_some() => {
                self.quiescence_held.remove(&binding);
                if answer.quiescent == Some(true) {
                    self.answers.push(answer);
                }
            }
            // Unanswered (timed out or panicked): ask again later.
            _ => {
                let timeouts = self
                    .quiescence_held
                    .get(&binding)
                    .map_or(1, |(_, n)| n.saturating_add(1));
                let at = tokio::time::Instant::now() + quiescence_backoff(timeouts);
                self.quiescence_held.insert(binding, (at, timeouts));
            }
        }
    }

    /// Owner decision 2026-09-23: record that `lane` now runs a task on
    /// `host`. Every Initialize already starting there, and the new task if
    /// it is one, is contended: none of them can attribute a change in the
    /// host's available memory to itself alone.
    fn occupy(&mut self, lane: &Lane, host: String) {
        let shared: Vec<Lane> = self
            .hosts
            .iter()
            .filter(|(other, on)| *other != lane && **on == host)
            .map(|(other, _)| other.clone())
            .collect();
        if !shared.is_empty() {
            for other in shared.iter().chain(std::iter::once(lane)) {
                if let Some(launch) = self.launching.get(other) {
                    launch.contended.store(true, Ordering::Release);
                }
            }
        }
        self.hosts.insert(lane.clone(), host);
    }

    fn apply(
        &mut self,
        joined: Result<Finished, tokio::task::JoinError>,
        status_tx: &watch::Sender<WorkerStatus>,
    ) {
        let finished = match joined {
            Ok(finished) => finished,
            // Every task body catches its own panics; this is the backstop.
            Err(_) => {
                self.halt.get_or_insert(WorkerStatus::Failed(
                    "worker step panicked; durable arm retained".into(),
                ));
                return;
            }
        };
        self.busy.remove(&finished.lane);
        self.launching.remove(&finished.lane);
        self.hosts.remove(&finished.lane);
        self.inflight.remove(&finished.binding);
        if finished.kind == Kind::Cleanup {
            self.cleanups_in_flight = self.cleanups_in_flight.saturating_sub(1);
        }
        match finished.outcome {
            Outcome::Done => {}
            Outcome::Hold { binding, at } => {
                self.held.insert(binding, at);
            }
            Outcome::Paused => publish(&self.shared, status_tx),
            Outcome::CleanupPaused { step } => {
                self.cleanup_held.insert(
                    finished.lane.clone(),
                    tokio::time::Instant::now() + self.shared.options.retry_cooldown,
                );
                self.cleanup_retry.insert(finished.lane.clone(), step);
                publish(&self.shared, status_tx);
            }
            // A settlement lifts its own pause inside its release transaction,
            // so the status is republished whether or not one remained here.
            Outcome::Settled(binding) => {
                self.shared.unpause(&binding);
                publish(&self.shared, status_tx);
            }
            Outcome::SettleLater(binding) => self.shared.settle_later(&binding),
            Outcome::Halt(status) => {
                self.halt.get_or_insert(status);
            }
        }
        self.shared.changed.notify_waiters();
    }

    /// One discovery pass. Store-only work runs inline; every effect is handed
    /// to a task. An error is a worker-wide condition.
    async fn pass(&mut self, status_tx: &watch::Sender<WorkerStatus>) -> Result<(), WorkerStatus> {
        let shared = self.shared.clone();
        let failed = |error: CoordinatorError| WorkerStatus::Failed(error.to_string());
        // A Stop of a start that never armed completes once its predecessor's
        // task, if any, has exited (its instance is excluded until then).
        loop {
            let busy = self.busy_lanes(false);
            let unarmed = shared
                .read(move |owner, _| {
                    let Some(work) = owner
                        .store()
                        .next_unarmed_stop_among(owner.session(), &busy)?
                    else {
                        return Ok(None);
                    };
                    owner
                        .store()
                        .complete_unarmed_stop(owner.session(), &work.step_id)?;
                    Ok(Some(work))
                })
                .await
                .map_err(failed)?;
            let Some(work) = unarmed else { break };
            self.held.remove(&work.binding_id);
            shared.forget_deferral(&work.binding_id);
            if shared.unpause(&work.binding_id) {
                publish(&shared, status_tx);
            }
            shared.changed.notify_waiters();
        }
        // SPEC §6.3 (live M47): an operator's Stop accepted while a launch was
        // in flight without an association is carried out once that launch has
        // settled, as an ordinary Stop.
        let resolved = shared
            .read(|owner, now| owner.store().resolve_deferred_stops(owner.session(), now))
            .await
            .map_err(failed)?;
        if !resolved.is_empty() {
            shared.changed.notify_waiters();
        }
        // ADR 0013 §7, owner decision Q8: stops a revision or a count decrease
        // asks for, retirements, and starts waiting for a host, all durable.
        let eligible = shared.observations.eligible_hosts();
        let starts = shared.paused_is_empty() && shared.initializing.load(Ordering::Acquire);
        let done = shared
            .read(move |owner, now| {
                owner
                    .store()
                    .reconcile_instances(owner.session(), now, eligible.as_ref(), starts)
            })
            .await
            .map_err(failed)?;
        if !done.is_empty() {
            journal_reconciled(&shared, &done).await;
            shared.changed.notify_waiters();
        }
        // ADR 0028 §11: under `recovery: reconcile` a group that failed after
        // READY relaunches as a new generation and plan, accepted only in a
        // transaction that reads every member of its old plan settled, and
        // only while every member host is eligible.
        if starts && shared.groups.is_some() {
            let eligible = shared.observations.eligible_hosts();
            let relaunched = shared
                .read(move |owner, now| {
                    owner
                        .store()
                        .relaunch_failed_groups(owner.session(), now, eligible.as_ref())
                })
                .await
                .map_err(failed)?;
            if !relaunched.is_empty() {
                shared.changed.notify_waiters();
            }
        }
        // Owner decision 2026-09-22: a drain Stop that expired, never armed,
        // while its host was offline is closed as expired and issued afresh
        // once the host is back. ADR 0015: that closes a planned cleanup, so it
        // waits until no cleanup task could be about to arm one.
        if self.cleanups_in_flight == 0 {
            if let Some(online) = shared.observations.online_hosts() {
                let reissued = shared
                    .read(move |owner, now| {
                        owner
                            .store()
                            .reissue_expired_drain_stops(owner.session(), &online, now)
                    })
                    .await
                    .map_err(failed)?;
                if !reissued.is_empty() {
                    shared.changed.notify_waiters();
                }
            }
        }
        // Owner decision 4: a cleanup whose remote host is offline waits,
        // unarmed, for the host to reconnect; it is never sent into a timeout.
        // ADR 0015: cleanups have their own slots and never queue behind loads.
        let now = tokio::time::Instant::now();
        self.cleanup_held.retain(|_, at| *at > now);
        // ADR 0015 follow-up (SPEC §13.2, ADR 0011 decision 4): an unproven
        // cleanup whose backoff ended is re-planned in this session. The store
        // keeps every charge and the recorded identities; the ordinary path
        // below re-arms it once its host is online (owner decision 4) and
        // completes it only on gone evidence. Past its deadline, or no longer
        // this session's armed step, it is left as it is: uncertain and charged.
        let due: Vec<(Lane, String)> = self
            .cleanup_retry
            .iter()
            .filter(|(lane, _)| !self.cleanup_held.contains_key(*lane))
            .map(|(lane, step)| (lane.clone(), step.clone()))
            .collect();
        for (lane, step) in due {
            self.cleanup_retry.remove(&lane);
            let replanned = shared
                .read(move |owner, now| {
                    match owner
                        .store()
                        .replan_unproven_cleanup(owner.session(), &step, now)
                    {
                        Ok(replanned) => Ok(replanned),
                        Err(LifecycleError::Rejected(_) | LifecycleError::Conflict) => Ok(false),
                        Err(error) => Err(error),
                    }
                })
                .await
                .map_err(failed)?;
            if replanned {
                shared.changed.notify_waiters();
            }
        }
        while let Ok(permit) = self.cleanup_slots.clone().try_acquire_owned() {
            let online = shared.observations.online_hosts();
            let mut busy = self.busy_lanes(false);
            busy.instances.extend(self.cleanup_held.keys().cloned());
            let found = shared
                .read(move |owner, now| {
                    owner.store().next_ordinary_cleanup_among(
                        owner.session(),
                        online.as_ref(),
                        now,
                        &busy,
                    )
                })
                .await
                .map_err(failed)?;
            let Some((cleanup, lane)) = found else { break };
            // A retained binding always names its instance; without one the
            // cleanup could not be kept off a second task.
            let Some(lane) = lane else {
                return Err(WorkerStatus::Failed(
                    "a planned cleanup names no instance".into(),
                ));
            };
            let task = cleanup_task(shared.clone(), cleanup.clone(), self.cancel.subscribe());
            let placed = {
                let (deployment, instance) = lane.clone();
                shared
                    .with_owner(move |owner| {
                        owner
                            .store()
                            .instance_host(&deployment, instance)
                            .map_err(|error| CoordinatorError::Service(error.to_string()))
                    })
                    .await
            };
            if let Ok(Some(host)) = placed {
                self.occupy(&lane, host);
            }
            self.spawn(lane, cleanup.binding_id, Kind::Cleanup, Some(permit), task);
        }
        // SPEC §§6.1, 13.2: keep asking the authenticated host about a paused
        // launch. Only its evidence settles the launch.
        for launch in shared.due_settlements() {
            if self.inflight.contains(&launch.binding_id) {
                continue;
            }
            let binding = launch.binding_id.clone();
            let lane = {
                let binding = binding.clone();
                shared
                    .read(move |owner, _| owner.store().binding_lane(&binding))
                    .await
                    .map_err(failed)?
            }
            .unwrap_or_else(|| (format!("binding:{binding}"), u32::MAX));
            if self.busy.contains(&lane) {
                continue;
            }
            let task = settlement_task(shared.clone(), launch, self.cancel.subscribe());
            self.spawn(lane, binding, Kind::Settlement, None, task);
        }
        // SPEC §10 (amended 2026-10-01): cancelled requests settle whether or
        // not new activations are paused.
        if self.settle_cancellations().await.map_err(failed)? {
            shared.changed.notify_waiters();
        }
        // An uncertain launch pauses every new activation until it is settled.
        if !shared.paused_is_empty() {
            return Ok(());
        }
        // SPEC §§6.3, 6.5, 9.1 (W5): planned parks and restores, one task each.
        // A read failure is left to the loop's own admission check.
        if let Ok(works) = shared
            .read(|owner, now| owner.store().next_residency_work(owner.session(), now))
            .await
        {
            for work in works {
                let lane = (work.deployment_id.clone(), work.instance);
                if self.busy.contains(&lane) || self.inflight.contains(&work.binding_id) {
                    continue;
                }
                let retained = shared
                    .retained
                    .lock()
                    .map_err(|_| failed(shared.fail("runtime registry poisoned")))?
                    .contains_key(&work.binding_id);
                if !retained {
                    continue;
                }
                let Ok(permit) = self.activation_slots.clone().try_acquire_owned() else {
                    break;
                };
                let binding = work.binding_id.clone();
                self.occupy(&lane, work.host.clone());
                let task = residency_task(
                    shared.clone(),
                    self.observations.clone(),
                    work,
                    self.cancel.subscribe(),
                );
                self.spawn(lane, binding, Kind::Activation, Some(permit), task);
            }
        }
        // SPEC §6.5 (W5): preinitialize and the idle policy only accept
        // ordinary work; nothing here sends anything.
        if residency_policy(&shared).await.map_err(failed)? {
            shared.changed.notify_waiters();
        }
        // Starts, oldest first, skipping busy instances and held starts, each
        // on its own task while activation slots remain.
        let mut waiting = BTreeSet::new();
        while let Ok(permit) = self.activation_slots.clone().try_acquire_owned() {
            let mut busy = self.busy_lanes(true);
            busy.held.extend(waiting.iter().cloned());
            let poll = shared
                .read(move |owner, now| {
                    owner
                        .store()
                        .next_initialize_or_expire_among(owner.session(), now, &busy)
                })
                .await
                .map_err(failed)?;
            match poll {
                InitializePoll::Work(work) => {
                    let lane = (work.fence().deployment_id.clone(), work.instance_index());
                    let binding = work.binding_id().to_owned();
                    // Owner decision 2026-09-23 (ADR 0015 implementation
                    // note): the per-host activation gate. A start whose
                    // startup peak does not fit beside the reservations and
                    // the peaks of the starts in flight on its host, but would
                    // once they reach Ready, waits planned for a later pass;
                    // it is not refused. A host agent keys its ingress per
                    // instance, so instances of one deployment are otherwise
                    // not serialized on a host.
                    let step = work.step_id().to_owned();
                    let in_flight: BTreeSet<String> =
                        self.launching.values().map(|l| l.step.clone()).collect();
                    // A gate that cannot decide lets the start proceed; its
                    // own task classifies whatever the store then says.
                    let gate = shared
                        .with_owner(move |owner| {
                            owner
                                .store()
                                .startup_gate(owner.session(), &step, &in_flight)
                                .map_err(|error| CoordinatorError::Service(error.to_string()))
                        })
                        .await;
                    if matches!(gate, Ok(StartupGate::Wait)) {
                        waiting.insert(binding);
                        continue;
                    }
                    let contended = Arc::new(AtomicBool::new(false));
                    self.launching.insert(
                        lane.clone(),
                        Launching {
                            step: work.step_id().to_owned(),
                            contended: contended.clone(),
                        },
                    );
                    self.occupy(&lane, work.effective().host.name.clone());
                    self.held.remove(&binding);
                    let task = initialize_task(
                        shared.clone(),
                        self.observations.clone(),
                        self.factory.clone(),
                        work,
                        self.cancel.subscribe(),
                        contended,
                    );
                    self.spawn(lane, binding, Kind::Activation, Some(permit), task);
                }
                InitializePoll::ExpiredUnarmed => shared.changed.notify_waiters(),
                InitializePoll::Idle => break,
            }
        }
        Ok(())
    }

    async fn wait(
        &mut self,
        stop: &mut watch::Receiver<bool>,
        status_tx: &watch::Sender<WorkerStatus>,
    ) {
        let shared = self.shared.clone();
        tokio::select! {
            _ = stop.changed() => {},
            _ = shared.wake.notified() => {},
            Some(joined) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                self.apply(joined, status_tx);
            }
            Some(joined) = self.quiescence.join_next_with_id(), if !self.quiescence.is_empty() => {
                self.answered(joined);
            }
            _ = tokio::time::sleep(shared.options.poll_interval) => {},
        }
    }

    /// ADR 0015 invariant 6: cancel and join every task before returning.
    async fn finish(mut self, exit: Exit) -> WorkerStatus {
        let mut reported = None;
        if !self.tasks.is_empty() {
            // A worker that is leaving leaves a remote launch uncertain for the
            // restarted worker to adopt and settle, as shutdown always has.
            self.shared
                .shutdown_requested
                .store(true, Ordering::Release);
            self.cancel.send_replace(true);
            while let Some(joined) = self.tasks.join_next().await {
                match joined {
                    Ok(Finished {
                        outcome: Outcome::Halt(status),
                        ..
                    }) => {
                        reported.get_or_insert(status);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        reported.get_or_insert(WorkerStatus::Failed(
                            "worker step panicked; durable arm retained".into(),
                        ));
                    }
                }
            }
        }
        match exit {
            Exit::Halted(status) => status,
            Exit::Other(status) => reported.unwrap_or(status),
        }
    }
}

/// The longest a deferred start is held between tries while its parked
/// reclaim is still in progress.
const DEFERRAL_HOLD_CAP: Duration = Duration::from_secs(2);

fn publish(shared: &Shared, status_tx: &watch::Sender<WorkerStatus>) {
    status_tx.send_replace(shared.paused_status().unwrap_or(WorkerStatus::Running));
}

impl Shared {
    /// Pause new activations on an uncertain launch. Admission reads the
    /// initializing flag under the owner mutex, so it changes under it.
    pub(super) fn pause(&self, paused: Paused) {
        let _owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        let mut map = self
            .paused
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        map.insert(paused.binding.clone(), paused);
        self.initializing.store(false, Ordering::Release);
    }

    /// [`Shared::pause`] from inside a transaction that already holds the
    /// owner mutex (an Initialize recorded uncertain).
    pub(super) fn pause_locked(&self, paused: Paused) {
        let mut map = self
            .paused
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        map.insert(paused.binding.clone(), paused);
        self.initializing.store(false, Ordering::Release);
    }

    /// SPEC §6.5 (W5), review finding: forget a start's deferral history once
    /// the start is no longer deferred.
    pub(super) fn forget_deferral(&self, binding: &str) {
        self.deferrals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(binding);
    }

    /// SPEC §6.5 (W5), review finding: record one more deferral of `binding`
    /// for `reason`. Returns whether the reason is new (journal it) and how
    /// long to hold the start: the poll interval, doubling while the same
    /// reason repeats, up to a bound.
    pub(super) fn defer(&self, binding: &str, reason: &str) -> (bool, Duration) {
        let mut map = self
            .deferrals
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.len() > 4096 {
            map.clear();
        }
        let entry = map
            .entry(binding.to_owned())
            .or_insert_with(|| (String::new(), 0));
        let new = entry.0 != reason;
        if new {
            *entry = (reason.to_owned(), 0);
        } else {
            entry.1 = entry.1.saturating_add(1);
        }
        let poll = self.options.poll_interval;
        let cap = poll.max(DEFERRAL_HOLD_CAP);
        let hold = poll
            .saturating_mul(1u32.checked_shl(entry.1.min(16)).unwrap_or(u32::MAX))
            .min(cap);
        (new, hold)
    }

    /// Lift the pause on this binding, if any. Returns whether it was paused.
    pub(super) fn unpause(&self, binding: &str) -> bool {
        let _owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        let mut map = self
            .paused
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = map.remove(binding).is_some();
        if removed {
            self.initializing.store(map.is_empty(), Ordering::Release);
        }
        removed
    }

    /// Lift a pause from inside a transaction that already holds the owner
    /// mutex (a settlement that released the launch on evidence).
    pub(super) fn unpause_locked(&self, binding: &str) {
        let mut map = self
            .paused
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        map.remove(binding);
        self.initializing.store(map.is_empty(), Ordering::Release);
    }

    pub(super) fn paused_is_empty(&self) -> bool {
        self.paused
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    }

    pub(super) fn paused_status(&self) -> Option<WorkerStatus> {
        self.paused
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .next()
            .map(|paused| paused.status.clone())
    }

    fn due_settlements(&self) -> Vec<native_failure::LaunchRef> {
        self.paused
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .filter_map(Paused::due)
            .collect()
    }

    fn settle_later(&self, binding: &str) {
        if let Some(paused) = self
            .paused
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(binding)
        {
            paused.retry_later(self.options.retry_cooldown);
        }
    }
}

async fn cleanup_task(
    shared: Arc<Shared>,
    cleanup: OrdinaryCleanupReceipt,
    mut stop: watch::Receiver<bool>,
) -> Outcome {
    let result = AssertUnwindSafe(drive_cleanup(&shared, &cleanup, &mut stop))
        .catch_unwind()
        .await;
    // `Stopped` from the cleanup now means only shutdown or closed admission
    // (a runtime this worker does not retain is a service error).
    let (reason, panicked, stopped) = match result {
        Ok(Ok(())) => return Outcome::Settled(cleanup.binding_id),
        Ok(Err(error)) => (
            error.to_string(),
            false,
            matches!(error, CoordinatorError::Stopped(_)),
        ),
        Err(_) => (
            "cleanup panicked; durable arm retained".to_owned(),
            true,
            false,
        ),
    };
    let shutdown = stopped || *stop.borrow();
    // Closed admission from a store fault, a poisoned mutex or a binding
    // mismatch the store refused is process-wide and still halts the worker.
    // The fault is read from its own flag: the scheduler leaves on the closed
    // admission and signals every task to stop, so the stop signal alone
    // would race it into reporting a mere shutdown.
    let faulted = shared.faulted.load(Ordering::Acquire) && !stopped;
    if faulted || (!shared.accepting.load(Ordering::Acquire) && !shutdown) {
        return Outcome::Halt(WorkerStatus::Failed(format!(
            "{reason}; cleanup authority retained"
        )));
    }
    let status = WorkerStatus::Uncertain {
        operation_id: cleanup.operation_id,
        reason,
    };
    // A panic is a worker-wide condition (the code, not a host, failed), and
    // on shutdown the worker is leaving anyway; the restarted worker adopts
    // the retained arm. Either still halts, reporting the retained arm.
    if panicked || shutdown {
        return Outcome::Halt(status);
    }
    // Review finding (ADR 0011 decision 4, SPEC §6.1): an unproven cleanup —
    // its host dropped mid-Stop, or its runtime is not retained here — keeps
    // its binding uncertain and its reservation charged, and pauses new
    // activations exactly as an uncertain Initialize does. It no longer halts
    // the worker, which cancelled every other instance's in-flight effect.
    shared.pause(Paused {
        binding: cleanup.binding_id,
        status,
        retry: None,
    });
    Outcome::CleanupPaused {
        step: cleanup.step_id,
    }
}

async fn settlement_task(
    shared: Arc<Shared>,
    launch: native_failure::LaunchRef,
    mut stop: watch::Receiver<bool>,
) -> Outcome {
    let binding = launch.binding_id.clone();
    let driver = match shared.retained.lock() {
        Ok(retained) => retained.get(&binding).cloned(),
        Err(_) => {
            return Outcome::Halt(WorkerStatus::Failed(
                shared.fail("runtime registry poisoned").to_string(),
            ))
        }
    };
    let settled = match driver {
        // ADR 0015 invariant 6 (review finding): a settlement waiting on a
        // host that does not answer no longer holds shutdown for its whole
        // protocol bound. Stopping leaves the launch uncertain with its
        // reservation charged, for the restarted worker to adopt and settle;
        // a store transaction already started still commits or rolls back on
        // its own blocking thread.
        Some(driver) => tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => {
                return Outcome::SettleLater(binding);
            }
            settled = AssertUnwindSafe(native_failure::settle_failed_launch(
                &shared,
                &driver,
                &launch,
                "settlement retried while paused",
                false,
            ))
            .catch_unwind() => settled.unwrap_or(Ok(InitializeStatus::Uncertain)),
        },
        // ADR 0028 §11: a group's launch is retried by stopping its members
        // again, each on its own host's evidence.
        None if launch.group => tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => {
                return Outcome::SettleLater(binding);
            }
            settled = AssertUnwindSafe(native_failure::settle_failed_group(
                &shared,
                &launch,
                "settlement retried while paused",
                false,
            ))
            .catch_unwind() => settled.unwrap_or(Ok(InitializeStatus::Uncertain)),
        },
        None => Ok(InitializeStatus::Uncertain),
    };
    if !shared.accepting.load(Ordering::Acquire) {
        return Outcome::Halt(WorkerStatus::Failed(
            "service stopped accepting work".into(),
        ));
    }
    match settled {
        Ok(InitializeStatus::Closed) => Outcome::Settled(binding),
        _ => Outcome::SettleLater(binding),
    }
}

async fn residency_task(
    shared: Arc<Shared>,
    observations: Arc<dyn ServiceObservation>,
    work: ResidencyWork,
    mut stop: watch::Receiver<bool>,
) -> Outcome {
    match AssertUnwindSafe(drive_residency(&shared, &*observations, work, &mut stop))
        .catch_unwind()
        .await
    {
        // Shutdown or a store failure: the loop's own checks decide.
        Ok(_) => Outcome::Done,
        Err(_) => Outcome::Halt(WorkerStatus::Failed(
            "residency step panicked; durable arm retained".into(),
        )),
    }
}

async fn initialize_task(
    shared: Arc<Shared>,
    observations: Arc<dyn ServiceObservation>,
    factory: DriverFactory,
    work: Box<InitializeWork>,
    mut stop: watch::Receiver<bool>,
    contended: Arc<AtomicBool>,
) -> Outcome {
    // Catch both service and adapter panics. If an arm exists, the fenced
    // annotation below preserves it; otherwise no effect has been allowed.
    // ADR 0028 §5: a start whose revision runs as a group activates its
    // members instead of one engine.
    let result = if work.group().is_some() {
        AssertUnwindSafe(super::drive_group(&shared, &work, &mut stop))
            .catch_unwind()
            .await
    } else {
        AssertUnwindSafe(drive(
            &shared,
            &work,
            &*observations,
            &factory,
            &mut stop,
            &contended,
        ))
        .catch_unwind()
        .await
    };
    match AssertUnwindSafe(conclude_initialize(&shared, &work, result, &stop))
        .catch_unwind()
        .await
    {
        Ok(outcome) => outcome,
        Err(_) => Outcome::Halt(WorkerStatus::Failed(
            "worker step panicked; durable arm retained".into(),
        )),
    }
}

/// Classify one Initialize's result, exactly as the single loop did.
async fn conclude_initialize(
    shared: &Arc<Shared>,
    work: &InitializeWork,
    result: Result<Result<(), CoordinatorError>, Box<dyn std::any::Any + Send>>,
    stop: &watch::Receiver<bool>,
) -> Outcome {
    let operation_id = work.operation_id().to_owned();
    let step_id = work.step_id().to_owned();
    let failure = match result {
        Ok(Ok(())) => {
            // ADR 0011 decision 5: a success resets the budget. This
            // configuration reached Ready, so the failures it took to get
            // there must not count against the next time it is started.
            let fence = work.fence().clone();
            if let Err(error) = shared
                .with_owner(move |owner| {
                    owner
                        .store()
                        .clear_attempts(&fence)
                        .map_err(|error| CoordinatorError::Service(error.to_string()))
                })
                .await
            {
                return Outcome::Halt(WorkerStatus::Failed(format!(
                    "reached Ready but the attempt budget was not reset: {error}"
                )));
            }
            return Outcome::Done;
        }
        // SPEC §6.5 (W5): deferred without any effect while parked instances
        // are reclaimed; the step stays planned and is not an attempt. The
        // scheduler drives their stops meanwhile and retries this start on a
        // later poll (ADR 0015: a hold, not a sleep).
        // Review finding: the deferral is journaled once per reason, not on
        // every retry, and the hold backs off while the reason repeats.
        Ok(Err(CoordinatorError::Deferred(reason))) => {
            let (new, hold) = shared.defer(work.binding_id(), &reason);
            if new {
                journal_failure(
                    shared,
                    &work.fence().deployment_id,
                    work.operation_id(),
                    "start_deferred",
                    &reason,
                )
                .await;
            }
            return Outcome::Hold {
                binding: work.binding_id().to_owned(),
                at: tokio::time::Instant::now() + hold,
            };
        }
        failure => failure,
    };
    shared.forget_deferral(work.binding_id());
    let reason = match failure {
        Ok(Err(error)) => error.to_string(),
        _ => "worker step panicked".into(),
    };
    // SPEC §17: failures are recorded. The match below moves `reason` into the
    // outcome, so keep a copy for the journal.
    let recorded_reason = reason.clone();
    let deployment_id = work.fence().deployment_id.clone();
    let journal_operation = work.operation_id().to_owned();
    let status_step = step_id.clone();
    // Spec §6: a builder that owns its processes can be classified by proof
    // instead of paused. Only a driver this worker retains for this exact
    // binding qualifies, and only one with process tools. SPEC §13.2: a remote
    // builder settles through its authenticated host instead, and qualifies
    // the same way.
    let native = shared
        .retained
        .lock()
        .ok()
        .and_then(|retained| retained.get(work.binding_id()).cloned())
        .filter(|driver| {
            // A shutting-down worker leaves a remote launch uncertain for the
            // restarted worker to adopt and settle, instead of holding
            // shutdown on a host that may not answer.
            driver.tools.is_some()
                || (driver.settle.is_some() && !shared.shutdown_requested.load(Ordering::Acquire))
        });
    // ADR 0028 §11: a group launch settles by stopping every member on its
    // own host, the way a remote launch settles through its host.
    let group = work.group().is_some()
        && shared.groups.is_some()
        && !shared.shutdown_requested.load(Ordering::Acquire);
    let is_native = native.is_some() || group;
    let pausing = shared.clone();
    let paused_binding = work.binding_id().to_owned();
    let paused_status = WorkerStatus::Uncertain {
        operation_id: operation_id.clone(),
        reason: reason.clone(),
    };
    let status = shared
        .read(move |owner, now| {
            // Keep observation and annotation under the exact owned lock. Stop
            // cannot transfer the claim between these transactions and strand
            // its frozen predecessor.
            let status = owner
                .store()
                .initialize_status(owner.session(), &status_step, now)?;
            if status == InitializeStatus::Expired {
                owner
                    .store()
                    .expire_unarmed_initialize(owner.session(), &status_step, now)?;
                Ok(InitializeStatus::ExpiredUnarmed)
            } else if status == InitializeStatus::Armed {
                if is_native {
                    // The annotation belongs to the settlement below, which
                    // decides between a proven release and the uncertain pause.
                    // Reporting it armed is what carries that decision out of
                    // this transaction.
                    Ok(InitializeStatus::Armed)
                } else {
                    owner
                        .store()
                        .mark_initialize_uncertain(owner.session(), &status_step, now)?;
                    // Review finding: pause new activations in the same
                    // owner-mutex critical section that records the launch
                    // uncertain, so no start command is admitted in between.
                    // Admission reads the pause under this lock.
                    pausing.pause_locked(Paused {
                        binding: paused_binding,
                        status: paused_status,
                        retry: None,
                    });
                    Ok(InitializeStatus::Uncertain)
                }
            } else {
                Ok(status)
            }
        })
        .await;
    let mut settled_native = false;
    let settles_remotely = group || native.as_ref().is_some_and(|d| d.settle.is_some());
    let status = match (status, native) {
        (Ok(InitializeStatus::Armed), Some(driver)) => {
            let settled = native_failure::settle_failed_launch(
                shared,
                &driver,
                &native_failure::LaunchRef::of(work),
                &recorded_reason,
                true,
            )
            .await;
            settled_native = matches!(settled, Ok(InitializeStatus::Closed));
            settled
        }
        (Ok(InitializeStatus::Armed), None) if group => {
            let settled = native_failure::settle_failed_group(
                shared,
                &native_failure::LaunchRef::of(work),
                &recorded_reason,
                true,
            )
            .await;
            settled_native = matches!(settled, Ok(InitializeStatus::Closed));
            settled
        }
        (status, _) => status,
    };
    // SPEC §13.2: only a step the store still reports as planned is a failure
    // known not to have landed. Nothing was armed, so nothing can be running,
    // and the same step is still there to be driven again. Every other outcome
    // is retained, not retried.
    let unlanded = matches!(status, Ok(InitializeStatus::Planned));
    let outcome = match status {
        Ok(InitializeStatus::ExpiredUnarmed) => return Outcome::Done,
        // Spec §6: the settlement terminated, proved, released, wrote the
        // redacted reason, closed this deployment's admission and re-admitted
        // every other one, all under a single lock. There is nothing left to
        // decide. Falling through to the give-up branch would journal the same
        // event a second time, unredacted and labelled as an exhausted retry
        // budget, when ADR 0011 decision 5 says a launch that failed after arm
        // is terminal for that start with no retry.
        Ok(InitializeStatus::Closed) if settled_native => return Outcome::Done,
        Ok(InitializeStatus::Uncertain) => WorkerStatus::Uncertain {
            operation_id,
            reason,
        },
        Ok(InitializeStatus::Superseded) => {
            // Stop can transfer the claim while Initialize is still running.
            // Its frozen predecessor must remain unchanged; validate the actual
            // successor instead of annotating through the now-stale fence.
            let predecessor = work.step_id().to_owned();
            match shared
                .read(move |owner, _| {
                    if let Some(receipt) = owner
                        .store()
                        .unarmed_stop_for_predecessor(owner.session(), &predecessor)?
                    {
                        return Ok(Some(receipt));
                    }
                    owner
                        .store()
                        .ordinary_cleanup_for_predecessor(owner.session(), &predecessor)
                        .map(|r| r.map(Into::into))
                })
                .await
            {
                Ok(Some(cleanup))
                    if cleanup.binding_id == work.binding_id()
                        && cleanup.incarnation == work.incarnation() =>
                {
                    WorkerStatus::Uncertain {
                        operation_id,
                        reason,
                    }
                }
                Ok(_) => WorkerStatus::Failed(format!("{reason}; superseded work retained")),
                Err(error) => {
                    WorkerStatus::Failed(format!("{reason}; cleanup validation failed: {error}"))
                }
            }
        }
        // ADR 0011 decision 4: a deployment that already closed its own
        // admission is blocked on that closure, not superseded by someone
        // else's work.
        Ok(InitializeStatus::Planned | InitializeStatus::Expired | InitializeStatus::Closed) => {
            WorkerStatus::Blocked {
                operation_id,
                reason,
            }
        }
        Ok(_) => WorkerStatus::Failed(format!("{reason}; superseded work retained")),
        Err(error) => {
            // SPEC §17: failures are recorded. This branch may leave an armed
            // step behind while the worker keeps running, so the reason must
            // survive even though the annotation did not.
            journal_failure(
                shared,
                &deployment_id,
                &journal_operation,
                "attempt_not_annotated",
                &redact_text(&format!(
                    "{recorded_reason}; durable arm retained if recorded: {error}"
                )),
            )
            .await;
            WorkerStatus::Failed(format!(
                "{reason}; durable arm retained if recorded: {error}"
            ))
        }
    };
    // Only a process-wide condition still halts the worker: closed global
    // admission (poisoned mutex, corrupt store — store_error already closed it
    // above) or requested shutdown.
    if !shared.accepting.load(Ordering::Acquire) || *stop.borrow() {
        return Outcome::Halt(outcome);
    }
    if matches!(outcome, WorkerStatus::Uncertain { .. }) {
        // Keep the same session and retained instance for explicit Stop. No
        // further Initialize is admitted or discovered until this exact
        // retained binding has a committed verified cleanup. Review finding:
        // the pause is taken here, as the outcome is classified, not when the
        // scheduler later applies this task's outcome.
        shared.pause(Paused {
            binding: work.binding_id().into(),
            status: outcome,
            // A remote launch keeps asking its host (SPEC §13.2).
            retry: settles_remotely.then(|| {
                (
                    native_failure::LaunchRef::of(work),
                    tokio::time::Instant::now() + shared.options.retry_cooldown,
                )
            }),
        });
        return Outcome::Paused;
    }
    // SPEC §13.2 and ADR 0011 decision 5: a failure known not to have landed is
    // retried. The attempt is counted against this exact configuration, and the
    // deployment is given up on once the budget is spent. An uncertain outcome
    // never reaches here: it pauses above and resolves through the gone-proof
    // first. SPEC §13.2 and spec §6: a builder's reason quotes the engine's own
    // output, and this journal is not the owner-only log, so every reason
    // written from here is redacted at the coordinator.
    let mut closing = redact_text(&format!("gave up: {recorded_reason}"));
    if unlanded {
        let fence = work.fence().clone();
        let counting = shared.clone();
        let attempt_operation = journal_operation.clone();
        // SPEC §17: failures are recorded. The attempt and the reason it failed
        // are written in the same transaction, so a counted attempt is never
        // left without an explanation.
        let attempt_evidence = redact_text(&format!(
            "deployment {deployment_id}: attempt failed: {recorded_reason}"
        ));
        let record = match shared
            .with_owner(move |owner| {
                let now = (counting.clock)()?;
                let record = owner
                    .store()
                    .record_attempt(&fence, now)
                    .map_err(|error| CoordinatorError::Service(error.to_string()))?;
                owner
                    .store()
                    .record_journal(
                        None,
                        Some(&attempt_operation),
                        Some("attempt_failed"),
                        &attempt_evidence,
                    )
                    .map_err(|error| CoordinatorError::Service(error.to_string()))?;
                Ok(record)
            })
            .await
        {
            Ok(record) => record,
            Err(error) => {
                return Outcome::Halt(WorkerStatus::Failed(format!(
                    "{outcome:?}; attempt not recorded: {error}"
                )))
            }
        };
        // Found live 2026-10-04: a refusal a retry cannot change gives up now.
        if !refused_for_good(&recorded_reason)
            && record.attempts < i64::from(shared.options.max_attempts)
        {
            // The step is still planned against its original reservation, so a
            // later poll rediscovers this exact work. Nothing is released, no
            // epoch advances and no dispatch is replayed.
            let exponent = u32::try_from(record.attempts.saturating_sub(1))
                .unwrap_or(u32::MAX)
                .min(16);
            let wait = shared
                .options
                .retry_cooldown
                .saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
            // ADR 0011 decision 5: retries happen within the start command's
            // deadline. Holding past it would only hand the step to the expiry
            // path mid-cooldown, so a deadline that arrives before the budget
            // is spent is terminal for this start and the deployment closes its
            // own admission now.
            let now = match (shared.clock)() {
                Ok(now) => now,
                Err(error) => {
                    return Outcome::Halt(WorkerStatus::Failed(format!(
                        "{outcome:?}; retry cooldown could not be bounded: {error}"
                    )))
                }
            };
            let left = work.deadline_ms().saturating_sub(now);
            let left = Duration::from_millis(u64::try_from(left).unwrap_or(0));
            if left > wait {
                // ADR 0015 (runbook limitation 4): the cooldown holds this one
                // start back; the worker keeps serving everything else.
                return Outcome::Hold {
                    binding: work.binding_id().to_owned(),
                    at: tokio::time::Instant::now() + wait.min(left),
                };
            }
            closing = redact_text(&format!(
                "deadline reached before the budget was spent: {recorded_reason}"
            ));
        }
    }
    // ADR 0011 decision 4: the budget is spent, the deadline arrived first, or
    // the failure was not one that may be replayed. A deployment that is given
    // up on closes its own admission, not the host's.
    let closed_deployment = deployment_id.clone();
    let closing_operation = journal_operation.clone();
    let closed_generation = work.fence().generation;
    let closed_instance = i64::from(work.instance_index());
    if let Err(error) = shared
        .with_owner(move |owner| {
            // ADR 0013 §6: one instance that gave up closes its own admission;
            // its siblings keep being admitted. Review finding: the closure is
            // keyed by (deployment, instance index), fenced by the generation
            // the plan carried, so it never reaches a sibling that shares a
            // generation; a stale incarnation closes nothing (T18).
            owner
                .store()
                .close_instance_admission_at(&closed_deployment, closed_instance, closed_generation)
                .map_err(|error| CoordinatorError::Service(error.to_string()))?;
            // SPEC §17: failures are recorded.
            owner
                .store()
                .record_journal(
                    None,
                    Some(&closing_operation),
                    Some("given_up"),
                    &format!("deployment {closed_deployment}: {closing}"),
                )
                .map_err(|error| CoordinatorError::Service(error.to_string()))
        })
        .await
    {
        return Outcome::Halt(WorkerStatus::Failed(format!(
            "{outcome:?}; failed to close the deployment's own admission: {error}"
        )));
    }
    Outcome::Done
}

/// Host refusals that can clear on their own (another start takes a free
/// port, memory another program held is freed, a download or a parked
/// instance's release completes, a host session is re-established): these
/// keep the retry budget (SPEC §13.2, ADR 0011 decision 5).
const TRANSIENT_REFUSALS: &[&str] = &[
    "port_conflict",
    "insufficient_memory",
    "insufficient_device_memory",
    "parked_capacity",
    "model_source_pending",
    "checkpoint_unverified",
    "unauthorized",
    // ADR 0028 §16: a group's rendezvous or service port taken outside
    // CapyCTL (excluded, then redrawn), or a head range with every port held
    // until another group settles. Every other group code is for good.
    "rendezvous_port_in_use:",
    "service_port_in_use:",
    "rendezvous_ports_exhausted",
];

/// Found live 2026-10-04: whether `reason` is a host refusal before any
/// effect that a retry of the same configuration cannot change (an SGLang
/// sizing refusal, a checkpoint mismatch, a missing capability). Such a
/// start gives up at once instead of waiting out its retry cooldowns.
fn refused_for_good(reason: &str) -> bool {
    let prefix = capyctl_adapters::traits::RuntimeError::Refused(String::new()).to_string();
    match reason.split_once(prefix.as_str()) {
        Some((_, category)) => !TRANSIENT_REFUSALS
            .iter()
            .any(|transient| category.starts_with(transient)),
        None => false,
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::refused_for_good;
    use capyctl_adapters::traits::RuntimeError;

    fn refused(code: &str) -> String {
        RuntimeError::Refused(code.into()).to_string()
    }

    // T30 (ADR 0028 §16): a port a Prepare found taken, or a head range with
    // no free port, clears on its own and keeps the retry budget; every other
    // group refusal a retry of the same configuration cannot change.
    #[test]
    fn group_refusals_are_classified() {
        for transient in [
            "rendezvous_port_in_use:25000",
            "service_port_in_use:8100",
            "rendezvous_ports_exhausted",
        ] {
            assert!(!refused_for_good(&refused(transient)), "{transient}");
        }
        for permanent in [
            "host_tuning_missing:memlock",
            "peer_address_not_local",
            "group_checkpoint_mismatch",
            "group_profile_mismatch",
            "group_model_path_mismatch",
            "group_topology_invalid",
            "group_shape_unsupported:tensorfold",
            "host_capability_missing:engine_groups",
            "group_drift:tensor_parallel",
        ] {
            assert!(refused_for_good(&refused(permanent)), "{permanent}");
        }
        // T39: the single-host classification is unchanged.
        assert!(!refused_for_good(&refused("port_conflict")));
        assert!(refused_for_good(&refused("capability_missing:deep_park")));
    }
}
