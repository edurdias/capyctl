//! SPEC §10 step 1 (W10): requests waiting for a deployment to become
//! servable are bounded by count, per deployment and in total, and by the
//! bytes their bodies hold, and each waits under a deadline.
//!
//! A waiting request holds a [`WaitTicket`]. Dropping it — the request was
//! dispatched, refused, timed out or its client disconnected — returns its
//! slot and bytes at once. A disconnect never cancels the activation the
//! request joined: that runs as its own task (see `WakeJoin::join_detached`),
//! so the other waiting requests and the deployment's lifecycle are
//! unaffected. Nothing is buffered for a response while waiting, and no
//! token or success is invented for a waiting client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bounds on waiting requests. Defaults are the host queue policy's defaults
/// (`resource_policy.queue`, SPEC §16.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitLimits {
    pub max_pending_per_deployment: usize,
    pub max_pending_total: usize,
    pub max_buffered_bytes_total: usize,
    /// How long one request waits for its deployment, activation included.
    /// SPEC §10: also the bound on a relayed stream's first backend event,
    /// counted from when the request arrived.
    pub deadline: Duration,
    /// SPEC §10: once a stream has produced an event, the longest it may go
    /// without another. A stream that keeps producing is never cut.
    pub stream_idle: Duration,
}

impl Default for WaitLimits {
    fn default() -> Self {
        Self {
            max_pending_per_deployment: 64,
            max_pending_total: 256,
            max_buffered_bytes_total: 64 << 20,
            deadline: Duration::from_secs(600),
            // The host queue policy's default (`DEFAULT_STREAM_IDLE_MS`).
            stream_idle: Duration::from_secs(120),
        }
    }
}

/// Why a request could not wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitRefusal {
    /// The deployment's or the router's waiting count is at its bound.
    Full,
    /// The waiting bodies would exceed the buffered-bytes bound.
    Bytes,
}

#[derive(Default)]
struct Counts {
    per: HashMap<String, usize>,
    total: usize,
    bytes: usize,
}

/// The router's waiting requests.
pub struct WaitQueue {
    limits: Mutex<WaitLimits>,
    counts: Mutex<Counts>,
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new(WaitLimits::default())
    }
}

impl WaitQueue {
    pub fn new(limits: WaitLimits) -> Self {
        Self {
            limits: Mutex::new(limits),
            counts: Mutex::new(Counts::default()),
        }
    }

    pub fn limits(&self) -> WaitLimits {
        self.limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn set_limits(&self, limits: WaitLimits) {
        *self
            .limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = limits;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Counts> {
        // Plain counters: a panic elsewhere leaves nothing half-written.
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Requests waiting for `deployment` now.
    pub fn waiting(&self, deployment: &str) -> usize {
        self.lock().per.get(deployment).copied().unwrap_or(0)
    }

    /// Requests waiting in total, and the bytes their bodies hold.
    pub fn totals(&self) -> (usize, usize) {
        let counts = self.lock();
        (counts.total, counts.bytes)
    }

    /// Admit one waiting request of `bytes`, checking and counting under one
    /// lock so concurrent arrivals never over-admit.
    pub fn enter(
        self: &Arc<Self>,
        deployment: &str,
        bytes: usize,
    ) -> Result<WaitTicket, WaitRefusal> {
        let limits = self.limits();
        let mut counts = self.lock();
        let here = counts.per.get(deployment).copied().unwrap_or(0);
        if here >= limits.max_pending_per_deployment || counts.total >= limits.max_pending_total {
            return Err(WaitRefusal::Full);
        }
        if counts
            .bytes
            .checked_add(bytes)
            .is_none_or(|b| b > limits.max_buffered_bytes_total)
        {
            return Err(WaitRefusal::Bytes);
        }
        *counts.per.entry(deployment.to_owned()).or_default() += 1;
        counts.total += 1;
        counts.bytes += bytes;
        Ok(WaitTicket {
            queue: self.clone(),
            deployment: deployment.to_owned(),
            bytes,
        })
    }
}

/// One waiting request's slot; released on drop.
pub struct WaitTicket {
    queue: Arc<WaitQueue>,
    deployment: String,
    bytes: usize,
}

impl Drop for WaitTicket {
    fn drop(&mut self) {
        let mut counts = self.queue.lock();
        if let Some(n) = counts.per.get_mut(&self.deployment) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per.remove(&self.deployment);
            }
        }
        counts.total = counts.total.saturating_sub(1);
        counts.bytes = counts.bytes.saturating_sub(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T19: count and byte bounds, released on drop (a disconnect included).
    #[test]
    fn waiting_is_bounded_by_count_and_bytes_and_released_on_drop() {
        let q = Arc::new(WaitQueue::new(WaitLimits {
            max_pending_per_deployment: 2,
            max_pending_total: 3,
            max_buffered_bytes_total: 100,
            deadline: Duration::from_secs(1),
            stream_idle: Duration::from_secs(1),
        }));
        let a1 = q.enter("a", 10).unwrap();
        let _a2 = q.enter("a", 10).unwrap();
        assert_eq!(q.enter("a", 1).err(), Some(WaitRefusal::Full));
        let _b1 = q.enter("b", 10).unwrap();
        assert_eq!(
            q.enter("c", 1).err(),
            Some(WaitRefusal::Full),
            "total bound"
        );
        drop(a1);
        assert_eq!(q.enter("c", 91).err(), Some(WaitRefusal::Bytes));
        let _c = q.enter("c", 80).unwrap();
        assert_eq!(q.totals(), (3, 100));
        assert_eq!(q.waiting("a"), 1);
    }
}
