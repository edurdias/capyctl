//! SPEC §9.1, §13.3 / T21 T37: the owner-only rule for capyctl's own runtime
//! helper files and the directories on the way to them.
//!
//! Owner decisions 2026-09-22 and 2026-09-23: a file or directory capyctl put
//! there itself is trusted when it is owned by an accepted owner (root or the
//! service user), never writable by other, and writable by group only when
//! that group is the owning user's private group. A private group adds no
//! writer; a group whose membership cannot be established is refused. This is
//! the one Rust statement of the rule (`runtime/owner_only.py` mirrors it for
//! the Python helpers).
//!
//! Scope. The rule covers capyctl's runtime helpers: the runtime directory and
//! its modules (`capyctl_agent::runtime_integrity`) and the protected SGLang
//! entry path with its ancestors (`sglang::SglangLaunch::validate_wrapper_path`).
//! capyctl's private state it creates 0600/0700 itself (identity, credentials,
//! remote role files, launcher lock files, observation sockets) stays strict:
//! no group write at all. Engine installation files get no permission rule at
//! all (ADR 0008): their fingerprint is recorded at registration and drift is
//! flagged, and the internals capyctl hooks are probed by shape at launch.
//!
//! The check reads metadata and the account database only and changes nothing.

use nix::unistd::{Gid, Group, Uid, User};
use std::os::unix::fs::MetadataExt;

/// Whether `gid` is private to `uid`: `Some(true)` when no account but `uid`
/// can write through it, `Some(false)` when another can, `None` when that
/// cannot be established.
pub type PrivateGroup = dyn Fn(u32, u32) -> Option<bool>;

/// Why a path is not owner-only. Never carries a path or an account name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// Owned by an account that is neither root nor the service user.
    NotOwned,
    /// Writable by other.
    WritableByOther,
    /// Group-writable through a group other accounts can write through.
    WritableBySharedGroup,
    /// Group-writable through a group whose membership could not be read.
    GroupUndetermined,
}

/// Apply the owner-only rule to one path's metadata. `owners` are the uids
/// accepted as its owner; group write is judged against the owner's private
/// group.
pub fn check(
    metadata: &std::fs::Metadata,
    owners: &[u32],
    private_group: &PrivateGroup,
) -> Result<(), Problem> {
    if !owners.contains(&metadata.uid()) {
        return Err(Problem::NotOwned);
    }
    if metadata.mode() & 0o002 != 0 {
        return Err(Problem::WritableByOther);
    }
    if metadata.mode() & 0o020 != 0 {
        match private_group(metadata.gid(), metadata.uid()) {
            Some(true) => {}
            Some(false) => return Err(Problem::WritableBySharedGroup),
            None => return Err(Problem::GroupUndetermined),
        }
    }
    Ok(())
}

/// SPEC §9.1 / T21 T37: whether the account database can be read in full from
/// `/etc/passwd`, given the text of `/etc/nsswitch.conf` (`None`: absent, which
/// glibc reads as `files`). Only `files` and `systemd` (which synthesizes root,
/// nobody and dynamic users with their own groups) qualify; a directory
/// service (`sss`, `ldap`, `nis`, `winbind`, `compat` and the like) can hold
/// accounts sharing a group that no enumeration here would see, so the answer
/// there is "cannot be established".
pub fn nss_enumerable(nsswitch: Option<&str>) -> bool {
    // Absent, or no `passwd:` line: glibc reads `files`.
    nsswitch.is_none_or(|text| {
        text.lines()
            .map(|line| line.split('#').next().unwrap_or("").trim())
            .filter_map(|line| line.strip_prefix("passwd:"))
            .all(|sources| {
                sources
                    .split_whitespace()
                    .filter(|token| !token.starts_with('['))
                    .all(|source| matches!(source, "files" | "systemd"))
            })
    })
}

/// The account database's answer for whether `gid` is `uid`'s private group.
///
/// Private means all of: the group's name is the user's name, it is the
/// user's primary group, its supplementary member list names nobody but the
/// user, and no other account in `/etc/passwd` has it as its primary group.
/// Any lookup that fails yields `None`, never a guess; so does an account
/// database `/etc/passwd` does not hold in full ([`nss_enumerable`]).
pub fn system_private_group(gid: u32, uid: u32) -> Option<bool> {
    let nsswitch = match std::fs::read_to_string("/etc/nsswitch.conf") {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    if !nss_enumerable(nsswitch.as_deref()) {
        return None;
    }
    let user = User::from_uid(Uid::from_raw(uid)).ok()??;
    let group = Group::from_gid(Gid::from_raw(gid)).ok()??;
    if group.name != user.name
        || user.gid.as_raw() != gid
        || group.mem.iter().any(|member| *member != user.name)
    {
        return Some(false);
    }
    let passwd = std::fs::read("/etc/passwd").ok()?;
    for line in passwd.split(|&byte| byte == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&byte| byte == b':').collect();
        if fields.len() < 4 || fields[0] == user.name.as_bytes() {
            continue;
        }
        let shares = std::str::from_utf8(fields[3])
            .ok()
            .and_then(|field| field.parse::<u32>().ok())
            .is_some_and(|other| other == gid);
        if shares {
            return Some(false);
        }
    }
    Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn file(mode: u32) -> std::fs::Metadata {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("capyctl-owner-only-{}-{n}.py", std::process::id()));
        std::fs::write(&path, "# helper\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        metadata
    }

    fn euid() -> u32 {
        Uid::effective().as_raw()
    }

    // T21 T37
    #[test]
    fn group_write_is_trusted_only_under_the_owners_private_group() {
        for mode in [0o664, 0o620] {
            let metadata = file(mode);
            check(&metadata, &[euid()], &|_, _| Some(true)).unwrap();
            assert_eq!(
                check(&metadata, &[euid()], &|_, _| Some(false)),
                Err(Problem::WritableBySharedGroup)
            );
            assert_eq!(
                check(&metadata, &[euid()], &|_, _| None),
                Err(Problem::GroupUndetermined)
            );
        }
    }

    // T21 T37
    #[test]
    fn other_write_and_foreign_owners_are_refused() {
        let metadata = file(0o646);
        assert_eq!(
            check(&metadata, &[euid()], &|_, _| Some(true)),
            Err(Problem::WritableByOther)
        );
        let metadata = file(0o644);
        assert_eq!(
            check(&metadata, &[euid().wrapping_add(1)], &|_, _| Some(true)),
            Err(Problem::NotOwned)
        );
        // Without group write the group is never consulted.
        check(&metadata, &[euid()], &|_, _| None).unwrap();
    }

    // T21 T37: SPEC §9.1. Private-group membership is judged only from an
    // account database that can be read in full; with a directory service in
    // `passwd:` another account may share the group unseen, so the answer is
    // "undetermined" (and group write is refused), never a guess.
    #[test]
    fn a_directory_service_leaves_the_group_undetermined() {
        assert!(nss_enumerable(None));
        assert!(nss_enumerable(Some("passwd: files\ngroup: files\n")));
        assert!(nss_enumerable(Some("passwd:         files systemd\n")));
        assert!(nss_enumerable(Some(
            "# passwd: ldap\npasswd: files [NOTFOUND=return]\n"
        )));
        for text in [
            "passwd: files systemd sss\n",
            "passwd: files ldap\n",
            "passwd: compat\n",
            "passwd: nis files\n",
            "passwd: files winbind\n",
        ] {
            assert!(!nss_enumerable(Some(text)), "{text}");
        }
    }

    // T21 T37: a group that does not exist is never assumed private.
    #[test]
    fn an_unknown_group_is_undetermined() {
        assert_eq!(system_private_group(u32::MAX - 7, euid()), None);
    }
}
