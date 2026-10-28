//! ADR 0038 §9: the descendant check. A launch's processes are stopped through
//! its process group, so a process the engine started that left the group
//! (Open MPI 4.1.6's singleton daemon is started with `--set-sid`, "to get
//! out of our process group") could hold the GPU after a verified group stop.
//! At readiness and at every re-proof the host lists the leader's descendants
//! from the parent links of `/proc/<pid>/stat`; any whose process group
//! differs from the launch's fails the launch `engine_process_escaped`,
//! naming it. A process that left and was reparented away from the leader is
//! not seen: this narrows the exposure, it does not prove it absent.
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use capyctl_domain::completion::ProcessIdentity;

/// ADR 0038 §16: the closed launch failure of a descendant outside the
/// launch's process group.
pub const ENGINE_PROCESS_ESCAPED: &str = "engine_process_escaped";

/// The role an escaped process is named by when it joins the set a stop must
/// prove gone (`escaped-N`, N in the order found). It is neither the `api`
/// leader nor a member of the LMCache group, so a verified stop signals it
/// one by one and proves it gone beside the group.
pub const ESCAPED_ROLE_PREFIX: &str = "escaped-";

/// The fields of one `/proc/<pid>/stat` line the check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: u32,
    pub parent: u32,
    pub group: u32,
    /// `starttime`, in clock ticks since boot (field 22).
    pub start_ticks: u64,
}

/// One `/proc/<pid>/stat` line. The command name sits in parentheses and may
/// hold spaces and parentheses itself, so the fields are read after the last
/// `)`. `None` when the line does not parse.
pub fn parse_stat(text: &str) -> Option<ProcStat> {
    let (head, rest) = text.trim_end().rsplit_once(')')?;
    let pid = head.split_once(" (")?.0.trim().parse().ok()?;
    // After the command: state (3), ppid (4), pgrp (5), ... starttime (22).
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some(ProcStat {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        group: fields.get(2)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
    })
}

/// A descendant of the launch's leader outside the launch's process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapedProcess {
    /// The process, named `escaped-N`, with the leader's boot id and its own
    /// start ticks, so a stop can signal exactly it and prove it gone.
    pub identity: ProcessIdentity,
    /// The process group it runs in.
    pub group: u32,
    /// The launch's process group.
    pub launch_group: u32,
    /// The leader's pid.
    pub leader: u32,
}

impl std::fmt::Display for EscapedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{ENGINE_PROCESS_ESCAPED}: process {} (a descendant of the engine's api process {}) \
             runs in process group {}, outside the launch's process group {}",
            self.identity.pid, self.leader, self.group, self.launch_group
        )
    }
}

/// Why the descendants could not be listed: nothing is proven either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeUnreadable(pub String);

impl std::fmt::Display for TreeUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the engine's process tree could not be read: {}", self.0)
    }
}

fn read_stat(proc_root: &Path, pid: u32) -> std::io::Result<Option<ProcStat>> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat"))?;
    Ok(parse_stat(&text).filter(|stat| stat.pid == pid))
}

/// The leader as recorded, read now: its stat must parse and carry the
/// recorded start ticks, or the pid is another process's.
fn leader_stat(proc_root: &Path, leader: &ProcessIdentity) -> Result<ProcStat, TreeUnreadable> {
    match read_stat(proc_root, leader.pid) {
        Ok(Some(stat)) if stat.start_ticks == leader.start_ticks => Ok(stat),
        Ok(_) => Err(TreeUnreadable(format!(
            "the {} process {} is not the recorded process",
            leader.role, leader.pid
        ))),
        Err(error) => Err(TreeUnreadable(format!(
            "the {} process {}: {error}",
            leader.role, leader.pid
        ))),
    }
}

/// ADR 0038 §9: every descendant of `leader` (the launch's recorded `api`
/// process) whose process group differs from the leader's, read under
/// `proc_root` (`/proc` on a host). A process that exits while the table is
/// read is skipped; a leader that is not the recorded process, before or
/// after the read, or a table that cannot be listed, is unreadable.
pub fn escaped_descendants(
    proc_root: &Path,
    leader: &ProcessIdentity,
) -> Result<Vec<EscapedProcess>, TreeUnreadable> {
    let launch_group = leader_stat(proc_root, leader)?.group;
    let entries = std::fs::read_dir(proc_root)
        .map_err(|error| TreeUnreadable(format!("{}: {error}", proc_root.display())))?;
    let mut children: BTreeMap<u32, Vec<ProcStat>> = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| TreeUnreadable(error.to_string()))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        match read_stat(proc_root, pid) {
            Ok(Some(stat)) => children.entry(stat.parent).or_default().push(stat),
            // Exited between the listing and the read.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(None) => {
                return Err(TreeUnreadable(format!(
                    "process {pid}'s stat does not parse"
                )))
            }
            Err(error) => return Err(TreeUnreadable(format!("process {pid}: {error}"))),
        }
    }
    let mut escaped = Vec::new();
    let mut queue = VecDeque::from([leader.pid]);
    let mut seen = std::collections::BTreeSet::from([leader.pid]);
    while let Some(parent) = queue.pop_front() {
        for child in children.get(&parent).into_iter().flatten() {
            if !seen.insert(child.pid) {
                continue;
            }
            queue.push_back(child.pid);
            if child.group != launch_group {
                escaped.push(EscapedProcess {
                    identity: ProcessIdentity {
                        role: format!("{ESCAPED_ROLE_PREFIX}{}", escaped.len()),
                        pid: child.pid,
                        boot_id: leader.boot_id.clone(),
                        start_ticks: child.start_ticks,
                    },
                    group: child.group,
                    launch_group,
                    leader: leader.pid,
                });
            }
        }
    }
    // The leader's pid was the recorded process throughout the read.
    leader_stat(proc_root, leader)?;
    Ok(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(pid: u32, parent: u32, group: u32, start: u64) -> String {
        format!(
            "{pid} (python3 (x) y) S {parent} {group} {group} 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 \
             {start} 1000 10 18446744073709551615\n"
        )
    }

    fn write(root: &Path, pid: u32, text: &str) {
        std::fs::create_dir_all(root.join(pid.to_string())).unwrap();
        std::fs::write(root.join(pid.to_string()).join("stat"), text).unwrap();
    }

    fn leader() -> ProcessIdentity {
        ProcessIdentity {
            role: "api".into(),
            pid: 100,
            boot_id: "boot".into(),
            start_ticks: 5,
        }
    }

    // T49 (ADR 0038 §9): the command name may hold spaces and parentheses.
    #[test]
    fn a_stat_line_parses_after_the_command() {
        assert_eq!(
            parse_stat(&stat(7, 1, 7, 99)),
            Some(ProcStat {
                pid: 7,
                parent: 1,
                group: 7,
                start_ticks: 99
            })
        );
        assert_eq!(parse_stat("7 (x) S"), None);
    }

    // T49 (ADR 0038 §9): a grandchild in another process group is escaped; a
    // child in the group, and a process that is no descendant, are not.
    #[test]
    fn a_descendant_outside_the_group_is_found() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), 100, &stat(100, 1, 100, 5));
        write(root.path(), 101, &stat(101, 100, 100, 6));
        write(root.path(), 102, &stat(102, 101, 102, 7));
        write(root.path(), 300, &stat(300, 1, 300, 8));
        std::fs::create_dir_all(root.path().join("self")).unwrap();
        let escaped = escaped_descendants(root.path(), &leader()).unwrap();
        assert_eq!(escaped.len(), 1);
        assert_eq!(escaped[0].identity.pid, 102);
        assert_eq!(escaped[0].identity.role, "escaped-0");
        assert_eq!(escaped[0].identity.start_ticks, 7);
        assert_eq!((escaped[0].group, escaped[0].launch_group), (102, 100));
        assert!(escaped[0].to_string().starts_with(ENGINE_PROCESS_ESCAPED));
    }

    // T49 (ADR 0038 §9): a leader pid that is another process now proves
    // nothing.
    #[test]
    fn a_reused_leader_pid_is_unreadable() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), 100, &stat(100, 1, 100, 9));
        assert!(escaped_descendants(root.path(), &leader()).is_err());
        assert!(escaped_descendants(tempfile::tempdir().unwrap().path(), &leader()).is_err());
    }
}
