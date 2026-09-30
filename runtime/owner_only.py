"""The owner-only rule for capyctl's own runtime files and directories.

Python mirror of `crates/capyctl-adapters/src/owner_only.rs` (owner decisions
2026-09-22 and 2026-09-23). A file or directory capyctl put there itself is
trusted when it is owned by an accepted owner (root or the service user),
never writable by other, and writable by group only when that group is the
owning user's private group: a private group adds no writer. A group whose
membership cannot be established is refused.

This rule is for capyctl's own helpers and the directories on the way to them.
capyctl's private state (identity, credentials, locks, observation sockets and
their 0700 directory) stays strict: no group write at all. Engine installation
files get no permission rule (ADR 0008): drift is flagged from the
installation's fingerprint and internals are probed by shape instead.
"""

import grp
import os
import pwd


class OwnerOnlyError(Exception):
    """One closed problem; never a path or an account name."""

    def __init__(self, problem):
        self.problem = problem
        super().__init__(problem)


def nss_enumerable(nsswitch):
    """Whether the account database can be enumerated in full.

    SPEC §9.1 / T21 T37, mirrors `owner_only.rs::nss_enumerable`: only `files`
    and `systemd` sources in `passwd:` qualify (`None`, an absent file, reads
    as `files`). A directory service can hold accounts sharing a group that no
    enumeration here would see.
    """
    if nsswitch is None:
        return True
    for line in nsswitch.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line.startswith("passwd:"):
            continue
        sources = [token for token in line[len("passwd:"):].split()
                   if not token.startswith("[")]
        if any(source not in ("files", "systemd") for source in sources):
            return False
    return True


def _nsswitch():
    try:
        with open("/etc/nsswitch.conf", encoding="utf-8") as stream:
            return stream.read(65536)
    except FileNotFoundError:
        return None


def system_private_group(gid, uid):
    """Whether `gid` is `uid`'s private group: True, False, or None if unknown.

    Private means all of: the group's name is the user's name, it is the
    user's primary group, its supplementary member list names nobody but the
    user, and no other account has it as its primary group. An account
    database that cannot be enumerated in full leaves it unknown.
    """
    try:
        if not nss_enumerable(_nsswitch()):
            return None
        user = pwd.getpwuid(uid)
        group = grp.getgrgid(gid)
        if (group.gr_name != user.pw_name or user.pw_gid != gid
                or any(member != user.pw_name for member in group.gr_mem)):
            return False
        for other in pwd.getpwall():
            if other.pw_name != user.pw_name and other.pw_gid == gid:
                return False
        return True
    except Exception:
        return None


def check(info, owners=None, private_group=None):
    """Raise OwnerOnlyError unless `info` (an os.stat_result) is owner-only."""
    owners = (0, os.geteuid()) if owners is None else owners
    private_group = system_private_group if private_group is None else private_group
    if info.st_uid not in owners:
        raise OwnerOnlyError("not_owned")
    if info.st_mode & 0o002:
        raise OwnerOnlyError("writable_by_other")
    if info.st_mode & 0o020:
        verdict = private_group(info.st_gid, info.st_uid)
        if verdict is None:
            raise OwnerOnlyError("group_undetermined")
        if verdict is not True:
            raise OwnerOnlyError("writable_by_shared_group")
