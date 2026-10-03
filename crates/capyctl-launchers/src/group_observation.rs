//! Read-only facts about a launcher's process group, not ownership authority.
//!
//! A stable group snapshot does not establish complete native enrollment: a
//! trusted engine topology hook and a no-group-escape contract are still needed.
//! API disappearance is an error, never evidence that the group has exited.
use std::collections::BTreeSet;
use std::io::Read;

use capyctl_domain::completion::{Presence, ProcessIdentity};

const MAX_STAT_BYTES: usize = 4096;
const MAX_METADATA_BYTES: usize = 65536;
const MAX_DIRECTORY_ENTRIES: usize = 65536;
const MAX_PROCESSES: usize = 32768;
const MAX_MEMBERS: usize = 256;

/// Kernel facts only. There is deliberately no inferred engine role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupProcessFact {
    pub pid: u32,
    pub parent_pid: u32,
    pub process_group: u32,
    pub boot_id: String,
    pub start_ticks: u64,
}

/// Two matching observations in the collector's current PID namespace.
/// Not a receipt, enrollment proof, allocation proof, or cleanup capability.
#[derive(Debug)]
pub struct ProcessGroupObservation {
    process_group: u32,
    members: Vec<GroupProcessFact>,
}

impl ProcessGroupObservation {
    pub fn process_group(&self) -> u32 {
        self.process_group
    }
    pub fn members(&self) -> &[GroupProcessFact] {
        &self.members
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum GroupObservationError {
    #[error("process visibility unavailable or restricted")]
    Visibility,
    #[error("process observation limit exceeded")]
    Limit,
    #[error("malformed or inconsistent kernel process data")]
    InvalidData,
    #[error("expected API identity is not the current group leader")]
    ApiIdentity,
    #[error("process group changed during observation")]
    Changed,
}

/// Observe a launcher group anchored to an independently persisted API identity.
/// Only fixed `/proc` paths are accepted. Any unreadable process, or a group that
/// changes between the two snapshots, fails closed; a process that exits during a
/// scan is simply not in it. No signals, role inference, retries, or receipt
/// creation occur.
pub fn observe_process_group(
    expected_api: &ProcessIdentity,
) -> Result<ProcessGroupObservation, GroupObservationError> {
    if expected_api.role != "api" || expected_api.pid == 0 || expected_api.start_ticks == 0 {
        return Err(GroupObservationError::ApiIdentity);
    }
    let mounts = read_bounded("/proc/mounts", MAX_METADATA_BYTES)?;
    check_mounts(&mounts)?;
    let boot = read_boot()?;
    if boot != expected_api.boot_id {
        return Err(GroupObservationError::ApiIdentity);
    }
    let first = snapshot(expected_api.pid, &boot)?;
    validate_api(expected_api, &first)?;
    let second = snapshot(expected_api.pid, &boot)?;
    if read_boot()? != boot || read_bounded("/proc/mounts", MAX_METADATA_BYTES)? != mounts {
        return Err(GroupObservationError::Changed);
    }
    finish(expected_api, first, second)
}

/// Like `observe_process_group`, but a group whose leader is gone is answered
/// rather than refused, because cleanup needs "nothing is left" as an answer.
/// Any other failure still fails closed.
///
/// A leader that has exited does not mean the group has: a worker can outlive it
/// and keep holding device memory. So the leaderless case is answered by scanning
/// for processes still in the group, not by assuming it is empty. A member that
/// started before the recorded leader cannot be one of its workers and is left
/// out, which excludes an older group that happens to carry this pid.
///
/// It does not exclude the other direction: if the pid is reused after our leader
/// exits by an unrelated process that becomes a group leader, that group's members
/// all started later and are reported here. Nothing is signalled on the strength of
/// that, because a recorded identity is what `terminate_owned` signals; the effect
/// is that the group reads as non-empty and the release is refused. Fail-closed,
/// and never a kill of something that is not ours.
pub fn observe_process_group_or_empty(
    expected_api: &ProcessIdentity,
) -> Result<Vec<GroupProcessFact>, GroupObservationError> {
    match super::process_absence::presence(expected_api) {
        Presence::Unknown => Err(GroupObservationError::Visibility),
        Presence::Alive => observe_process_group(expected_api).map(|o| o.members().to_vec()),
        Presence::Gone => {
            let boot = read_boot()?;
            // A different boot is positive evidence: nothing recorded against the
            // previous boot can still be running under any pid.
            if boot != expected_api.boot_id {
                return Ok(Vec::new());
            }
            let mut members = scan_group_by_pgid(expected_api.pid, &boot)?;
            members.retain(|fact| fact.start_ticks >= expected_api.start_ticks);
            Ok(members)
        }
    }
}

/// Every live process whose process group is `pgid`, whether or not the leader
/// still exists. The group id is the leader's pid, so this survives the leader.
///
/// A pid that exits during the scan is skipped (see `read_listed`); every other
/// read failure still fails closed.
pub fn scan_group_by_pgid(
    pgid: u32,
    boot: &str,
) -> Result<Vec<GroupProcessFact>, GroupObservationError> {
    if pgid == 0 {
        return Err(GroupObservationError::ApiIdentity);
    }
    let mounts = read_bounded("/proc/mounts", MAX_METADATA_BYTES)?;
    check_mounts(&mounts)?;
    // The host rebooted between the caller reading the boot id and this scan.
    // Nothing about the expected identity is wrong, so this is the observation
    // changing underneath us, which is what the caller retries.
    if read_boot()? != boot {
        return Err(GroupObservationError::Changed);
    }
    let facts = list_pids()?
        .into_iter()
        .filter_map(|pid| read_listed(pid, boot));
    collect_members(pgid, facts)
}

/// ADR 0014 amendment A12: the programs a JIT kernel build runs (FlashInfer,
/// SGLang's and PyTorch's extension builds, TensorFold's CUDA build), by the
/// kernel's process name. Triton and NVRTC compile inside the engine process
/// and are not seen.
const KERNEL_BUILD_TOOLS: &[&str] = &[
    "ninja",
    "nvcc",
    "cicc",
    "ptxas",
    "cudafe++",
    "fatbinary",
    "nvlink",
    "cc1plus",
    "cc1",
    "c++",
    "g++",
    "gcc",
    "cc",
    "ld",
    "collect2",
    "as",
    "make",
    "cmake",
];

/// ADR 0014 amendment A12: whether a compiler runs now in the process group
/// `api` leads. A best-effort reading for the startup peak only, never for
/// ownership: anything unreadable answers `false`, which counts the sample
/// as the engine's, as before this amendment.
pub fn group_building(api: &ProcessIdentity) -> bool {
    if api.pid == 0 || read_boot().ok().as_deref() != Some(api.boot_id.as_str()) {
        return false;
    }
    let Ok(pids) = list_pids() else {
        return false;
    };
    let members = pids.into_iter().filter_map(|pid| {
        let raw = read_bounded(&format!("/proc/{pid}/stat"), MAX_STAT_BYTES).ok()?;
        let fact = parse_stat(pid, &raw, &api.boot_id).ok()?;
        Some((fact, stat_comm(&raw)?.to_owned()))
    });
    builds_in(api, members)
}

/// Whether any of `members` is a compiler in `api`'s group that began no
/// earlier than its leader (an older one belongs to an earlier group).
fn builds_in(
    api: &ProcessIdentity,
    members: impl IntoIterator<Item = (GroupProcessFact, String)>,
) -> bool {
    members.into_iter().any(|(fact, comm)| {
        fact.process_group == api.pid
            && fact.start_ticks >= api.start_ticks
            && KERNEL_BUILD_TOOLS.contains(&comm.as_str())
    })
}

/// The process name in a `/proc/<pid>/stat` line: between the first `(` and
/// the last `) `, since the name itself may hold either.
fn stat_comm(raw: &str) -> Option<&str> {
    let open = raw.find(" (")? + 2;
    let close = raw.rfind(") ")?;
    raw.get(open..close)
}

/// One listed pid's facts, or `None` if it exited after the listing.
///
/// A pid that disappears between the directory listing and its `stat` read is
/// not a live member of any group; every other read failure still fails closed,
/// because an unreadable process is indistinguishable from a hidden one. Without
/// this, any unrelated process exiting anywhere on the host during a scan made
/// the whole observation fail, which on a busy host is nearly every scan.
fn read_listed(pid: u32, boot: &str) -> Option<Result<GroupProcessFact, GroupObservationError>> {
    match read_bounded(&format!("/proc/{pid}/stat"), MAX_STAT_BYTES) {
        Ok(raw) => Some(parse_stat(pid, &raw, boot)),
        Err(GroupObservationError::Visibility)
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() =>
        {
            None
        }
        Err(error) => Some(Err(error)),
    }
}

fn read_bounded(path: &str, limit: usize) -> Result<String, GroupObservationError> {
    let file = std::fs::File::open(path).map_err(|_| GroupObservationError::Visibility)?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| GroupObservationError::Visibility)?;
    if bytes.len() > limit {
        return Err(GroupObservationError::Limit);
    }
    String::from_utf8(bytes).map_err(|_| GroupObservationError::InvalidData)
}

fn read_boot() -> Result<String, GroupObservationError> {
    let raw = read_bounded("/proc/sys/kernel/random/boot_id", 64)?;
    let boot = raw.trim_end_matches('\n');
    if boot.len() != 36
        || !boot.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err(GroupObservationError::InvalidData);
    }
    Ok(boot.to_owned())
}

fn check_mounts(raw: &str) -> Result<(), GroupObservationError> {
    let mut found = false;
    for line in raw.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 6 {
            return Err(GroupObservationError::Visibility);
        }
        if fields[1] == "/proc" {
            if found || fields[2] != "proc" {
                return Err(GroupObservationError::Visibility);
            }
            found = true;
            if fields[3].split(',').any(|option| {
                (option.starts_with("hidepid=") && option != "hidepid=0")
                    || option.starts_with("subset=")
            }) {
                return Err(GroupObservationError::Visibility);
            }
        } else if fields[1].starts_with("/proc/") {
            // Submounts on a numeric process path could conceal group members.
            if fields[1][6..]
                .split('/')
                .next()
                .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
            {
                return Err(GroupObservationError::Visibility);
            }
        }
    }
    if !found {
        return Err(GroupObservationError::Visibility);
    }
    Ok(())
}

fn snapshot(group: u32, boot: &str) -> Result<Vec<GroupProcessFact>, GroupObservationError> {
    // A member that exits mid-scan is absent from this snapshot and the next, and
    // one that exits between the two makes them differ, which `finish` refuses as
    // Changed. So skipping exited pids never hides a live member.
    let facts = list_pids()?
        .into_iter()
        .filter_map(|pid| read_listed(pid, boot));
    collect_members(group, facts)
}

fn list_pids() -> Result<BTreeSet<u32>, GroupObservationError> {
    let entries = std::fs::read_dir("/proc").map_err(|_| GroupObservationError::Visibility)?;
    let mut pids = BTreeSet::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return Err(GroupObservationError::Limit);
        }
        let entry = entry.map_err(|_| GroupObservationError::Visibility)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(GroupObservationError::InvalidData)?;
        if name.bytes().all(|b| b.is_ascii_digit()) {
            let pid = name
                .parse::<u32>()
                .map_err(|_| GroupObservationError::InvalidData)?;
            if pid == 0 || pid.to_string() != name || !pids.insert(pid) {
                return Err(GroupObservationError::InvalidData);
            }
            if pids.len() > MAX_PROCESSES {
                return Err(GroupObservationError::Limit);
            }
        }
    }
    Ok(pids)
}

fn collect_members(
    group: u32,
    facts: impl IntoIterator<Item = Result<GroupProcessFact, GroupObservationError>>,
) -> Result<Vec<GroupProcessFact>, GroupObservationError> {
    let mut seen = BTreeSet::new();
    let mut members = Vec::new();
    for fact in facts {
        let fact = fact?;
        if !seen.insert(fact.pid) {
            return Err(GroupObservationError::InvalidData);
        }
        if seen.len() > MAX_PROCESSES {
            return Err(GroupObservationError::Limit);
        }
        if fact.process_group == group {
            // A member that began at boot cannot belong to a group launched later;
            // reporting it would attribute a kernel process to a launch.
            if fact.start_ticks == 0 {
                return Err(GroupObservationError::InvalidData);
            }
            if members.len() >= MAX_MEMBERS {
                return Err(GroupObservationError::Limit);
            }
            members.push(fact);
        }
    }
    members.sort_unstable_by_key(|fact| fact.pid);
    Ok(members)
}

fn parse_stat(pid: u32, raw: &str, boot: &str) -> Result<GroupProcessFact, GroupObservationError> {
    let bad = GroupObservationError::InvalidData;
    if raw.len() > MAX_STAT_BYTES {
        return Err(GroupObservationError::Limit);
    }
    let prefix = format!("{pid} (");
    if !raw.starts_with(&prefix) {
        return Err(bad);
    }
    let close = raw.rfind(") ").ok_or(GroupObservationError::InvalidData)?;
    if close < prefix.len() {
        return Err(bad);
    }
    let fields: Vec<_> = raw[close + 2..].split_ascii_whitespace().collect();
    if fields.len() != 50 || fields[0].len() != 1 || !"RSDZTtXxKWPI".contains(fields[0]) {
        return Err(bad);
    }
    if fields[1..].iter().any(|s| {
        let digits = s.strip_prefix('-').unwrap_or(s);
        digits.is_empty()
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || s.parse::<i128>().is_err()
    }) {
        return Err(bad);
    }
    let parent_pid = fields[1]
        .parse()
        .map_err(|_| GroupObservationError::InvalidData)?;
    let process_group = fields[2]
        .parse()
        .map_err(|_| GroupObservationError::InvalidData)?;
    let start_ticks = fields[19]
        .parse()
        .map_err(|_| GroupObservationError::InvalidData)?;
    // A start time of zero is not malformed: on some kernels (host-a, Linux
    // 6.17 nvidia) init and every kernel thread report exactly that. Such a process
    // began at boot, so it can never be a member of a group we launched, and it is
    // refused where membership is claimed (`collect_members`, `validate_api`)
    // rather than here, where refusing it would fail the whole scan.
    if pid == 0 || parent_pid == pid {
        return Err(bad);
    }
    Ok(GroupProcessFact {
        pid,
        parent_pid,
        process_group,
        boot_id: boot.into(),
        start_ticks,
    })
}

fn validate_api(
    expected: &ProcessIdentity,
    facts: &[GroupProcessFact],
) -> Result<(), GroupObservationError> {
    if expected.role != "api"
        || !facts.iter().any(|fact| {
            fact.pid == expected.pid
                && fact.process_group == expected.pid
                && fact.start_ticks == expected.start_ticks
                && fact.boot_id == expected.boot_id
        })
    {
        return Err(GroupObservationError::ApiIdentity);
    }
    Ok(())
}

fn finish(
    expected: &ProcessIdentity,
    first: Vec<GroupProcessFact>,
    second: Vec<GroupProcessFact>,
) -> Result<ProcessGroupObservation, GroupObservationError> {
    validate_api(expected, &first)?;
    validate_api(expected, &second)?;
    if first != second {
        return Err(GroupObservationError::Changed);
    }
    Ok(ProcessGroupObservation {
        process_group: expected.pid,
        members: second,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: &str = "12345678-1234-1234-1234-123456789abc";

    fn stat(pid: u32, parent: u32, group: u32, ticks: u64) -> String {
        let mut fields = vec!["0".to_owned(); 50];
        fields[0] = "S".into();
        fields[1] = parent.to_string();
        fields[2] = group.to_string();
        fields[19] = ticks.to_string();
        format!("{pid} (odd ) name\n) {}\n", fields.join(" "))
    }

    fn api() -> ProcessIdentity {
        ProcessIdentity {
            role: "api".into(),
            pid: 42,
            boot_id: BOOT.into(),
            start_ticks: 10,
        }
    }

    /// ADR 0014 amendment A12: a compiler in the launched group is a kernel
    /// build; the same name outside it, or older than the leader, is not.
    #[test]
    fn a_compiler_in_the_launched_group_is_a_kernel_build() {
        let member = |pid, group, ticks, comm: &str| {
            let raw = stat(pid, 1, group, ticks).replace("(odd ) name\n)", &format!("({comm})"));
            (
                parse_stat(pid, &raw, BOOT).unwrap(),
                stat_comm(&raw).unwrap().to_owned(),
            )
        };
        let engine = member(42, 42, 10, "python3");
        assert!(!builds_in(&api(), [engine.clone()]));
        for tool in ["ninja", "nvcc", "cicc", "ptxas", "cc1plus", "c++", "g++"] {
            assert!(
                builds_in(&api(), [engine.clone(), member(50, 42, 11, tool)]),
                "{tool}"
            );
        }
        // Another group, or a process that began before the leader.
        assert!(!builds_in(
            &api(),
            [engine.clone(), member(50, 77, 11, "nvcc")]
        ));
        assert!(!builds_in(&api(), [engine, member(50, 42, 9, "nvcc")]));
        assert_eq!(stat_comm(&stat(42, 1, 42, 10)), Some("odd ) name\n"));
    }

    /// ADR 0014 amendment A12, on this host's /proc: a group led by a
    /// process named like a compiler is building; one led by `sleep` is not.
    #[test]
    fn a_running_compiler_group_is_seen_as_building() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let ninja = dir.path().join("ninja");
        // A link, not a copy: a copy's write descriptor can leak into a child
        // another test forks meanwhile, and exec then fails with ETXTBSY. The
        // kernel names the process after the link.
        std::os::unix::fs::symlink("/bin/sleep", &ninja).unwrap();
        let identity = |child: &std::process::Child| {
            let pid = child.id();
            let boot = read_boot().unwrap();
            let raw = read_bounded(&format!("/proc/{pid}/stat"), MAX_STAT_BYTES).unwrap();
            let fact = parse_stat(pid, &raw, &boot).unwrap();
            ProcessIdentity {
                role: "api".into(),
                pid,
                boot_id: boot,
                start_ticks: fact.start_ticks,
            }
        };
        let mut building = std::process::Command::new(&ninja)
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut idle = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        // The copy is exec'd by the time its stat names it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !group_building(&identity(&building)) {
            assert!(std::time::Instant::now() < deadline, "build never seen");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!group_building(&identity(&idle)));
        let _ = building.kill();
        let _ = idle.kill();
        let _ = building.wait();
        let _ = idle.wait();
    }

    #[test]
    fn stat_handles_parenthesized_names_and_extracts_kernel_identity() {
        let parsed = parse_stat(42, &stat(42, 1, 42, 10), BOOT).unwrap();
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.parent_pid, 1);
        assert_eq!(parsed.process_group, 42);
        assert_eq!(parsed.start_ticks, 10);
        assert_eq!(parsed.boot_id, BOOT);
    }

    #[test]
    fn stat_rejects_inconsistent_truncated_and_oversized_data() {
        for raw in [
            stat(43, 1, 42, 10),
            "42 (x) S 1 42".into(),
            "x".repeat(MAX_STAT_BYTES + 1),
        ] {
            assert!(parse_stat(42, &raw, BOOT).is_err());
        }
        assert!(parse_stat(42, &stat(42, 1, 42, 10).replace("S 1 42", "S +1 42"), BOOT).is_err());
    }

    /// Found live on host-a (Linux 6.17 nvidia): init and every kernel thread
    /// report a start time of zero. A scan that rejects them as malformed can never
    /// prove any group gone on such a host. They parse, they are never members of a
    /// launched group, and one claiming membership is inconsistent data.
    // T33
    #[test]
    fn boot_time_processes_parse_but_never_join_a_launched_group() {
        let init = parse_stat(1, &stat(1, 0, 1, 0), BOOT).unwrap();
        assert_eq!(init.start_ticks, 0);
        let kthread = parse_stat(2, &stat(2, 0, 0, 0), BOOT).unwrap();
        let ours = parse_stat(42, &stat(42, 1, 42, 10), BOOT).unwrap();
        let members =
            collect_members(42, [Ok(init.clone()), Ok(kthread), Ok(ours.clone())]).unwrap();
        assert_eq!(members, vec![ours]);
        let impostor = parse_stat(7, &stat(7, 1, 42, 0), BOOT).unwrap();
        assert_eq!(
            collect_members(42, [Ok(init), Ok(impostor)]),
            Err(GroupObservationError::InvalidData)
        );
        assert!(validate_api(
            &api(),
            &[parse_stat(42, &stat(42, 1, 42, 0), BOOT).unwrap()]
        )
        .is_err());
    }

    #[test]
    fn api_identity_must_match_exactly_and_lead_group() {
        let facts = vec![parse_stat(42, &stat(42, 1, 42, 10), BOOT).unwrap()];
        assert!(validate_api(&api(), &facts).is_ok());
        for expected in [
            ProcessIdentity {
                role: "worker-0".into(),
                ..api()
            },
            ProcessIdentity {
                start_ticks: 11,
                ..api()
            },
            ProcessIdentity {
                boot_id: "other".into(),
                ..api()
            },
        ] {
            assert!(validate_api(&expected, &facts).is_err());
        }
        assert!(validate_api(&api(), &[]).is_err());
        assert!(validate_api(
            &api(),
            &[parse_stat(42, &stat(42, 1, 41, 10), BOOT).unwrap()]
        )
        .is_err());
    }

    #[test]
    fn snapshots_reject_membership_identity_and_parent_changes() {
        let first = vec![parse_stat(42, &stat(42, 1, 42, 10), BOOT).unwrap()];
        assert!(finish(&api(), first.clone(), first.clone()).is_ok());
        for second in [
            vec![],
            vec![parse_stat(42, &stat(42, 2, 42, 10), BOOT).unwrap()],
            vec![parse_stat(42, &stat(42, 1, 42, 11), BOOT).unwrap()],
        ] {
            assert!(finish(&api(), first.clone(), second).is_err());
        }
    }

    #[test]
    fn restricted_or_unknown_proc_mount_visibility_is_rejected() {
        assert!(check_mounts("proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n").is_ok());
        for mounts in [
            "",
            "proc /proc proc rw,hidepid=2 0 0\n",
            "proc /proc proc rw,subset=pid 0 0\n",
            "tmpfs /proc tmpfs rw 0 0\n",
            "proc /proc proc rw 0 0\ntmpfs /proc/42 tmpfs rw 0 0\n",
            "proc /proc proc rw 0 0\nproc /proc proc rw 0 0\n",
        ] {
            assert!(check_mounts(mounts).is_err());
        }
    }

    #[test]
    fn scans_reject_duplicate_pids_unreadable_processes_and_limits() {
        let fact = parse_stat(42, &stat(42, 1, 42, 10), BOOT).unwrap();
        assert_eq!(
            collect_members(42, [Ok(fact.clone()), Ok(fact)]),
            Err(GroupObservationError::InvalidData)
        );
        assert_eq!(
            collect_members(42, [Err(GroupObservationError::Visibility)]),
            Err(GroupObservationError::Visibility)
        );
        let members = (1..=257).map(|pid| parse_stat(pid, &stat(pid, 0, 42, 10), BOOT));
        assert_eq!(
            collect_members(42, members),
            Err(GroupObservationError::Limit)
        );
        let processes = (1..=32769).map(|pid| parse_stat(pid, &stat(pid, 0, 0, 10), BOOT));
        assert_eq!(
            collect_members(42, processes),
            Err(GroupObservationError::Limit)
        );
    }

    #[test]
    fn real_owned_cpu_group_is_observed_without_inferred_roles_or_signals() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut command = Command::new("sh");
        command.args(["-c", "read value"]).stdin(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)
            });
        }
        let mut child = command.spawn().unwrap();
        let identity = crate::exec::process_identity(child.id(), "api").unwrap();
        // Unrelated exits no longer fail a scan, but a mount-table change
        // elsewhere on the host is a genuine Changed; the fixture may take
        // another fresh observation while its own child remains unchanged.
        let mut observed = observe_process_group(&identity);
        for _ in 0..7 {
            if observed.as_ref().err() != Some(&GroupObservationError::Changed) {
                break;
            }
            observed = observe_process_group(&identity);
        }
        // EOF lets the owned CPU fixture exit normally, including on failure.
        drop(child.stdin.take());
        child.wait().unwrap();
        let observed = observed.unwrap();
        assert_eq!(observed.process_group(), identity.pid);
        assert_eq!(observed.members().len(), 1);
        assert_eq!(observed.members()[0].pid, identity.pid);
        assert_eq!(observed.members()[0].start_ticks, identity.start_ticks);
        assert_eq!(observed.members()[0].parent_pid, std::process::id());
        assert!(observe_process_group(&identity).is_err());
    }

    /// An unrelated process that exits between the `/proc` listing and its `stat`
    /// read is not a member of the observed group and says nothing about it. A
    /// busy host (or a parallel test run) retires processes constantly, so an
    /// observation that failed on every such exit could rarely answer at all.
    /// The group itself is stable here, so every observation must succeed without
    /// a retry while unrelated processes are created and reaped around it.
    // T31
    #[test]
    fn unrelated_process_exits_during_a_scan_do_not_fail_the_observation() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut command = Command::new("sh");
        command.args(["-c", "read value"]).stdin(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)
            });
        }
        let mut child = command.spawn().unwrap();
        let identity = crate::exec::process_identity(child.id(), "api").unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let churn: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = Command::new("true").status();
                    }
                })
            })
            .collect();
        let outcomes: Vec<_> = (0..200)
            .map(|_| observe_process_group(&identity).map(|o| o.members().len()))
            .collect();
        stop.store(true, Ordering::Relaxed);
        for thread in churn {
            thread.join().unwrap();
        }
        // EOF lets the owned CPU fixture exit normally, including on failure.
        drop(child.stdin.take());
        child.wait().unwrap();
        // A mount-table change elsewhere on the host is a real Changed and is
        // left to the caller's retry; an unrelated exit must never surface.
        let failures: Vec<_> = outcomes
            .iter()
            .filter(|o| !matches!(o, Ok(1) | Err(GroupObservationError::Changed)))
            .collect();
        assert!(
            failures.is_empty(),
            "{} of 200 observations failed, first: {:?}",
            failures.len(),
            failures[0]
        );
        assert!(outcomes.iter().any(|o| o == &Ok(1)), "{outcomes:?}");
    }

    /// A leader that has exited is not proof that its group has: the worker it
    /// started is still in the group and still holding whatever it allocated, so it
    /// is reported rather than hidden behind an empty answer.
    // T31: head gone, worker surviving.
    #[test]
    fn a_worker_that_outlives_its_leader_is_still_reported() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & read value"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)
            });
        }
        let mut child = command.spawn().unwrap();
        let leader = crate::exec::process_identity(child.id(), "api").unwrap();
        // EOF ends the leader while its worker keeps running.
        drop(child.stdin.take());
        child.wait().unwrap();
        assert_eq!(crate::process_absence::presence(&leader), Presence::Gone);
        assert!(observe_process_group(&leader).is_err());
        let members = observe_process_group_or_empty(&leader).unwrap();
        assert_eq!(members.len(), 1, "{members:?}");
        assert_ne!(members[0].pid, leader.pid);
        assert_eq!(members[0].process_group, leader.pid);
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(members[0].pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if observe_process_group_or_empty(&leader).is_ok_and(|m| m.is_empty()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the group was never observed empty after the worker was killed");
    }
}
