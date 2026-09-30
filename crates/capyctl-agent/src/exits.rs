//! SPEC §13.2 (W13, G08): exits of owned engine processes, observed by the host.
//!
//! A Ready engine that dies without a Terminate is found here, not at the
//! controller's next probe. Every Ready launch this host claims is watched by the
//! exact group its native readiness recorded (PID, boot and start ticks). A
//! member of that group that is gone on this boot is an exit: it is journaled
//! (with the exit code or signal when this host's launcher reaped it) and
//! reported to the controller as a `MemberExit`, which closes that instance's
//! dispatch and drives its verified cleanup.
//!
//! Nothing here releases a claim, terminates a process or restarts an engine.
//! The claim stays until an authenticated Terminate proves the whole recorded
//! group gone (SPEC §13.2); an exit report is not that proof.
use crate::journal::HostJournal;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_protocol::execution::MemberCommand;
use capyctl_protocol::reports::{ExitStatus, MemberExit};
use std::sync::Arc;
use std::time::Duration;

/// How often Ready groups are checked. An exit reaches the controller within
/// about one period plus the session's delivery (the W13 target is ~1 s).
pub const EXIT_SCAN_INTERVAL: Duration = Duration::from_millis(250);
/// A reported exit is reported again this often while its launch is still
/// claimed Ready, so a report lost with a session, or one the controller could
/// not act on yet, is not lost for good. The controller treats repeats as one.
pub const EXIT_RESEND_INTERVAL: Duration = Duration::from_secs(5);

/// One Ready launch with an exited member.
pub struct ExitedLaunch {
    /// The retained launch command (its owned handle is the command id).
    pub command: MemberCommand,
    pub exit: MemberExit,
}

/// Every Ready or parked launch this host claims that has a member exited on
/// this boot (a parked one is reported only; nothing changes its residency),
/// with that exit journaled. The API process is named first when it exited,
/// because it is the one this host's launcher reaps and so knows the status of.
pub fn scan(journal: &Arc<HostJournal>, host_id: &str, now_ms: i64) -> Vec<ExitedLaunch> {
    let Ok(ready) = journal.ready_launches() else {
        return Vec::new();
    };
    let mut exited = Vec::new();
    for (command, mut group) in ready {
        group.sort_by_key(|p| (p.role != "api", p.role.clone(), p.pid));
        let Some(process) = group
            .iter()
            .find(|p| capyctl_launchers::process_absence::exit_observed(p))
        else {
            continue;
        };
        let handle = command.identity.command_id.clone();
        let Ok((code, signal, observed_at_ms)) =
            journal.record_member_exit(&handle, process, reaped(process), now_ms)
        else {
            continue;
        };
        let status = match (code, signal) {
            (Some(code), None) => ExitStatus::Code(code),
            (None, Some(signal)) if (1..=64).contains(&signal) => ExitStatus::Signal(signal),
            _ => ExitStatus::Unobserved,
        };
        exited.push(ExitedLaunch {
            exit: MemberExit {
                host_id: host_id.to_owned(),
                deployment_id: command.identity.deployment_id.clone(),
                generation: command.identity.generation,
                owned_handle: handle,
                process: process.clone(),
                status,
                observed_at_ms,
            },
            command,
        });
    }
    exited
}

/// How the process ended, when this host's launcher reaped it.
fn reaped(process: &ProcessIdentity) -> (Option<i32>, Option<i32>) {
    match capyctl_launchers::reaped::status_of(process) {
        Some(capyctl_launchers::reaped::ReapedStatus::Code(code)) => (Some(code), None),
        Some(capyctl_launchers::reaped::ReapedStatus::Signal(signal)) => (None, Some(signal)),
        None => (None, None),
    }
}
