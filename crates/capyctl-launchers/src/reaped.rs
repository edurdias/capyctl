//! How a launched engine ended, as its launcher observed it (SPEC §§5, 13.2).
//!
//! The launcher that spawned an engine is its parent and reaps it, so it is the
//! only party that can know the engine's exit code or terminating signal. The
//! reaper records that status here, keyed by the exact process identity (PID,
//! boot and start ticks) the launch recorded, so an exit report can say how the
//! engine ended. A PID alone is never a key: a reused PID must not inherit
//! another process's status.
//!
//! This is evidence for the operator only. Absence of a process is decided from
//! `/proc` (`process_absence`), never from this table, and nothing is released
//! because a status was recorded.

use std::collections::VecDeque;
use std::sync::Mutex;

use capyctl_domain::completion::ProcessIdentity;

/// SPEC §17: bounded memory. Older statuses are dropped first; a status that is
/// no longer held is reported as unobserved, never guessed.
const MAX_REAPED: usize = 256;

/// How one reaped child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapedStatus {
    /// It exited with this code.
    Code(i32),
    /// A signal ended it.
    Signal(i32),
}

static REAPED: Mutex<VecDeque<(ProcessIdentity, ReapedStatus)>> = Mutex::new(VecDeque::new());

/// Record how the child with exactly this identity ended.
pub(crate) fn record(identity: &ProcessIdentity, status: std::process::ExitStatus) {
    use std::os::unix::process::ExitStatusExt;
    let status = match (status.code(), status.signal()) {
        (Some(code), _) => ReapedStatus::Code(code),
        (None, Some(signal)) => ReapedStatus::Signal(signal),
        (None, None) => return,
    };
    let Ok(mut reaped) = REAPED.lock() else {
        return;
    };
    if reaped.len() >= MAX_REAPED {
        reaped.pop_front();
    }
    reaped.push_back((identity.clone(), status));
}

/// How the process with exactly this identity ended, if this process's launcher
/// spawned and reaped it. `None` for anything else (a worker the engine forked,
/// an engine a previous agent run launched, or a status no longer held).
pub fn status_of(identity: &ProcessIdentity) -> Option<ReapedStatus> {
    REAPED
        .lock()
        .ok()?
        .iter()
        .rev()
        .find(|(reaped, _)| reaped == identity)
        .map(|(_, status)| *status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(pid: u32, ticks: u64) -> ProcessIdentity {
        ProcessIdentity {
            role: "api".into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: ticks,
        }
    }

    // T33: a status is keyed by the whole identity, never a PID alone.
    #[test]
    fn a_status_is_found_only_for_the_exact_identity() {
        use std::os::unix::process::ExitStatusExt;
        record(
            &identity(4_000_001, 7),
            std::process::ExitStatus::from_raw(9),
        );
        record(
            &identity(4_000_002, 8),
            std::process::ExitStatus::from_raw(3 << 8),
        );
        assert_eq!(
            status_of(&identity(4_000_001, 7)),
            Some(ReapedStatus::Signal(9))
        );
        assert_eq!(
            status_of(&identity(4_000_002, 8)),
            Some(ReapedStatus::Code(3))
        );
        assert_eq!(status_of(&identity(4_000_001, 99)), None, "a reused pid");
    }
}
