//! Switching engine (F1 design §5): single wake join (T15), A→B switching
//! with drain → quiescence → release evidence → reserve → launch → gate
//! open (T16), bounded non-resetting fairness windows (T19), and the
//! failure branch (A reopens, B fail-fast).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mllm_controller::LifecyclePort;
use mllm_domain::LifecycleState;

/// Bounded, non-resetting admission window (T19): busy traffic cannot
/// extend it; it closes a fixed interval after it opens.
#[derive(Debug)]
pub struct AdmissionWindow {
    opened_at: Instant,
    duration: Duration,
}

impl AdmissionWindow {
    pub fn open(duration: Duration) -> Self {
        Self {
            opened_at: Instant::now(),
            duration,
        }
    }

    /// No-op by contract: sustained A load NEVER resets the window.
    pub fn try_extend(&mut self) {}

    pub fn expired(&self) -> bool {
        self.opened_at.elapsed() >= self.duration
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SwitchError {
    #[error("switch failed: drain timeout on {0} (A reopens)")]
    DrainTimeout(String),
    #[error("switch failed: activation error for {0}: {1}")]
    Activation(String, String),
    #[error("switch failed: admission blocked for {0}: {1:?}")]
    AdmissionBlocked(String, String),
}

/// Leader/follower wake join (T15): concurrent callers waking the same
/// target join ONE wake. The leader publishes its outcome on a per-claim
/// watch channel; followers subscribe BEFORE the claim check, so an outcome
/// published between the claim check and the await is never lost (a watch
/// value is retained; `notify_waiters` is not). The joined slot is removed
/// from the map only AFTER publishing: a caller arriving in that window
/// reads the completed outcome instead of racing a second wake.
pub struct WakeJoin<E> {
    wakes: Mutex<HashMap<String, Arc<JoinSlot<E>>>>,
}

struct JoinSlot<E> {
    tx: tokio::sync::watch::Sender<Option<Result<u64, E>>>,
    claimed: std::sync::atomic::AtomicBool,
}

impl<E: Clone> Default for WakeJoin<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Clone> WakeJoin<E> {
    pub fn new() -> Self {
        Self {
            wakes: Mutex::new(HashMap::new()),
        }
    }

    /// Run (or join) the wake for `key`: the first caller of the cohort
    /// activates; every caller receives the wake's outcome.
    pub async fn join<F, Fut>(&self, key: &str, wake: F) -> Result<u64, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<u64, E>>,
    {
        // Subscribe BEFORE the claim check: a freshly created receiver sees
        // the current value, so an outcome already published at subscribe
        // time is observed immediately — no lost wake.
        let (slot, mut rx) = {
            let mut wakes = self.wakes.lock().unwrap();
            let slot = wakes
                .entry(key.to_string())
                .or_insert_with(|| {
                    Arc::new(JoinSlot {
                        tx: tokio::sync::watch::channel(None).0,
                        claimed: std::sync::atomic::AtomicBool::new(false),
                    })
                })
                .clone();
            let rx = slot.tx.subscribe();
            (slot, rx)
        };
        // Exactly one leader claims the wake; the rest await the published
        // outcome (retained on the watch, not a one-shot notify).
        if !slot.claimed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let r = wake().await;
            // Publish BEFORE removing from the map: a caller arriving in
            // between joins the outcome, never a second wake.
            let _ = slot.tx.send(Some(r.clone()));
            self.wakes.lock().unwrap().remove(key);
            r
        } else {
            loop {
                if let Some(r) = rx.borrow().clone() {
                    return r;
                }
                // The sender lives inside our clone of the slot's Arc, so
                // `changed` cannot fail before a publish — it only wakes
                // when the outcome lands.
                let _ = rx.changed().await;
            }
        }
    }
}

impl<E: Clone + Send + Sync + 'static> WakeJoin<E> {
    /// Join (or start) the wake for `key` as a detached task (SPEC §10 step
    /// 2, W10). Unlike [`WakeJoin::join`], the wake does not run inside the
    /// first caller's future: a client that disconnects, or a caller that
    /// stops waiting at its deadline, never cancels the activation the other
    /// waiting requests joined, and a later caller still joins it.
    ///
    /// `aborted` is the outcome published if the wake task ends without one (it
    /// panicked, or the runtime dropped it): the slot is then removed, so the
    /// waiting callers are answered and the next caller starts a fresh wake
    /// instead of joining a claim nobody will ever complete.
    pub async fn join_detached<F, Fut>(
        self: &Arc<Self>,
        key: &str,
        wake: F,
        aborted: E,
    ) -> Result<u64, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<u64, E>> + Send + 'static,
    {
        let (slot, mut rx) = {
            let mut wakes = self.wakes.lock().unwrap_or_else(|p| p.into_inner());
            let slot = wakes
                .entry(key.to_string())
                .or_insert_with(|| {
                    Arc::new(JoinSlot {
                        tx: tokio::sync::watch::channel(None).0,
                        claimed: std::sync::atomic::AtomicBool::new(false),
                    })
                })
                .clone();
            let rx = slot.tx.subscribe();
            (slot, rx)
        };
        if !slot.claimed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let task = wake();
            let join = self.clone();
            let owned = slot.clone();
            let key = key.to_string();
            tokio::spawn(async move {
                let mut publish = Publish {
                    join,
                    slot: owned,
                    key,
                    aborted: Some(aborted),
                };
                let outcome = task.await;
                publish.aborted = None;
                publish.send(outcome);
            });
        }
        loop {
            if let Some(r) = rx.borrow().clone() {
                return r;
            }
            // The slot (and its sender) is held here, so `changed` returns
            // only when the outcome lands.
            let _ = rx.changed().await;
        }
    }
}

/// Publishes a detached wake's outcome and frees its slot — on completion, or
/// with the `aborted` outcome when the wake task unwinds or is dropped first.
struct Publish<E> {
    join: Arc<WakeJoin<E>>,
    slot: Arc<JoinSlot<E>>,
    key: String,
    aborted: Option<E>,
}

impl<E> Publish<E> {
    fn send(&self, outcome: Result<u64, E>) {
        // Publish before removing: a caller arriving in between reads the
        // outcome instead of starting a second wake.
        let _ = self.slot.tx.send(Some(outcome));
        let mut wakes = self.join.wakes.lock().unwrap_or_else(|p| p.into_inner());
        if wakes
            .get(&self.key)
            .is_some_and(|s| Arc::ptr_eq(s, &self.slot))
        {
            wakes.remove(&self.key);
        }
    }
}

impl<E> Drop for Publish<E> {
    fn drop(&mut self) {
        if let Some(aborted) = self.aborted.take() {
            self.send(Err(aborted));
        }
    }
}

/// F1 router-owned switching over the F1 controller. Not constructed by any
/// role: under the coordinator, victims are chosen and released by the
/// lifecycle authority (`mllm_controller::switching`, W10), and the router
/// only queues and joins (`LifecyclePort::activate_for_request`).
pub struct SwitchEngine {
    controller: Arc<dyn LifecyclePort>,
    /// Bounded drain grace before the best-effort abort (design §5).
    drain_grace: Duration,
    /// In-flight wake joins (T15): concurrent activations for one target
    /// join a single wake.
    wake_join: WakeJoin<SwitchError>,
}

impl SwitchEngine {
    pub fn new(controller: Arc<dyn LifecyclePort>, drain_grace: Duration) -> Self {
        Self {
            controller,
            drain_grace,
            wake_join: WakeJoin::new(),
        }
    }

    /// Bring `target` to READY, draining any currently-ready deployment
    /// first. Concurrent activations for the same target join one wake
    /// (T15); returns the target's generation after activation (T16).
    pub async fn switch_to(&self, target: &str) -> Result<u64, SwitchError> {
        self.wake_join.join(target, || self.activate(target)).await
    }

    async fn current_generation(&self, target: &str) -> Result<u64, SwitchError> {
        let gen = self
            .controller
            .get_deployment(target)
            .map_err(|e| SwitchError::Activation(target.into(), e.to_string()))?
            .ok_or_else(|| SwitchError::Activation(target.into(), "vanished".into()))?
            .current_generation;
        Ok(gen.max(0) as u64)
    }

    async fn activate(&self, target: &str) -> Result<u64, SwitchError> {
        let (target_state, target_observed) = {
            let row = self
                .controller
                .get_deployment(target)
                .map_err(|e| SwitchError::Activation(target.into(), e.to_string()))?
                .ok_or_else(|| SwitchError::Activation(target.into(), "vanished".into()))?;
            (row.desired_state, row.observed_state)
        };
        let _ = target_state;

        if target_observed == LifecycleState::Ready {
            return self.current_generation(target).await;
        }

        // Step 1: any other READY deployment holds the pool — drain and
        // release it before B can start (one pool, exclusive residency).
        let ready_others: Vec<String> = self
            .controller
            .ready_deployments_excluding(target)
            .map_err(|e| SwitchError::Activation(target.into(), e.to_string()))?;
        for a in ready_others {
            self.drain_and_release(&a).await?;
        }

        // Step 2: wake the target (Start handles Parked→Waking and
        // Stopped→Starting).
        let op = self
            .controller
            .request_transition(target, mllm_domain::LifecycleAction::Start)
            .await
            .map_err(|e| SwitchError::Activation(target.into(), e.to_string()))?;
        self.controller
            .wait_terminal(&op)
            .await
            .map_err(|e| SwitchError::Activation(target.into(), e.to_string()))?;
        self.current_generation(target).await
    }

    /// Drain A: close admission, wait for in-flight work (bounded grace),
    /// confirm quiescence, then park-or-stop with release evidence. On
    /// failure: A reopens (not suspended, window preserved) and the failed
    /// switch is journaled (T16/T19 failure branch).
    async fn drain_and_release(&self, a: &str) -> Result<(), SwitchError> {
        let deadline = Instant::now() + self.drain_grace;
        // Drain: the fake's observe_work is the quiescence oracle; real
        // engines' in-flight telemetry arrives via the adapter (F1 design
        // §5: bounded grace → best-effort abort → residual uncertainty in
        // release evidence).
        loop {
            let work = self.controller.observe_adapter(a).await;
            match work {
                Ok(mllm_adapters::traits::WorkObservation::Idle) => break,
                // Unknown in-flight state (vLLM exposes no per-request
                // surface): the drain liveness policy (design §5) — proceed
                // with the residual uncertainty recorded; never block
                // forever on unprovable work.
                Ok(mllm_adapters::traits::WorkObservation::Unknown) => {
                    self.journal_switch(
                        a,
                        r#"{"event":"drain_unknown","residual":"engine work unprovable"}"#,
                    );
                    break;
                }
                Ok(other) => {
                    if Instant::now() >= deadline {
                        return Err(self.fail_switch(a).await);
                    }
                    let _ = other;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => {
                    // Uncertainty cannot block forever: bounded grace, then
                    // proceed on best effort with the residual uncertainty
                    // recorded (design §5 drain liveness policy).
                    if Instant::now() >= deadline {
                        self.journal_switch(a, r#"{"event":"switch_drain_uncertain"}"#);
                        break;
                    }
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        // Release A: park if qualified, else stop (restart-only fallback).
        let deployment = self
            .controller
            .get_deployment(a)
            .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?;
        let observed = deployment.as_ref().map(|r| r.observed_state);
        if observed == Some(LifecycleState::Ready) {
            // Stock profiles release by stopping: parking would keep
            // the shared F1 engine port bound. Only the sleep profile
            // is eligible for the park path (design §7).
            if deployment.as_ref().is_some_and(|r| r.kind != "vllm-sleep") {
                let stop = self
                    .controller
                    .idle_stop(a)
                    .await
                    .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?;
                self.controller
                    .wait_terminal(&stop)
                    .await
                    .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?;
                return Ok(());
            }
            let op = self
                .controller
                .request_transition(a, mllm_domain::LifecycleAction::Park)
                .await;
            match op {
                Ok(op) => {
                    if self.controller.wait_terminal(&op).await.is_err() {
                        // Park failed: stop after drain (design §7).
                        let stop = self
                            .controller
                            .request_transition(a, mllm_domain::LifecycleAction::Stop)
                            .await
                            .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?;
                        self.controller
                            .wait_terminal(&stop)
                            .await
                            .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?;
                    }
                }
                Err(_) => {
                    // Parking unsupported (restart-only): stop instead —
                    // the idle-stop path keeps A on-demand eligible.
                    let stop = self
                        .controller
                        .idle_stop(a)
                        .await
                        .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?;
                    self.controller
                        .wait_terminal(&stop)
                        .await
                        .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    async fn fail_switch(&self, a: &str) -> SwitchError {
        // A reopens with its window state preserved: not suspended, the
        // failed switch does not punish A. The event feeds SPEC §17's
        // failed-switches metric.
        let _ = self.controller.clear_suspension(a);
        self.journal_switch_failed(a);
        SwitchError::DrainTimeout(a.to_string())
    }

    fn journal_switch_failed(&self, a: &str) {
        self.journal_switch(a, r#"{"event":"switch_failed","reason":"drain_timeout"}"#);
    }

    fn journal_switch(&self, a: &str, evidence: &str) {
        // Journal on the deployment's latest operation (evidence-only).
        let op = self.controller.latest_operation(a).ok().flatten();
        if let Some(op) = op {
            let _ = self
                .controller
                .journal(Some("switch"), Some(&op.id), None, evidence);
        }
    }
}
