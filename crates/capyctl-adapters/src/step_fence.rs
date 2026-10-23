//! SPEC §13.2, T20: the once-only guard of a launch's residency steps.
//!
//! An adapter runs each persisted step id at most once, so a step whose
//! dispatch is repeated (a retry of the same step) never reaches the engine
//! twice. The guard remembers the most recent [`RECENT_STEPS`] step ids, not
//! every id of the launch's life: a launch parks and wakes for as long as it
//! runs, and a lifetime bound refused the steps of a healthy launch once it had
//! parked often enough (found live 2026-10-23: a vLLM launch's 13th wake,
//! 5 steps per park and wake, was refused at its probe by a 64-id bound and
//! the wake read uncertain). Step ids are the store's own, unique per step, and
//! a step is dispatched only while its operation is armed, so a repeat can
//! only be of a step of the current or a recent operation; the store's step
//! state stays the durable guard (a restarted controller builds a new adapter
//! and an empty guard).

use std::collections::{HashSet, VecDeque};

/// How many recent step ids are remembered: a park and its wake are 5 steps
/// on vLLM, so this covers a dozen operations.
pub const RECENT_STEPS: usize = 64;

/// The step ids admitted most recently, oldest first.
#[derive(Debug, Default)]
pub struct RecentSteps {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl RecentSteps {
    /// Admit `step_id` once: `false` when it is among the recent ids. Admitting
    /// a new id past [`RECENT_STEPS`] forgets the oldest.
    pub fn admit(&mut self, step_id: &str) -> bool {
        if self.seen.contains(step_id) {
            return false;
        }
        if self.order.len() == RECENT_STEPS {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.order.push_back(step_id.to_owned());
        self.seen.insert(step_id.to_owned());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T20: a repeat of a recent step is refused however long the launch has
    // run; a launch that parks and wakes many times keeps admitting new steps.
    #[test]
    fn new_steps_are_admitted_for_the_launch_life_and_recent_repeats_refused() {
        let mut steps = RecentSteps::default();
        for n in 0..10 * RECENT_STEPS {
            let id = format!("step-{n}");
            assert!(steps.admit(&id), "{id}");
            assert!(!steps.admit(&id), "{id} repeated");
        }
        assert_eq!(steps.order.len(), RECENT_STEPS);
        assert_eq!(steps.seen.len(), RECENT_STEPS);
        let last = 10 * RECENT_STEPS - 1;
        assert!(!steps.admit(&format!("step-{}", last + 1 - RECENT_STEPS)));
    }
}
