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
