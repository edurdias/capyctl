//! Admission accounting for chat requests (T19 groundwork): per-deployment
//! in-flight bounds. In-flight counts are conservative (released only on
//! completion/confirmed cancellation — Task 8).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::RouterDeps;

/// SPEC §10 (T19): requests waiting for an in-flight slot of one deployment,
/// served strictly in arrival order.
#[derive(Default)]
struct SlotQueue {
    /// Tickets of the requests waiting, oldest first.
    waiting: std::collections::VecDeque<u64>,
    next_ticket: u64,
    /// Woken whenever a slot frees or the head of the queue changes.
    changed: Arc<tokio::sync::Notify>,
}

/// Per-process in-flight registry. Server-restart durability is NOT
/// promised (F1 design §5: bodies/streams don't survive router restarts).
#[derive(Default)]
pub struct InFlight {
    counts: Mutex<std::collections::HashMap<String, Arc<AtomicUsize>>>,
    /// SPEC §10 (T19): per-deployment FIFO of requests waiting for a slot.
    /// Lock order: `slots`, then `counts`.
    slots: Mutex<std::collections::HashMap<String, SlotQueue>>,
    /// ADR 0013 §10 (I3): per-instance routing counts and tie rotation. A
    /// routing hint only; accounting is the per-deployment count above and the
    /// durable lease.
    pub(crate) instances: crate::balance::InstanceCounts,
    /// SPEC §10 step 1 (W10): requests waiting for a deployment to become
    /// servable, bounded by count and buffered bytes, each under a deadline.
    pub waiting: Arc<crate::queue::WaitQueue>,
    /// SPEC §17 (M80): the router's per-request latency distributions and
    /// its timing-header switch.
    pub latency: Arc<crate::timing::LatencyRecorder>,
}

impl InFlight {
    /// SPEC §10 (W10): an in-flight registry whose waiting requests are
    /// bounded by `limits`.
    pub fn with_wait_limits(limits: crate::queue::WaitLimits) -> Self {
        Self {
            waiting: Arc::new(crate::queue::WaitQueue::new(limits)),
            ..Self::default()
        }
    }

    /// ADR 0013 §10: requests this router has outstanding on one instance
    /// incarnation.
    pub fn instance_in_flight(&self, deployment: &str, generation: i64) -> usize {
        self.instances.current(deployment, generation)
    }

    /// Atomic-conditional increment (T19): registers one slot only while
    /// the deployment's in-flight count is below `max`; `false` when the
    /// bound is reached — the check and the increment share the lock, so
    /// the check-then-act race cannot over-admit.
    pub fn try_increment(&self, deployment: &str, max: usize) -> bool {
        let mut map = self.counts.lock().unwrap();
        let c = map.entry(deployment.to_string()).or_default();
        if c.load(Ordering::SeqCst) >= max {
            return false;
        }
        c.fetch_add(1, Ordering::SeqCst);
        true
    }

    pub fn increment(&self, deployment: &str) -> usize {
        let mut map = self.counts.lock().unwrap();
        let c = map.entry(deployment.to_string()).or_default();
        c.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn decrement(&self, deployment: &str) {
        if let Some(c) = self.counts.lock().unwrap().get(deployment) {
            c.fetch_sub(1, Ordering::SeqCst);
        }
        // SPEC §10 (T19): a freed slot goes to the oldest waiting request.
        self.wake(deployment);
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, SlotQueue>> {
        // Plain queues: a panic elsewhere leaves nothing half-written.
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wake(&self, deployment: &str) {
        if let Some(queue) = self.slots().get(deployment) {
            queue.changed.notify_waiters();
        }
    }

    /// Requests waiting for an in-flight slot of `deployment` now.
    pub fn slot_waiters(&self, deployment: &str) -> usize {
        self.slots().get(deployment).map_or(0, |q| q.waiting.len())
    }

    /// SPEC §10 (T19): take one in-flight slot of `deployment`, waiting in
    /// arrival order until `deadline` when the bound is reached. A request
    /// arriving while others wait queues behind them, so a fresh arrival never
    /// bypasses one that has waited. `None` once the deadline passes; no
    /// accounting was taken. Dropping the future leaves the queue.
    pub async fn acquire_arc(
        self: &Arc<Self>,
        deployment: &str,
        max: usize,
        deadline: tokio::time::Instant,
    ) -> Option<StaticStreamGuard> {
        let (ticket, changed) = {
            let mut slots = self.slots();
            let queue = slots.entry(deployment.to_owned()).or_default();
            if queue.waiting.is_empty() && self.try_increment(deployment, max) {
                return Some(self.static_guard(deployment));
            }
            let ticket = queue.next_ticket;
            queue.next_ticket += 1;
            queue.waiting.push_back(ticket);
            (ticket, queue.changed.clone())
        };
        let mut place = Place {
            inflight: self,
            deployment,
            ticket,
            queued: true,
        };
        loop {
            // Registered before the check, so a slot freed in between wakes us.
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut slots = self.slots();
                let queue = slots.entry(deployment.to_owned()).or_default();
                if queue.waiting.front() == Some(&ticket) && self.try_increment(deployment, max) {
                    queue.waiting.pop_front();
                    place.queued = false;
                    // The next in line may fit too.
                    queue.changed.notify_waiters();
                    return Some(self.static_guard(deployment));
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    fn static_guard(self: &Arc<Self>, deployment: &str) -> StaticStreamGuard {
        StaticStreamGuard {
            inflight: self.clone(),
            deployment: deployment.to_string(),
            abandoned: false,
            released: false,
        }
    }

    pub fn current(&self, deployment: &str) -> usize {
        self.counts
            .lock()
            .unwrap()
            .get(deployment)
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(0)
    }
}

/// A request's place in a deployment's slot queue; leaving it (a deadline,
/// a disconnect) lets the next request move up.
struct Place<'a> {
    inflight: &'a InFlight,
    deployment: &'a str,
    ticket: u64,
    queued: bool,
}

impl Drop for Place<'_> {
    fn drop(&mut self) {
        if !self.queued {
            return;
        }
        let mut slots = self.inflight.slots();
        if let Some(queue) = slots.get_mut(self.deployment) {
            queue.waiting.retain(|t| *t != self.ticket);
            queue.changed.notify_waiters();
        }
    }
}

pub fn admit_request(_deps: &RouterDeps, _deployment: &str) -> Result<(), (String, String)> {
    // Per-deployment in-flight bound is enforced by the caller's guard on
    // the shared InFlight registry (Task 8 wires the streaming path). The
    // core bounds (body size) are checked in the router itself.
    Ok(())
}

impl InFlight {
    /// Register one in-flight request. The guard releases on normal
    /// completion (drop) or confirmed cancellation; `abandon()` marks a
    /// client disconnect — accounting is retained until an explicit
    /// release, because a disconnect is not proof the engine stopped
    /// (F1 design §5).
    pub fn guard(&self, deployment: &str) -> StreamGuard<'_> {
        self.try_increment(deployment, usize::MAX);
        StreamGuard {
            inflight: BorrowedInFlight { inner: self },
            deployment: deployment.to_string(),
            abandoned: false,
            released: false,
        }
    }

    /// Register only if the per-deployment in-flight bound allows it
    /// (atomic-conditional — the bound is enforced under the same lock as
    /// the increment). `None` = bound reached; no accounting was taken.
    pub fn try_guard(&self, deployment: &str, max: usize) -> Option<StreamGuard<'_>> {
        if self.try_increment(deployment, max) {
            Some(StreamGuard {
                inflight: BorrowedInFlight { inner: self },
                deployment: deployment.to_string(),
                abandoned: false,
                released: false,
            })
        } else {
            None
        }
    }
}

/// Guard owning its registry: for the streaming pump task (needs 'static).
pub struct StaticStreamGuard {
    inflight: Arc<InFlight>,
    deployment: String,
    abandoned: bool,
    released: bool,
}

impl InFlight {
    pub fn guard_arc(self: &Arc<Self>, deployment: &str) -> StaticStreamGuard {
        self.try_increment(deployment, usize::MAX);
        StaticStreamGuard {
            inflight: self.clone(),
            deployment: deployment.to_string(),
            abandoned: false,
            released: false,
        }
    }

    /// Owned streaming guard with the in-flight bound enforced atomically
    /// (T19): `None` = bound reached, no accounting taken.
    pub fn try_guard_arc(
        self: &Arc<Self>,
        deployment: &str,
        max: usize,
    ) -> Option<StaticStreamGuard> {
        // SPEC §10 (T19): never ahead of a request already waiting for a slot.
        let slots = self.slots();
        if slots.get(deployment).is_some_and(|q| !q.waiting.is_empty()) {
            return None;
        }
        if self.try_increment(deployment, max) {
            Some(StaticStreamGuard {
                inflight: self.clone(),
                deployment: deployment.to_string(),
                abandoned: false,
                released: false,
            })
        } else {
            None
        }
    }
}

impl StaticStreamGuard {
    pub fn abandon(mut self) -> Self {
        self.abandoned = true;
        self
    }
    pub fn release(mut self) {
        self.released = true;
        self.inflight.decrement(&self.deployment);
    }
}

impl Drop for StaticStreamGuard {
    fn drop(&mut self) {
        if !self.released && !self.abandoned {
            self.inflight.decrement(&self.deployment);
        }
    }
}

/// Borrowed guard over one admitted request (owned-path accounting).
pub struct StreamGuard<'a> {
    inflight: BorrowedInFlight<'a>,
    deployment: String,
    abandoned: bool,
    released: bool,
}

struct BorrowedInFlight<'a> {
    inner: &'a InFlight,
}

impl StreamGuard<'_> {
    /// The client left. The engine may still be working: retain accounting
    /// until the backend completes or cancellation is confirmed.
    pub fn abandon(mut self) -> Self {
        self.abandoned = true;
        self
    }

    /// Backend stream completed or cancellation confirmed: release.
    pub fn release(mut self) {
        self.released = true;
        self.inflight.inner.decrement(&self.deployment);
    }
}

impl Drop for StreamGuard<'_> {
    fn drop(&mut self) {
        if !self.released && !self.abandoned {
            self.inflight.inner.decrement(&self.deployment);
        }
        // abandoned guards are released by explicit `release()` after the
        // backend confirms completion; a leak of abandoned guards is
        // bounded by the queue-deadline sweeper (switch engine, Task 11).
    }
}
