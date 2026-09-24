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
//! Unlike `ExecLauncher`'s handle check, this works from a recorded identity set
//! rather than a live handle, so it survives the restart that destroys handles.

use mllm_domain::completion::ProcessIdentity;
pub use mllm_domain::completion::Presence;

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

fn start_ticks(pid: u32) -> Option<u64> {
    // Field 22 of /proc/<pid>/stat, counted after the comm field, which may itself
    // contain spaces and parentheses; split after its final ')'.
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    raw[close + 2..]
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
}

/// What the system reported about one recorded pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// No process directory for this pid.
    NoSuchPid,
    /// A process exists and its start time was read.
    StartedAt(u64),
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
        Observed::Unreadable => Presence::Unknown,
    }
}

/// Resolve one recorded identity against the running system.
pub fn presence(identity: &ProcessIdentity) -> Presence {
    let observed = if std::path::Path::new(&format!("/proc/{}", identity.pid)).exists() {
        start_ticks(identity.pid).map_or(Observed::Unreadable, Observed::StartedAt)
    } else {
        Observed::NoSuchPid
    };
    resolve(identity, current_boot().as_deref(), observed)
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
    for identity in identities {
        match presence(identity) {
            Presence::Alive => return GoneProof::SomeAlive,
            Presence::Unknown => indeterminate = true,
            Presence::Gone => {}
        }
    }
    if indeterminate {
        GoneProof::Indeterminate
    } else {
        GoneProof::AllGone
    }
}

#[cfg(test)]
mod tests;
