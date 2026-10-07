//! SPEC §10 steps 3–5, ADR 0013 §8 (owner decision D4, plan W10): automatic,
//! request-driven switching in the coordinator.
//!
//! The router never chooses a victim (SPEC §10: no second scheduler). It
//! queues a request for a deployment with no READY instance and joins that
//! deployment's one activation (steps 1–2); the lifecycle authority then asks
//! the [`Switcher`] to make room when the activation cannot be placed as the
//! ledger stands:
//!
//! 1. **Plan** on one host only: the store picks the host needing the least
//!    eviction and the minimum set of READY instances there, preferring
//!    instances whose deployment keeps serving elsewhere, then the least
//!    recently used. Instances serving a waiting group are never victims.
//! 2. **Turn.** Switches on one host run one at a time in arrival order
//!    (tokio's mutex is FIFO), so the oldest waiting group goes first. The
//!    plan is taken again under the turn, since an earlier group may have
//!    changed the host.
//! 3. **Fairness** (step 3): when a victim is its deployment's last READY
//!    instance and that deployment is busy, its admission stays open for the
//!    host's admission window, counted from the switch's start. The window
//!    never resets: sustained traffic to the victim cannot extend it (T19).
//! 4. **Drain** (step 4): each victim's dispatch gate closes; the switch waits
//!    for every request lease charged to it to close on evidence. A lease is
//!    closed only on completion or confirmed non-acceptance, so a quiet lease
//!    ledger is the router's and the ingress's drain; an uncertain lease keeps
//!    it undrained. A drain that does not finish within the bound **fails the
//!    switch** and the victims serve again (no force kill; SPEC §10).
//! 5. **Release** (step 5): each victim parks at its declared tier, or stops
//!    with absence proof when it does not park; the switch waits for each
//!    operation's own evidence before the waiting instance is placed.
//!
//! The caller then activates the waiting instance while holding the turn
//! (steps 6–7), and the router dispatches the queued requests (step 8).
//! Every transition is recorded on the management event stream
//! (`switch_planned`, `switch_admission_closed`, `switch_released`,
//! `switch_completed`, `switch_failed`), and each release is its own park or
//! stop operation with its own journal entries.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capyctl_store::events::SwitchPhase;
use capyctl_store::ordinary_lifecycle::switching::{
    StartStep, StartSwitchPlan, SwitchPlan, SwitchRecord, SwitchRelease, SwitchVictim,
};

use crate::coordinator::CoordinatorCommands;
use crate::fault::LifecycleFault;

/// The principal a switch's parks and stops are accepted under, so history
/// tells a switch release from an operator's or the idle policy's.
pub const SWITCH_PRINCIPAL: &str = "switch";

/// How a switch is bounded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchOptions {
    /// SPEC §10: how long a victim's accepted work may take to drain before
    /// the switch fails. Force termination is never implied.
    pub drain_timeout: Duration,
    /// How often drain, fairness and release progress are re-read.
    pub poll: Duration,
    /// How long one victim's park or stop may take to reach a terminal
    /// state before the switch reports it uncertain (it keeps running and
    /// keeps its accounting).
    pub release_timeout: Duration,
    /// How many plan-release-activate rounds one activation may take before
    /// it reports that no room could be made.
    pub max_rounds: u32,
}

impl Default for SwitchOptions {
    fn default() -> Self {
        Self {
            // The same default as a role's `shutdown.drain_timeout`.
            drain_timeout: Duration::from_secs(30),
            poll: Duration::from_millis(50),
            release_timeout: Duration::from_secs(600),
            max_rounds: 3,
        }
    }
}

/// A switch's host turns and the switch they belong to. Held while the
/// waiting instance activates, so the next group on those hosts plans against
/// what this one left. A group holds the turn of every host it names.
pub struct Room {
    _turns: Vec<tokio::sync::OwnedMutexGuard<()>>,
    /// Counts this switch as active until the waiting instance's activation
    /// ends, so another group's plan waits for it instead of refusing.
    _active: Option<Active>,
    pub switch_id: String,
    pub host: String,
    pub victims: Vec<SwitchVictim>,
    /// W10 gap (c): clears the status row of a switch whose caller gave up
    /// before recording its end.
    commands: Option<CoordinatorCommands>,
    /// Started by an operator's `--evict`, not by a waiting request.
    pub explicit: bool,
}

impl Drop for Room {
    fn drop(&mut self) {
        if let Some(commands) = &self.commands {
            if !self.switch_id.is_empty() {
                let _ = commands.end_switch(&self.switch_id);
            }
        }
    }
}

/// Why no room was made.
#[derive(Debug)]
pub enum NoRoom {
    /// The plan moved to another host while this one waited its turn; plan
    /// again.
    Moved,
    Fault(LifecycleFault),
}

impl From<LifecycleFault> for NoRoom {
    fn from(fault: LifecycleFault) -> Self {
        Self::Fault(fault)
    }
}

/// Request-driven switching over one coordinator (see the module docs).
pub struct Switcher {
    commands: CoordinatorCommands,
    options: SwitchOptions,
    /// One FIFO turn per host (ADR 0013 §8 rule 3: plans are per host).
    hosts: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Deployments with a waiting group, and how many activations wait for
    /// each: never chosen as victims (ADR 0013 §8 rule 4).
    waiting: Mutex<BTreeMap<String, usize>>,
    /// T15: one activation per deployment at a time; a concurrent one for the
    /// same deployment waits for it and then finds it serving.
    targets: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Switches between their plan and their end, and a signal when one
    /// ends: a plan that finds no room while another switch holds victims
    /// closed waits for it instead of refusing.
    active: std::sync::atomic::AtomicUsize,
    ended: tokio::sync::Notify,
    /// The `--evict` follow-ups `finish_in_background` started, tracked so a
    /// role's shutdown joins them instead of leaving them detached
    /// ([`Switcher::shutdown`]).
    background: crate::supervised::Children,
}

/// A waiting group's registration; dropping it ends the protection.
pub struct Waiting<'a> {
    switcher: &'a Switcher,
    deployment: String,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut waiting = self
            .switcher
            .waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = waiting.get_mut(&self.deployment) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                waiting.remove(&self.deployment);
            }
        }
    }
}

fn fault(error: crate::coordinator::CoordinatorCommandError) -> LifecycleFault {
    error.into()
}

impl Switcher {
    pub fn new(commands: CoordinatorCommands, options: SwitchOptions) -> Self {
        Self {
            commands,
            options,
            hosts: Mutex::new(HashMap::new()),
            waiting: Mutex::new(BTreeMap::new()),
            targets: Mutex::new(HashMap::new()),
            active: std::sync::atomic::AtomicUsize::new(0),
            ended: tokio::sync::Notify::new(),
            background: Default::default(),
        }
    }

    /// Join the `--evict` follow-ups still running, aborting any that outlive
    /// `bound`. A follow-up aborted here only waits on its start operation and
    /// records the switch's end: the operation itself is durable and carries
    /// on under the coordinator, and a new coordinator session drops the
    /// switch's closure reasons with the switch (ADR 0015 amendment,
    /// "closure reasons"), so nothing stays closed because of it.
    pub async fn shutdown(&self, bound: std::time::Duration) {
        self.background.join_within(bound).await;
    }

    pub fn options(&self) -> &SwitchOptions {
        &self.options
    }

    /// Register a waiting group for `deployment` until the guard drops.
    pub fn wait_for(&self, deployment: &str) -> Waiting<'_> {
        *self
            .waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(deployment.to_owned())
            .or_default() += 1;
        Waiting {
            switcher: self,
            deployment: deployment.to_owned(),
        }
    }

    /// T15: the deployment's activation turn. Held for a whole
    /// request-driven activation, so concurrent callers for one deployment
    /// run one after another and every later one joins the first's result.
    pub async fn target_turn(&self, deployment: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let turn = self
            .targets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(deployment.to_owned())
            .or_default()
            .clone();
        turn.lock_owned().await
    }

    fn protected(&self) -> BTreeSet<String> {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    fn turn(&self, host: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(host.to_owned())
            .or_default()
            .clone()
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        phase: SwitchPhase,
        switch_id: &str,
        target: &str,
        host: Option<&str>,
        victims: &[SwitchVictim],
        detail: &str,
        explicit: bool,
    ) {
        let record = SwitchRecord {
            phase,
            switch_id,
            target,
            host,
            victims,
            detail,
            explicit,
        };
        // Evidence only: a failed event write never changes the outcome.
        if let Err(error) = self.commands.record_switch(&record) {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!("switch {switch_id}: event not recorded: {error}"),
            );
        }
        capyctl_domain::role_log::event(serde_json::json!({
            "event": "switch",
            "phase": format!("{phase:?}"),
            "switch_id": switch_id,
            "target": target,
            "host": host,
            "victims": victims,
            "detail": detail,
        }));
    }

    /// Record that the waiting deployment is READY after this switch.
    pub fn completed(&self, room: &Room, target: &str) {
        self.record(
            SwitchPhase::Completed,
            &room.switch_id,
            target,
            Some(&room.host),
            &room.victims,
            "the waiting instance is READY and its dispatch is open",
            room.explicit,
        );
    }

    /// Record that the waiting deployment's activation failed after its
    /// victims were released. They stay parked or stopped (ADR 0013 §8 rule
    /// 6) and eligible for on-demand activation.
    pub fn activation_failed(&self, room: &Room, target: &str, reason: &str) {
        self.record(
            SwitchPhase::Failed,
            &room.switch_id,
            target,
            Some(&room.host),
            &room.victims,
            &format!("activation after release failed: {reason}"),
            room.explicit,
        );
    }

    /// Make room for one instance of `target`. `Ok(None)` when nothing needs
    /// releasing (it fits, it serves, or an activation is in flight).
    pub async fn make_room(self: &Arc<Self>, target: &str) -> Result<Option<Room>, NoRoom> {
        self.make_room_for(target, None, false).await
    }

    /// Owner decision 2026-09-23: `start deployment --evict` and `start
    /// instance --evict` run the same switch plan as a waiting request (same
    /// fairness window, drain timeout and victim order), for the deployment's
    /// next instance or the named one. A default start never evicts. Rounds
    /// that find the plan moved, or a victim's park refused (it is stopped on
    /// the next round), are retried within `max_rounds`.
    pub async fn make_room_explicit(
        self: &Arc<Self>,
        target: &str,
        instance: Option<u32>,
    ) -> Result<Option<Room>, LifecycleFault> {
        let rounds = self.options.max_rounds.max(1);
        let mut round = 0;
        loop {
            match self.make_room_for(target, instance, true).await {
                Ok(room) => return Ok(room),
                Err(NoRoom::Moved) if round < rounds => round += 1,
                Err(NoRoom::Fault(LifecycleFault::Failed(_))) if round + 1 < rounds => round += 1,
                Err(NoRoom::Moved) => {
                    return Err(LifecycleFault::Blocked(format!(
                    "no room could be made for deployment {target} after {rounds} switch round(s)"
                )))
                }
                Err(NoRoom::Fault(fault)) => return Err(fault),
            }
        }
    }

    /// Owner decision 2026-09-25: `start deployment --evict` makes room for
    /// every instance the start activates, not only the first, so that no
    /// instance of an explicit start is left queued behind capacity the
    /// operator asked to reclaim. The whole start is planned before anyone is
    /// released (each instance against the ledger as the earlier ones leave
    /// it, each victim set minimal); when any instance cannot be placed even
    /// with eviction, nothing is released and the refusal names the instance
    /// and each host's shortfall. Victims are released per host with the same
    /// switch sequence as a waiting request (fairness window, drain bound,
    /// park or verified stop). One room per host that releases anything;
    /// empty when everything fits as the ledger stands.
    pub async fn make_room_for_start(
        self: &Arc<Self>,
        target: &str,
    ) -> Result<Vec<Room>, LifecycleFault> {
        let rounds = self.options.max_rounds.max(1);
        let mut round = 0;
        loop {
            match self.start_rooms(target).await {
                Ok(rooms) => return Ok(rooms),
                Err(NoRoom::Moved) if round < rounds => round += 1,
                Err(NoRoom::Fault(LifecycleFault::Failed(_))) if round + 1 < rounds => round += 1,
                Err(NoRoom::Moved) => {
                    return Err(LifecycleFault::Blocked(format!(
                    "no room could be made for deployment {target} after {rounds} switch round(s)"
                )))
                }
                Err(NoRoom::Fault(fault)) => return Err(fault),
            }
        }
    }

    async fn start_rooms(self: &Arc<Self>, target: &str) -> Result<Vec<Room>, NoRoom> {
        let steps = match self
            .commands
            .plan_start_switch(target, &self.protected(), &Default::default())
            .map_err(fault)?
        {
            StartSwitchPlan::Steps(steps) => steps,
            // Another switch may hold its victims closed right now: wait for
            // it to end and plan again rather than refuse.
            StartSwitchPlan::Impossible { .. }
                if self.active.load(std::sync::atomic::Ordering::SeqCst) > 0 =>
            {
                let ended = self.ended.notified();
                if self.active.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    let _ = tokio::time::timeout(self.options.drain_timeout, ended).await;
                }
                return Err(NoRoom::Moved);
            }
            StartSwitchPlan::Impossible {
                instance,
                code,
                detail,
            } => {
                return Err(NoRoom::Fault(start_capacity(
                    target, instance, &code, &detail,
                )))
            }
        };
        let hosts: BTreeSet<String> = steps.iter().map(|s| s.host.clone()).collect();
        if hosts.is_empty() {
            return Ok(Vec::new());
        }
        // SPEC §10: the oldest waiting group first on each host. Turns are
        // taken in host order, so two starts spanning hosts never wait on each
        // other crosswise.
        let mut turns = BTreeMap::new();
        for host in &hosts {
            turns.insert(host.clone(), self.turn(host).lock_owned().await);
        }
        // The plan again under the turns: an earlier group may have changed
        // a host meanwhile. Final review I4: with each host's fresh
        // observation, so a park its memory cannot take is planned a stop.
        let observed = self.commands.observe_for_planning(hosts.clone()).await;
        let steps = match self
            .commands
            .plan_start_switch(target, &self.protected(), &observed)
            .map_err(fault)?
        {
            StartSwitchPlan::Steps(steps) => steps,
            StartSwitchPlan::Impossible {
                instance,
                code,
                detail,
            } => {
                return Err(NoRoom::Fault(start_capacity(
                    target, instance, &code, &detail,
                )))
            }
        };
        let again: BTreeSet<String> = steps.iter().map(|s| s.host.clone()).collect();
        if again.is_empty() {
            return Ok(Vec::new());
        }
        if again != hosts {
            return Err(NoRoom::Moved);
        }
        let mut rooms: Vec<Room> = Vec::new();
        for host in hosts {
            let here: Vec<&StartStep> = steps.iter().filter(|s| s.host == host).collect();
            let victims: Vec<SwitchVictim> = here.iter().flat_map(|s| s.victims.clone()).collect();
            let window = here
                .iter()
                .map(|s| s.admission_window_ms)
                .max()
                .unwrap_or(0);
            let switch_id = ulid::Ulid::new().to_string();
            self.record(
                SwitchPhase::Planned,
                &switch_id,
                target,
                Some(&host),
                &victims,
                &format!(
                    "instance(s) {} of an explicit start activate on {host} after releasing {} instance(s); admission window {window} ms",
                    here.iter()
                        .map(|s| format!("{}{}", s.instance, if s.wake { " (wakes)" } else { "" }))
                        .collect::<Vec<_>>()
                        .join(", "),
                    victims.len()
                ),
                true,
            );
            let active = Active::enter(self);
            let released = self
                .release(&switch_id, target, &host, &victims, window, true)
                .await;
            if let Err(error) = released {
                // A failure path records its own end; this covers one that
                // returned without it.
                let _ = self.commands.end_switch(&switch_id);
                // Hosts already released stay released and on-demand eligible
                // (ADR 0013 §8 rule 6); their switches end as failed, since
                // nothing is started.
                for room in &rooms {
                    self.activation_failed(
                        room,
                        target,
                        &format!(
                            "the release on host {host} did not complete; nothing was started"
                        ),
                    );
                }
                return Err(NoRoom::Fault(error));
            }
            rooms.push(Room {
                _turns: vec![turns.remove(&host).expect("a turn per planned host")],
                _active: Some(active),
                switch_id,
                host,
                victims,
                commands: Some(self.commands.clone()),
                explicit: true,
            });
        }
        Ok(rooms)
    }

    /// Owner decision 2026-09-23: after an explicit `--evict` start was
    /// accepted, keep the host's turn until that start's operation ends, then
    /// record the switch's end, off the caller's request.
    pub fn finish_in_background(self: &Arc<Self>, room: Room, target: String, operation: String) {
        let switcher = self.clone();
        self.background.track(tokio::spawn(async move {
            let outcome = switcher.settled(&operation).await;
            if !room.switch_id.is_empty() {
                match outcome {
                    Ok(()) => switcher.completed(&room, &target),
                    Err(error) => switcher.activation_failed(&room, &target, &error.to_string()),
                }
            }
            drop(room);
        }));
    }

    /// Make room for one instance of `target` (`only` names it), recording
    /// the switch as `explicit` when an operator's `--evict` started it.
    async fn make_room_for(
        self: &Arc<Self>,
        target: &str,
        only: Option<u32>,
        explicit: bool,
    ) -> Result<Option<Room>, NoRoom> {
        let plan = self
            .commands
            .plan_switch(
                target,
                only,
                explicit,
                &self.protected(),
                &Default::default(),
            )
            .map_err(fault)?;
        let hosts = match plan {
            SwitchPlan::FitsNow => return Ok(None),
            // Another switch may hold its victims closed right now: wait for
            // it to end and plan again rather than refuse.
            SwitchPlan::Impossible(_)
                if self.active.load(std::sync::atomic::Ordering::SeqCst) > 0 =>
            {
                let ended = self.ended.notified();
                if self.active.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    let _ = tokio::time::timeout(self.options.drain_timeout, ended).await;
                }
                return Err(NoRoom::Moved);
            }
            SwitchPlan::Impossible(code) => return Err(NoRoom::Fault(capacity(target, &code))),
            SwitchPlan::Evict { host, .. } => vec![host],
            // ADR 0028 §5: a group activates on every named host at once.
            SwitchPlan::EvictGroup { hosts, .. } => hosts,
        };
        // SPEC §10: the oldest waiting group first. Turns are FIFO, and taken
        // in host order, so a group never waits crosswise on another switch.
        let mut turns = Vec::with_capacity(hosts.len());
        for host in hosts.iter().collect::<BTreeSet<_>>() {
            turns.push(self.turn(host).lock_owned().await);
        }
        let head = hosts[0].clone();
        // Final review I4: planned again with each host's fresh observation,
        // so a park its memory cannot take now is planned as a stop.
        let observed = self.commands.observe_for_planning(hosts.clone()).await;
        let plan = self
            .commands
            .plan_switch(target, only, explicit, &self.protected(), &observed)
            .map_err(fault)?;
        let (detail, victims, admission_window_ms) = match plan {
            SwitchPlan::FitsNow => {
                return Ok(Some(Room {
                    _turns: turns,
                    _active: None,
                    switch_id: String::new(),
                    host: head,
                    victims: Vec::new(),
                    commands: None,
                    explicit,
                }))
            }
            SwitchPlan::Impossible(code) => return Err(NoRoom::Fault(capacity(target, &code))),
            SwitchPlan::Evict { host: moved, .. } if hosts != std::slice::from_ref(&moved) => {
                return Err(NoRoom::Moved)
            }
            SwitchPlan::EvictGroup { hosts: moved, .. } if moved != hosts => {
                return Err(NoRoom::Moved)
            }
            SwitchPlan::Evict {
                host,
                instance,
                wake,
                victims,
                admission_window_ms,
            } => (
                format!(
                    "instance {instance} {} on {host} after releasing {} instance(s); admission window {admission_window_ms} ms",
                    if wake { "wakes" } else { "starts" },
                    victims.len()
                ),
                victims,
                admission_window_ms,
            ),
            // ADR 0028 §5, SPEC §11: every named host's victims were planned
            // before any is released; each is released once, a group victim
            // whole.
            SwitchPlan::EvictGroup {
                hosts,
                instance,
                wake,
                victims,
                admission_window_ms,
            } => {
                let victims: Vec<SwitchVictim> = victims.into_values().flatten().collect();
                (
                    format!(
                        "group instance {instance} {} on {} after releasing {} instance(s); admission window {admission_window_ms} ms",
                        if wake { "wakes" } else { "starts" },
                        hosts.join(", "),
                        victims.len()
                    ),
                    victims,
                    admission_window_ms,
                )
            }
        };
        let switch_id = ulid::Ulid::new().to_string();
        self.record(
            SwitchPhase::Planned,
            &switch_id,
            target,
            Some(&head),
            &victims,
            &detail,
            explicit,
        );
        let active = Active::enter(self);
        let released = self
            .release(
                &switch_id,
                target,
                &head,
                &victims,
                admission_window_ms,
                explicit,
            )
            .await;
        if released.is_err() {
            // A failure path records its own end; this covers one that
            // returned without it.
            let _ = self.commands.end_switch(&switch_id);
        }
        released?;
        Ok(Some(Room {
            _turns: turns,
            _active: Some(active),
            switch_id,
            host: head,
            victims,
            commands: Some(self.commands.clone()),
            explicit,
        }))
    }

    async fn release(
        &self,
        switch_id: &str,
        target: &str,
        host: &str,
        victims: &[SwitchVictim],
        admission_window_ms: i64,
        explicit: bool,
    ) -> Result<(), LifecycleFault> {
        let started = Instant::now();
        let window_ms = admission_window_ms.max(0);
        let window = Duration::from_millis(window_ms as u64);
        // Step 3 (fairness): the last READY instance of a busy deployment
        // keeps admitting until the window, fixed at the switch's start,
        // ends. Its own traffic never moves that instant.
        let last_ready: Vec<&SwitchVictim> = victims.iter().filter(|v| v.last_ready).collect();
        if !last_ready.is_empty() {
            let closes_at = started + window;
            loop {
                let now_ms = self.commands.now_ms()?;
                let mut busy = false;
                for v in &last_ready {
                    let recent = self
                        .commands
                        .last_activity(&v.deployment_id, v.generation)
                        .is_some_and(|at| now_ms.saturating_sub(at) < window_ms);
                    let leased = self
                        .commands
                        .switch_outstanding_leases(&v.deployment_id, v.instance, v.generation)
                        .map_err(fault)?
                        > 0;
                    busy |= recent || leased;
                }
                let now = Instant::now();
                if !busy || now >= closes_at {
                    break;
                }
                tokio::time::sleep(self.options.poll.min(closes_at - now)).await;
            }
        }
        let mut closed: Vec<&SwitchVictim> = Vec::new();
        for v in victims {
            match self
                .commands
                .close_for_switch(&v.deployment_id, v.instance, v.generation)
            {
                Ok(true) => closed.push(v),
                Ok(false) => {
                    self.reopen(&closed);
                    let reason = format!(
                        "victim {}/{} is no longer the READY incarnation the plan chose",
                        v.deployment_id, v.instance
                    );
                    self.record(
                        SwitchPhase::Failed,
                        switch_id,
                        target,
                        Some(host),
                        victims,
                        &reason,
                        explicit,
                    );
                    return Err(LifecycleFault::Unavailable(format!(
                        "switch to {target} failed: {reason}; retry shortly"
                    )));
                }
                Err(error) => {
                    self.reopen(&closed);
                    return Err(fault(error));
                }
            }
        }
        self.record(
            SwitchPhase::AdmissionClosed,
            switch_id,
            target,
            Some(host),
            victims,
            &format!(
                "admission closed after {} ms; draining accepted work",
                started.elapsed().as_millis()
            ),
            explicit,
        );
        // Step 4 (drain): every lease charged to a victim closes on
        // evidence, bounded; a timeout fails the switch and the victims
        // serve again. Nothing is killed.
        let drain_until = Instant::now() + self.options.drain_timeout;
        loop {
            let mut outstanding = 0;
            for v in &closed {
                outstanding += self
                    .commands
                    .switch_outstanding_leases(&v.deployment_id, v.instance, v.generation)
                    .map_err(|error| {
                        self.reopen(&closed);
                        fault(error)
                    })?;
            }
            if outstanding == 0 {
                break;
            }
            if Instant::now() >= drain_until {
                self.reopen(&closed);
                let reason = format!(
                    "drain timeout after {} ms with {outstanding} request(s) still charged; the victims serve again",
                    self.options.drain_timeout.as_millis()
                );
                self.record(
                    SwitchPhase::Failed,
                    switch_id,
                    target,
                    Some(host),
                    victims,
                    &reason,
                    explicit,
                );
                return Err(LifecycleFault::Unavailable(format!(
                    "switch to {target} failed: {reason}; retry shortly"
                )));
            }
            tokio::time::sleep(self.options.poll).await;
        }
        // Step 5 (release): park or stop each victim; each completes only
        // on its own evidence.
        let mut operations = Vec::new();
        for (index, v) in closed.iter().enumerate() {
            let key = format!("switch:{switch_id}:{}:{}", v.deployment_id, v.instance);
            match self.commands.accept_switch_release(
                SWITCH_PRINCIPAL,
                &v.deployment_id,
                v.instance,
                v.generation,
                &key,
                v.parks,
            ) {
                Ok(release) => operations.push((*v, release)),
                Err(error) => {
                    // Nothing was accepted for this victim or the rest:
                    // they serve again. Those already accepted proceed.
                    self.reopen(&closed[index..]);
                    let reason = format!(
                        "release of {}/{} refused: {error}",
                        v.deployment_id, v.instance
                    );
                    self.record(
                        SwitchPhase::Failed,
                        switch_id,
                        target,
                        Some(host),
                        victims,
                        &reason,
                        explicit,
                    );
                    return Err(fault(error));
                }
            }
        }
        // Every accepted release is waited for on its own evidence, even after
        // one fails, so the next round plans against settled victims (a park
        // refused on its own evidence is stopped then) rather than one still
        // in flight (owner decision 2026-09-25: a start may release several).
        let mut first_failure: Option<(String, LifecycleFault)> = None;
        for (v, release) in &operations {
            if let Err(error) = self.settled(&release.operation_id).await {
                if first_failure.is_none() {
                    let reason = format!(
                        "{} of {}/{} did not complete: {error}",
                        if release.parked { "park" } else { "stop" },
                        v.deployment_id,
                        v.instance
                    );
                    first_failure = Some((reason, error));
                }
            }
        }
        if let Some((reason, error)) = first_failure {
            self.record(
                SwitchPhase::Failed,
                switch_id,
                target,
                Some(host),
                victims,
                &reason,
                explicit,
            );
            return Err(error);
        }
        self.record(
            SwitchPhase::Released,
            switch_id,
            target,
            Some(host),
            victims,
            &operations
                .iter()
                .map(|(v, r)| release_line(v, r))
                .collect::<Vec<_>>()
                .join(", "),
            explicit,
        );
        Ok(())
    }

    /// Reopen victims this switch closed but never released.
    fn reopen(&self, victims: &[&SwitchVictim]) {
        for v in victims {
            if let Err(error) =
                self.commands
                    .reopen_after_switch(&v.deployment_id, v.instance, v.generation)
            {
                capyctl_domain::role_log::notice(
                    capyctl_domain::role_log::Level::Warning,
                    &format!(
                        "switch: {}/{} not reopened: {error}",
                        v.deployment_id, v.instance
                    ),
                );
            }
        }
    }

    /// Wait for a release operation's terminal state on its own evidence.
    async fn settled(&self, operation: &str) -> Result<(), LifecycleFault> {
        let until = Instant::now() + self.options.release_timeout;
        loop {
            let id = operation.to_owned();
            let (state, error, uncertain) = self
                .commands
                .read(move |store| {
                    let row = store.get_operation(&id)?;
                    let uncertain = store.operation_is_uncertain(&id)?;
                    Ok(row.map(|r| (r.state, r.error_code, uncertain)))
                })?
                .ok_or_else(|| LifecycleFault::NotFound(operation.to_owned()))?;
            use capyctl_store::deployments::OpState;
            match state {
                _ if uncertain => {
                    return Err(LifecycleFault::Uncertain(format!(
                        "release {operation} is uncertain; its accounting is retained"
                    )))
                }
                OpState::Succeeded => return Ok(()),
                OpState::Failed => {
                    return Err(LifecycleFault::Failed(format!(
                        "release {operation} failed with code {}",
                        error.unwrap_or_else(|| "unknown".into())
                    )))
                }
                OpState::Pending | OpState::Running if Instant::now() >= until => {
                    return Err(LifecycleFault::Uncertain(format!(
                        "release {operation} has not reached a terminal state; it keeps running"
                    )))
                }
                OpState::Pending | OpState::Running => tokio::time::sleep(self.options.poll).await,
            }
        }
    }
}

/// One switch between its plan and its end (see `Switcher::active`).
struct Active(Arc<Switcher>);

impl Active {
    fn enter(switcher: &Arc<Switcher>) -> Self {
        switcher
            .active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(switcher.clone())
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.0
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        self.0.ended.notify_waiters();
    }
}

/// Owner decision 2026-09-25: an explicit start one of whose instances fits
/// nowhere even with eviction; the closed code leads, so management keeps
/// classing it as a capacity block, and each host's shortfall follows.
/// Discrete GPU design §5: the status line for one released victim says
/// whether it parked or stopped, and names a victim that parks but stopped
/// because its parked footprint did not fit the host after the switch. That
/// can be host RAM (a `host_backed` copy) or the card (a `deep` residue beside
/// the waiting instance), so the line names neither (found live 2026-09-26: a
/// `deep` victim on a full card was reported "host RAM full").
fn release_line(v: &SwitchVictim, r: &SwitchRelease) -> String {
    format!(
        "{}/{} released: {} ({})",
        v.deployment_id,
        v.instance,
        if r.parked {
            "parked"
        } else if v.park_does_not_fit {
            "stopped (no room to park)"
        } else {
            "stopped"
        },
        r.operation_id
    )
}

fn start_capacity(target: &str, instance: u32, code: &str, detail: &str) -> LifecycleFault {
    LifecycleFault::Blocked(format!(
        "{code}: instance {instance} of deployment {target} cannot be placed even with eviction: {detail}; nothing was released"
    ))
}

fn capacity(target: &str, code: &str) -> LifecycleFault {
    LifecycleFault::Blocked(format!(
        "deployment {target} fits on no allowed host even after releasing every eligible READY instance ({code})"
    ))
}

#[cfg(test)]
mod release_line_tests {
    use super::*;

    // Discrete GPU design §5: the status line names which release happened.
    // T27
    #[test]
    fn the_status_line_names_park_stop_and_a_copy_that_did_not_fit() {
        let victim = |park_does_not_fit| SwitchVictim {
            deployment_id: "d".into(),
            instance: 0,
            generation: 1,
            parks: false,
            park_does_not_fit,
            last_ready: false,
            serves_elsewhere: false,
        };
        let release = |parked| SwitchRelease {
            operation_id: "op".into(),
            parked,
        };
        assert_eq!(
            release_line(&victim(false), &release(true)),
            "d/0 released: parked (op)"
        );
        assert_eq!(
            release_line(&victim(true), &release(false)),
            "d/0 released: stopped (no room to park) (op)"
        );
        assert_eq!(
            release_line(&victim(false), &release(false)),
            "d/0 released: stopped (op)"
        );
    }
}
