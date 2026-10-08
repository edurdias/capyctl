//! SPEC §13.2 / T12: a role reaps the processes it inherits.
//!
//! When an engine process exits, the kernel hands its children (workers, compile
//! pools) to the nearest child subreaper above them, or to PID 1. Under a service
//! manager that is the manager, which waits for them. In a container where
//! CapyCTL is PID 1 and no init runs, they come to CapyCTL, and one that exits is
//! a zombie until its parent waits for it: its `/proc/<pid>` stays, so it holds
//! its pid and its process group, and verified cleanup can never complete.
//!
//! So a role makes itself a child subreaper at start ([`start`]): the orphans of
//! its engines then come to it whatever runs above it (an init, a shell, a
//! container runtime), and it reaps every exited child it did not spawn itself,
//! once a second and whenever `process_absence` meets one.
//!
//! The children it did spawn keep their own waiters (`Child::wait` in their
//! spawners); reaping one of those would take its exit status and make that wait
//! fail. They are never touched here:
//!
//! - a child in CapyCTL's own process group: every `Command` that does not move
//!   its child into another group;
//! - a child started through [`spawn_direct`]: every spawn that does, which is
//!   every engine launch.
//!
//! An exited child is a zombie, and only its parent can reap it, so its pid
//! cannot be reused before the wait below: the identity read from its `stat` is
//! the process that wait reaps.
//!
//! The cost of the first rule: an orphan left in CapyCTL's own process group (a
//! grandchild of a plain `Command`) is never reaped. None of CapyCTL's own tools
//! leaves one; every engine runs in a group of its own.

use std::collections::VecDeque;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError, RwLock};
use std::time::Duration;

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

/// How often the inherited children are swept.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// SPEC §17: bounded memory for the reaping failures already reported.
const MAX_REPORTED: usize = 64;

/// Whether this process reaps what it inherits ([`start`] succeeded).
static ACTIVE: AtomicBool = AtomicBool::new(false);
static STARTED: LazyLock<Result<Supervision, String>> = LazyLock::new(arm);
/// Held shared while a direct child is spawned and registered, exclusively while
/// children are reaped: a child cannot be reaped between its spawn and its
/// registration, including the wait `Command::spawn` itself does after a failed
/// `exec`.
static GATE: RwLock<()> = RwLock::new(());
/// Direct children in a process group of their own: pid and start ticks (`None`
/// when they could not be read, which matches the pid until it is gone).
static DIRECT: Mutex<Vec<(u32, Option<u64>)>> = Mutex::new(Vec::new());
static REPORTED: Mutex<VecDeque<(u32, u64)>> = Mutex::new(VecDeque::new());

/// Why the orphans of this role's engines come to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Supervision {
    /// PID 1 of its PID namespace (a container without an init): every orphan in
    /// the namespace is handed to it.
    Init,
    /// A child subreaper: the orphans among its own descendants are handed to it.
    Subreaper,
}

/// SPEC §13.2 / T12: become a child subreaper and start reaping what is
/// inherited. Called once at role start; later calls answer the first result.
/// PID 1 reaps even if the subreaper flag cannot be set, since its orphans
/// come to it anyway.
pub fn start() -> Result<Supervision, String> {
    STARTED.clone()
}

fn arm() -> Result<Supervision, String> {
    let flagged = nix::sys::prctl::set_child_subreaper(true);
    let supervision = if std::process::id() == 1 {
        Supervision::Init
    } else {
        flagged.map_err(|error| format!("PR_SET_CHILD_SUBREAPER failed: {error}"))?;
        Supervision::Subreaper
    };
    ACTIVE.store(true, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("capyctl-reaper".into())
        .spawn(|| loop {
            reap_orphans();
            std::thread::sleep(SWEEP_INTERVAL);
        })
        .map_err(|error| format!("the reaper thread did not start: {error}"))?;
    Ok(supervision)
}

/// Whether [`start`] armed reaping in this process.
pub fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Spawn a child that leaves CapyCTL's process group, registered so the reaper
/// never waits for it: its spawner does.
pub fn spawn_direct(command: &mut Command) -> std::io::Result<Child> {
    let _gate = GATE.read().unwrap_or_else(PoisonError::into_inner);
    let child = command.spawn()?;
    let start_ticks = proc_stat(child.id()).map(|stat| stat.start_ticks);
    DIRECT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((child.id(), start_ticks));
    Ok(child)
}

/// Reap every exited child this process inherited; answers how many. Nothing
/// happens before [`start`]: a process that is neither PID 1 nor a subreaper
/// inherits nothing, and a test process must not reap its siblings' children.
pub fn reap_orphans() -> usize {
    if !active() {
        return 0;
    }
    let _gate = GATE.write().unwrap_or_else(PoisonError::into_inner);
    DIRECT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|&(pid, recorded)| {
            proc_stat(pid).is_some_and(|stat| recorded.is_none_or(|t| t == stat.start_ticks))
        });
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let me = std::process::id();
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            proc_stat(pid).is_some_and(|stat| {
                stat.ppid == me && stat.exited() && reap_locked(pid, &stat) == Reap::Reaped
            })
        })
        .count()
}

/// Reap `pid` if it is an exited child of this process with these start ticks
/// that its spawner does not wait for. Answers whether it was reaped.
pub(crate) fn reap_exited(pid: u32, start_ticks: u64) -> bool {
    if !active() {
        return false;
    }
    let _gate = GATE.write().unwrap_or_else(PoisonError::into_inner);
    match proc_stat(pid) {
        Some(stat) if stat.start_ticks == start_ticks && stat.exited() => {
            reap_locked(pid, &stat) == Reap::Reaped
        }
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Reap {
    Reaped,
    /// Another process is its parent: only that process can reap it.
    NotOurs,
    /// Spawned here; its spawner waits for it.
    Spawned,
    Failed,
}

/// Called with [`GATE`] held exclusively and `stat` read for an exited process.
fn reap_locked(pid: u32, stat: &ProcStat) -> Reap {
    if stat.ppid != std::process::id() {
        return Reap::NotOurs;
    }
    let own_group = nix::unistd::getpgrp().as_raw();
    let direct = DIRECT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|&(p, t)| p == pid && t.is_none_or(|t| t == stat.start_ticks));
    if i64::from(stat.pgrp) == i64::from(own_group) || direct {
        return Reap::Spawned;
    }
    let Ok(raw) = i32::try_from(pid) else {
        return Reap::Failed;
    };
    match waitpid(
        nix::unistd::Pid::from_raw(raw),
        Some(WaitPidFlag::WNOHANG | WaitPidFlag::__WALL),
    ) {
        Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => Reap::Reaped,
        outcome => {
            report(pid, stat.start_ticks, &format!("{outcome:?}"));
            Reap::Failed
        }
    }
}

/// Say once per process that an inherited zombie could not be reaped.
fn report(pid: u32, start_ticks: u64, outcome: &str) {
    let mut reported = REPORTED.lock().unwrap_or_else(PoisonError::into_inner);
    if reported.contains(&(pid, start_ticks)) {
        return;
    }
    if reported.len() == MAX_REPORTED {
        reported.pop_front();
    }
    reported.push_back((pid, start_ticks));
    capyctl_domain::role_log::notice(
        capyctl_domain::role_log::Level::Warning,
        &format!("Could not reap exited process {pid} inherited by this role ({outcome})."),
    );
}

/// The fields of `/proc/<pid>/stat` the reaper and absence proofs read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcStat {
    pub state: char,
    pub ppid: u32,
    pub pgrp: u32,
    pub threads: u64,
    pub start_ticks: u64,
}

impl ProcStat {
    /// The process has exited and waits only to be reaped. A zombie thread-group
    /// leader whose other threads still run is not exited: the process lives on.
    pub(crate) fn exited(&self) -> bool {
        matches!(self.state, 'Z' | 'X' | 'x') && self.threads <= 1
    }
}

/// Fields 3, 4, 5, 20 and 22, counted after the command name, which may itself
/// contain spaces and parentheses; split after its final ')'.
pub(crate) fn parse_stat(raw: &str) -> Option<ProcStat> {
    let close = raw.rfind(')')?;
    let fields: Vec<&str> = raw.get(close + 2..)?.split_whitespace().collect();
    let mut state = fields.first()?.chars();
    let stat = ProcStat {
        state: state.next()?,
        ppid: fields.get(1)?.parse().ok()?,
        pgrp: fields.get(2)?.parse().ok()?,
        threads: fields.get(17)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
    };
    state.next().is_none().then_some(stat)
}

pub(crate) fn proc_stat(pid: u32) -> Option<ProcStat> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(test)]
mod tests;
