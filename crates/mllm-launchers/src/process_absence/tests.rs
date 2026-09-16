use super::*;

fn boot() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_owned()
}

fn identity(pid: u32, start_ticks: u64, boot_id: &str) -> ProcessIdentity {
    ProcessIdentity {
        role: "api".into(),
        pid,
        boot_id: boot_id.into(),
        start_ticks,
    }
}

/// This process is the one identity we can name with certainty.
fn own_identity() -> ProcessIdentity {
    let pid = std::process::id();
    identity(
        pid,
        start_ticks(pid).expect("own stat is readable"),
        &boot(),
    )
}

#[test]
fn a_running_process_reads_as_alive() {
    assert_eq!(presence(&own_identity()), Presence::Alive);
    assert_eq!(verify_gone(&[own_identity()]), GoneProof::SomeAlive);
}

/// A reused pid is the case a bare existence check gets wrong: the number is in
/// use, but not by the process that was recorded.
#[test]
fn a_reused_pid_is_gone_not_alive() {
    let mut reused = own_identity();
    reused.start_ticks += 1;
    assert_eq!(presence(&reused), Presence::Gone);
}

#[test]
fn an_unused_pid_is_gone() {
    // Above the pid range any live process can occupy.
    let absent = identity(0x7FFF_FFF0, 1, &boot());
    assert_eq!(presence(&absent), Presence::Gone);
    assert_eq!(verify_gone(&[absent]), GoneProof::AllGone);
}

/// Nothing recorded against a previous boot can still be running, whatever pid
/// numbers exist now.
#[test]
fn a_process_from_another_boot_is_gone() {
    let previous = identity(
        std::process::id(),
        1,
        "00000000-0000-0000-0000-000000000000",
    );
    assert_eq!(presence(&previous), Presence::Gone);
}

/// An incomplete record cannot be resolved, and must never read as absent — that
/// would release ownership against a missing field rather than an observation.
#[test]
fn an_incomplete_identity_is_unknown_not_gone() {
    let b = boot();
    for broken in [
        identity(0, 12, &b),
        identity(41, 0, &b),
        identity(41, 12, ""),
    ] {
        assert_eq!(presence(&broken), Presence::Unknown, "{broken:?}");
    }
}

/// One live member denies the whole set, even alongside proven-absent members.
#[test]
fn one_live_member_denies_the_set() {
    let set = vec![identity(0x7FFF_FFF0, 1, &boot()), own_identity()];
    assert_eq!(verify_gone(&set), GoneProof::SomeAlive);
}

/// An unresolvable member downgrades the set to indeterminate rather than letting
/// its proven-absent siblings authorise a release.
#[test]
fn an_unresolvable_member_downgrades_the_set() {
    let set = vec![
        identity(0x7FFF_FFF0, 1, &boot()),
        identity(0, 0, &boot()), // unresolvable
    ];
    assert_eq!(verify_gone(&set), GoneProof::Indeterminate);
}

/// An empty set is a missing record, not an observation of absence.
#[test]
fn an_empty_set_proves_nothing() {
    assert_eq!(verify_gone(&[]), GoneProof::Indeterminate);
}

/// Every member proven absent is the only shape that authorises release.
#[test]
fn only_a_fully_proven_set_authorises_release() {
    let b = boot();
    let set = vec![
        identity(0x7FFF_FFF0, 1, &b),
        identity(0x7FFF_FFF1, 2, &b),
        identity(
            std::process::id(),
            1,
            "00000000-0000-0000-0000-000000000000",
        ),
    ];
    assert_eq!(verify_gone(&set), GoneProof::AllGone);
}

/// A readable directory whose stat could not be read is a race or a permission
/// boundary, never proof of absence. This branch is unreachable from the I/O path
/// on a normal system, which is why the decision is separated from the reading.
#[test]
fn an_unreadable_stat_is_unknown_not_gone() {
    let b = boot();
    let id = identity(41, 12, &b);
    assert_eq!(
        resolve(&id, Some(&b), Observed::Unreadable),
        Presence::Unknown
    );
}

#[test]
fn the_decision_covers_every_observation() {
    let b = boot();
    let id = identity(41, 12, &b);
    assert_eq!(resolve(&id, Some(&b), Observed::NoSuchPid), Presence::Gone);
    assert_eq!(
        resolve(&id, Some(&b), Observed::StartedAt(12)),
        Presence::Alive
    );
    assert_eq!(
        resolve(&id, Some(&b), Observed::StartedAt(13)),
        Presence::Gone
    );
    // An unreadable boot id must not read as a different boot.
    assert_eq!(resolve(&id, None, Observed::NoSuchPid), Presence::Unknown);
}
