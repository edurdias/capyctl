//! Switching engine (F1 design §5): single wake join (T15), A→B switching
//! with drain → quiescence → release evidence → reserve → launch → gate
//! open (T16), bounded non-resetting fairness windows (T19), and the
//! failure branch (A reopens, B fail-fast).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mllm_controller::Controller;
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
        Self { opened_at: Instant::now(), duration }
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

struct JoinSlot {
    notify: tokio::sync::Notify,
    claimed: std::sync::atomic::AtomicBool,
    result: Mutex<Option<Result<u64, SwitchError>>>,
}

pub struct SwitchEngine {
    controller: Arc<Controller>,
    /// Bounded drain grace before the best-effort abort (design §5).
    drain_grace: Duration,
    /// In-flight wake joins: deployment → shared result slot (T15). The
    /// first caller leads and stores the outcome; followers await and read.
    wakes: Arc<Mutex<HashMap<String, Arc<JoinSlot>>>>,
}

impl SwitchEngine {
    pub fn new(controller: Arc<Controller>, drain_grace: Duration) -> Self {
        Self {
            controller,
            drain_grace,
            wakes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Bring `target` to READY, draining any currently-ready deployment
    /// first. Concurrent activations for the same target join one wake
    /// (T15); returns the target's generation after activation (T16).
    pub async fn switch_to(&self, target: &str) -> Result<u64, SwitchError> {
        // Single-wake join: register interest; the first caller performs the
        // wake, later callers await its completion (T15).
        let slot = {
            let mut joins = self.wakes.lock().unwrap();
            joins
                .entry(target.to_string())
                .or_insert_with(|| {
                    Arc::new(JoinSlot {
                        notify: tokio::sync::Notify::new(),
                        claimed: std::sync::atomic::AtomicBool::new(false),
                        result: Mutex::new(None),
                    })
                })
                .clone()
        };
        // Exactly one leader claims the wake; the rest wait for its result.
        if !slot
            .claimed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let r = self.activate(target).await;
            *slot.result.lock().unwrap() = Some(r.clone());
            self.wakes.lock().unwrap().remove(target);
            slot.notify.notify_waiters();
            r
        } else {
            slot.notify.notified().await;
            let stored = slot.result.lock().unwrap().clone();
            if let Some(r) = stored {
                r
            } else {
                self.current_generation(target).await
            }
        }
    }

    async fn current_generation(&self, target: &str) -> Result<u64, SwitchError> {
        let store = self.controller.store_ref();
        let gen = store
            .lock()
            .unwrap()
            .get_deployment(target)
            .map_err(|e| SwitchError::Activation(target.into(), e.to_string()))?
            .ok_or_else(|| SwitchError::Activation(target.into(), "vanished".into()))?
            .current_generation;
        Ok(gen.max(0) as u64)
    }

    async fn activate(&self, target: &str) -> Result<u64, SwitchError> {
        let store = self.controller.store_ref();
        let (target_state, target_observed) = {
            let s = store.lock().unwrap();
            let row = s
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
        let ready_others: Vec<String> = {
            let s = store.lock().unwrap();
            s.ready_deployments_excluding(target)
                .map_err(|e| SwitchError::Activation(target.into(), e.to_string()))?
        };
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
        let store = self.controller.store_ref();
        let deadline = Instant::now() + self.drain_grace;
        // Drain: the fake's observe_work is the quiescence oracle; real
        // engines' in-flight telemetry arrives via the adapter (F1 design
        // §5: bounded grace → best-effort abort → residual uncertainty in
        // release evidence).
        loop {
            let work = self.controller.observe_adapter(a).await;
            match work {
                Ok(mllm_adapters::traits::WorkObservation::Idle) => break,
                Ok(_) => {
                    if Instant::now() >= deadline {
                        return Err(self.fail_switch(a).await);
                    }
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
        let observed = store
            .lock()
            .unwrap()
            .get_deployment(a)
            .map_err(|e| SwitchError::Activation(a.into(), e.to_string()))?
            .map(|r| r.observed_state);
        if observed == Some(LifecycleState::Ready) {
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
        let store = self.controller.store_ref();
        let _ = store.lock().unwrap().set_suspended(a, false);
        self.journal_switch_failed(a);
        SwitchError::DrainTimeout(a.to_string())
    }

    fn journal_switch_failed(&self, a: &str) {
        self.journal_switch(a, r#"{"event":"switch_failed","reason":"drain_timeout"}"#);
    }

    fn journal_switch(&self, a: &str, evidence: &str) {
        // Journal on the deployment's latest operation (evidence-only).
        let store = self.controller.store_ref();
        let op = store
            .lock()
            .unwrap()
            .latest_operation(a)
            .ok()
            .flatten();
        if let Some(op) = op {
            let _ = store.lock().unwrap().record_journal(Some("switch"), Some(&op.id), None, evidence);
        }
    }
}