//! Child tasks owned by a background supervisor.
//!
//! The readiness and checkpoint-digest supervisors start one task per probe or
//! measurement. Those tasks hold the supervisor, and through it the
//! coordinator's owned state and its process lock. Spawned detached, they
//! outlived an aborted supervisor, so a role that shut down could keep probing
//! hosts and keep the process lock after it had returned. Every child is now
//! tracked here: dropping (aborting) the supervisor's loop aborts them, and a
//! cancelled loop aborts and joins them before it returns.
use std::sync::Mutex;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Default)]
pub(crate) struct Children(Mutex<Vec<JoinHandle<()>>>);

impl Children {
    /// Track one child, forgetting the children that already finished.
    pub(crate) fn track(&self, child: JoinHandle<()>) {
        let mut children = self.0.lock().unwrap_or_else(|error| error.into_inner());
        children.retain(|child| !child.is_finished());
        children.push(child);
    }

    /// How many children are still running.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        let mut children = self.0.lock().unwrap_or_else(|error| error.into_inner());
        children.retain(|child| !child.is_finished());
        children.len()
    }

    pub(crate) fn abort_all(&self) {
        for child in self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            child.abort();
        }
    }

    /// Wait up to `bound` for every child to end on its own, then abort the
    /// rest and wait until each has exited. Returns how many were aborted.
    /// For children that end on a cancel signal sent before this is called,
    /// so an effect in progress (a blocking store pass, a hook) completes
    /// instead of being cut off mid-way.
    pub(crate) async fn join_within(&self, bound: std::time::Duration) -> usize {
        let children =
            std::mem::take(&mut *self.0.lock().unwrap_or_else(|error| error.into_inner()));
        let deadline = tokio::time::Instant::now() + bound;
        let mut aborted = 0;
        for mut child in children {
            if tokio::time::timeout_at(deadline, &mut child).await.is_err() {
                child.abort();
                let _ = child.await;
                aborted += 1;
            }
        }
        aborted
    }

    /// Abort every child and wait until each has exited.
    pub(crate) async fn join_all(&self) {
        let children =
            std::mem::take(&mut *self.0.lock().unwrap_or_else(|error| error.into_inner()));
        for child in &children {
            child.abort();
        }
        for child in children {
            let _ = child.await;
        }
    }
}

/// Runs its closure when dropped: an aborted supervisor loop aborts its
/// children on the way out.
pub(crate) struct OnDrop<F: FnMut()>(pub(crate) F);
impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// A cancel signal that never fires, for a supervisor ended only by abort.
pub(crate) fn never() -> watch::Receiver<bool> {
    watch::channel(false).1
}

/// Resolves once `cancel` reads true. A signal whose sender is gone never
/// fires: the supervisor then ends only by abort, as before.
pub(crate) async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // T33 (ADR 0015 invariant 6): a tracked child that ends on its own inside
    // the bound is awaited, not cut off; one still running at the bound is
    // aborted and awaited, so nothing tracked outlives the join.
    #[tokio::test]
    async fn join_within_awaits_finishing_children_and_aborts_the_rest() {
        let children = Children::default();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        children.track(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            done.store(true, Ordering::SeqCst);
        }));
        let stuck = tokio::spawn(std::future::pending::<()>());
        let stuck_handle = stuck.abort_handle();
        children.track(stuck);
        assert_eq!(children.join_within(Duration::from_millis(300)).await, 1);
        assert!(finished.load(Ordering::SeqCst));
        assert!(stuck_handle.is_finished());
        assert_eq!(children.running(), 0);
    }
}
