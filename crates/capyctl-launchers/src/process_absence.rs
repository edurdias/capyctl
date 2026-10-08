//! Proof that specific processes are gone.
//!
//! Cleanup may release ownership, endpoints and accounting only against evidence
//! that the exact processes it owned no longer exist. Failing to observe a process
//! is not that evidence: an unreadable `/proc`, a restricted namespace or a racing
//! scan all look like absence while the process keeps running and keeps holding
//! device memory. Conflating the two is how a release happens without proof.
//!
//! So this answers three ways, and only one of them authorises a release.
//! `Unknown` is returned whenever the question cannot be settled, and the caller
//! must treat it as retained rather than as absent.
//!
//! A process that has exited but that its parent has not reaped (a zombie) still
//! has its `/proc/<pid>`, and still holds its pid. It is neither alive nor gone:
//! when this role is its parent (`subreaper`), it is reaped here and then reads
//! gone; otherwise it reads `Unknown`, and a recorded set holding one is
//! reported as `Unreaped` rather than read as alive forever (SPEC §13.2, T12).
//!
//! Unlike `ExecLauncher`'s handle check, this works from a recorded identity set
//! rather than a live handle, so it survives the restart that destroys handles.

pub use capyctl_domain::completion::Presence;
use capyctl_domain::completion::ProcessIdentity;

/// The whole recorded set, resolved together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoneProof {
    /// Every recorded process is proven absent.
    AllGone,
    /// At least one is still alive.
    SomeAlive,
    /// No process is known alive, but at least one could not be settled. This is not
    /// a release: it is the uncertainty that has to be reconciled later.
    Indeterminate,
    /// No process is known alive, and at least one recorded process has exited but
    /// its parent, another live process, has not reaped it. Its pid is still held,
    /// so this is not a release either; it ends when that parent waits for it or
    /// exits (the zombie is then handed to this role, which reaps it).
    Unreaped,
}

fn current_boot() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot = raw.trim();
    // A malformed boot id means the question cannot be settled. Never treat an
    // unreadable identity source as a different boot, which would read as Gone.
    (boot.len() == 36
        && boot.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        }))
    .then(|| boot.to_owned())
}

/// What the system reported about one recorded pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// No process directory for this pid.
    NoSuchPid,
    /// A process exists and its start time was read.
    StartedAt(u64),
    /// A process with this start time has exited and waits for its parent to
    /// reap it (a zombie with no live threads).
    Exited(u64),
    /// A process directory exists but its start time could not be read — a race
    /// with exit, or a permission boundary. Deliberately distinct from `NoSuchPid`.
    Unreadable,
}

/// The decision, separated from the reading of it so every branch is reachable in
/// a test. `presence` is the thin I/O wrapper; this holds the rules.
pub fn resolve(identity: &ProcessIdentity, boot: Option<&str>, observed: Observed) -> Presence {
    if identity.pid == 0 || identity.start_ticks == 0 || identity.boot_id.is_empty() {
        return Presence::Unknown;
    }
    let Some(boot) = boot else {
        return Presence::Unknown;
    };
    // A different boot is positive evidence: nothing recorded against the previous
    // boot can still be running, whatever pid numbers currently exist.
    if boot != identity.boot_id {
        return Presence::Gone;
    }
    match observed {
        Observed::NoSuchPid => Presence::Gone,
        // Same pid, different start time: the pid was reused, so the recorded
        // process is gone even though the number is in use again.
        Observed::StartedAt(t) if t != identity.start_ticks => Presence::Gone,
        Observed::StartedAt(_) => Presence::Alive,
        Observed::Exited(t) if t != identity.start_ticks => Presence::Gone,
        // SPEC §13.2: exited, but its pid is held until its parent reaps it.
        // Neither alive nor proven gone.
        Observed::Exited(_) => Presence::Unknown,
        Observed::Unreadable => Presence::Unknown,
    }
}

fn observe(pid: u32) -> Observed {
    if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
        return Observed::NoSuchPid;
    }
    match crate::subreaper::proc_stat(pid) {
        None => Observed::Unreadable,
        Some(stat) if stat.exited() => Observed::Exited(stat.start_ticks),
        Some(stat) => Observed::StartedAt(stat.start_ticks),
    }
}

/// The reading `presence` decides from. An exited process this role inherited
/// is reaped first (T12), so the reading that follows proves it gone.
fn assess(identity: &ProcessIdentity) -> (Presence, Observed) {
    let boot = current_boot();
    let mut observed = observe(identity.pid);
    if observed == Observed::Exited(identity.start_ticks)
        && boot.as_deref() == Some(identity.boot_id.as_str())
        && crate::subreaper::reap_exited(identity.pid, identity.start_ticks)
    {
        observed = observe(identity.pid);
    }
    (resolve(identity, boot.as_deref(), observed), observed)
}

/// Resolve one recorded identity against the running system.
pub fn presence(identity: &ProcessIdentity) -> Presence {
    assess(identity).0
}

/// Whether this exact recorded process has exited and still waits for a parent
/// other than this role to reap it: the case `Unreaped` reports.
pub fn exited_unreaped(identity: &ProcessIdentity) -> bool {
    matches!(
        assess(identity),
        (Presence::Unknown, Observed::Exited(t)) if t == identity.start_ticks
    )
}

/// SPEC §13.2 (W13): whether a process launched on this boot has exited. Only
/// a process recorded against the current boot is judged: one recorded against
/// another boot (or a fabricated identity) is gone by `presence`, but its end is
/// not an exit this host observed, and an unreadable boot or process never is.
pub fn exit_observed(identity: &ProcessIdentity) -> bool {
    current_boot().is_some_and(|boot| boot == identity.boot_id)
        && presence(identity) == Presence::Gone
}

/// Resolve a recorded set. An empty set proves nothing and is `Indeterminate`,
/// because "no recorded processes" is a missing record rather than an observation.
pub fn verify_gone(identities: &[ProcessIdentity]) -> GoneProof {
    if identities.is_empty() {
        return GoneProof::Indeterminate;
    }
    let mut indeterminate = false;
    let mut unreaped = false;
    for identity in identities {
        match assess(identity) {
            (Presence::Alive, _) => return GoneProof::SomeAlive,
            (Presence::Unknown, Observed::Exited(t)) if t == identity.start_ticks => {
                unreaped = true
            }
            (Presence::Unknown, _) => indeterminate = true,
            (Presence::Gone, _) => {}
        }
    }
    if unreaped {
        GoneProof::Unreaped
    } else if indeterminate {
        GoneProof::Indeterminate
    } else {
        GoneProof::AllGone
    }
}

#[cfg(test)]
mod tests;
