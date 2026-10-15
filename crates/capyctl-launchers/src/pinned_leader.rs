//! SPEC §13.2, ADR 0028 §11: a launch's leader is reaped only once nothing of
//! its process group still runs, so the group stays nameable after it dies.
//!
//! An engine's workers run in the process group its spawned (API) process
//! leads, and the group id is that process's pid. When the leader dies first
//! (a SIGKILLed vLLM group worker, found live in MN5) its workers keep running
//! and keep holding device memory, but they were not always recorded: a group
//! worker's tree is recorded right after its spawn, before the engine forks
//! them. A leaderless group could only be named by its id, and once the
//! leader is reaped its pid can be handed to an unrelated process that leads
//! a group of its own, so such a scan never authorized a signal and the stop
//! stayed uncertain for as long as the orphans ran.
//!
//! The kernel keeps a pid allocated while a process holds it, a zombie
//! included, and a zombie stays a member of its process group until it is
//! reaped. So the launcher that spawned the leader, its only reaper, holds it
//! here from its spawn until its exit has been observed (without reaping,
//! `WNOWAIT`) and no other process of its group runs; only then does it reap.
//! While the leader is held, its pid, and so its group id, can only name this
//! launch's group, and [`signal_group`] may signal that whole group, the
//! processes never recorded included. The hold and the reap share one lock, so
//! no signal is sent between the reap and a reuse of the id.
//!
//! A held leader whose exit was observed is gone ([`exited_and_held`] makes
//! `presence` read it so): nothing of it runs, and its exit is reported as
//! before, with the status the launcher's wait observed. A launch whose leader
//! this process did not spawn (one launched before a restart of this role) is
//! not held, and its leaderless group still never authorizes a signal.

use std::sync::{Mutex, PoisonError};

use capyctl_domain::completion::ProcessIdentity;

/// How often a held leader's group is checked for processes still running.
const GROUP_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// The leaders this process spawned and has not reaped yet, each with whether
/// its exit has been observed (and its status recorded).
static HELD: Mutex<Vec<(ProcessIdentity, bool)>> = Mutex::new(Vec::new());

fn same(a: &ProcessIdentity, b: &ProcessIdentity) -> bool {
    a.pid == b.pid && a.boot_id == b.boot_id && a.start_ticks == b.start_ticks
}

/// Hold `leader`, a child of this process its caller will reap through
/// [`reap_when_group_ends`].
pub(crate) fn hold(leader: &ProcessIdentity) {
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((leader.clone(), false));
}

/// Whether this exact process (pid, boot and start ticks) is a leader this
/// process spawned and still holds unreaped, alive or exited.
pub(crate) fn is_held(identity: &ProcessIdentity) -> bool {
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|(held, _)| same(held, identity))
}

/// Whether this exact process is a held leader whose exit its launcher has
/// observed: a zombie kept only for its group's sake, which runs nothing.
pub(crate) fn exited_and_held(identity: &ProcessIdentity) -> bool {
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|(held, exited)| *exited && same(held, identity))
}

/// Signal the whole process group `leader` leads when this process still
/// holds it: `None` when it does not (nothing is signalled), otherwise the
/// signal's own result. The hold is kept across the signal, so the group id
/// cannot have been handed out again.
pub(crate) fn signal_group(
    leader: &ProcessIdentity,
    signal: nix::sys::signal::Signal,
) -> Option<nix::Result<()>> {
    let held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
    if !held.iter().any(|(h, _)| same(h, leader)) {
        return None;
    }
    let group = nix::unistd::Pid::from_raw(i32::try_from(leader.pid).ok()?);
    Some(nix::sys::signal::killpg(group, signal))
}

/// Wait for the held `leader` to exit without reaping it, report its status
/// through `exited`, then keep holding it until no other process of its group
/// is running, and reap it. Blocks; the caller runs it on its own thread.
pub(crate) fn reap_when_group_ends(
    leader: &ProcessIdentity,
    child: &mut std::process::Child,
    exited: impl FnOnce(nix::sys::wait::WaitStatus),
) {
    use nix::sys::wait::{waitid, Id, WaitPidFlag};
    let pid = nix::unistd::Pid::from_raw(leader.pid as i32);
    let observed = loop {
        match waitid(Id::Pid(pid), WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT) {
            Err(nix::errno::Errno::EINTR) => continue,
            other => break other,
        }
    };
    if let Ok(status) = observed {
        exited(status);
        for (held, exited) in HELD
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter_mut()
        {
            if same(held, leader) {
                *exited = true;
            }
        }
        // A scan that cannot be read keeps the hold: holding a zombie costs
        // only its pid, reaping it early loses the group's name.
        while group_runs_besides(leader) {
            std::thread::sleep(GROUP_POLL);
        }
        // The leader is a zombie: this wait returns at once, under the lock.
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        held.retain(|(h, _)| !same(h, leader));
        let _ = child.wait();
        return;
    }
    // Its exit could not be observed: stop holding it before the ordinary
    // wait, which never runs under the lock.
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(h, _)| !same(h, leader));
    let _ = child.wait();
}

/// Whether a process other than `leader` still runs in the group it leads.
/// An exited process (a zombie its parent has not reaped) runs nothing.
fn group_runs_besides(leader: &ProcessIdentity) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != leader.pid)
        .filter_map(crate::subreaper::proc_stat)
        .any(|stat| stat.pgrp == leader.pid && !stat.exited())
}
