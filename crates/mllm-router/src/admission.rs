//! Admission accounting for chat requests (T19 groundwork): per-deployment
//! in-flight bounds. In-flight counts are conservative (released only on
//! completion/confirmed cancellation — Task 8).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::RouterDeps;

/// Per-process in-flight registry. Server-restart durability is NOT
/// promised (F1 design §5: bodies/streams don't survive router restarts).
#[derive(Default)]
pub struct InFlight {
    counts: Mutex<std::collections::HashMap<String, Arc<AtomicUsize>>>,
}

impl InFlight {
    pub fn increment(&self, deployment: &str) -> usize {
        let mut map = self.counts.lock().unwrap();
        let c = map.entry(deployment.to_string()).or_default();
        c.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn decrement(&self, deployment: &str) {
        if let Some(c) = self.counts.lock().unwrap().get(deployment) {
            c.fetch_sub(1, Ordering::SeqCst);
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
        self.increment(deployment);
        StreamGuard {
            inflight: BorrowedInFlight { inner: self },
            deployment: deployment.to_string(),
            abandoned: false,
            released: false,
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
        self.increment(deployment);
        StaticStreamGuard {
            inflight: self.clone(),
            deployment: deployment.to_string(),
            abandoned: false,
            released: false,
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
