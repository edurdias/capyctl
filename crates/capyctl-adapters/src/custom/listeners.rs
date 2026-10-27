//! ADR 0031 §7: the loopback check. CapyCTL cannot validate a custom
//! engine's options, so once its probe passes the host lists the TCP
//! listening sockets the launch's owned processes hold: each process's socket
//! inodes (`/proc/<pid>/fd` links `socket:[<inode>]`) against the listening
//! rows of `/proc/<pid>/net/tcp` and `tcp6`. Any listener on an address other
//! than `127.0.0.1` or `::1` fails the launch `custom_listener_exposed`. A
//! listener opened before readiness and closed again, or opened later, is not
//! seen: this narrows the exposure, it does not prove it absent.
//!
//! ADR 0035 §5: the same check runs on every built-in engine's launch on
//! several GPUs of one host, which fails `engine_listener_exposed` naming the
//! process role, address and port ([`engine_listener_exposed`]).
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use capyctl_domain::completion::{Presence, ProcessIdentity};

/// The kernel's `TCP_LISTEN` state in `/proc/net/tcp`.
const TCP_LISTEN: &str = "0A";

/// One TCP listener an owned process holds: the process (its recorded role
/// and pid) and the address it listens on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnedListener {
    pub address: SocketAddr,
    pub role: String,
    pub pid: u32,
}

/// Why the check did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerCheck {
    /// An owned process listens off loopback (ADR 0031 §10); ADR 0035 §5:
    /// with the process that holds it.
    Exposed(OwnedListener),
    /// The sockets could not be read, or a process could not be established
    /// as the recorded one after they were: nothing is proven either way.
    Unreadable(String),
    /// A member exited while it was read: its `/proc/<pid>` was gone (or it
    /// was no longer the recorded process once read) and its presence is
    /// verified `Gone`. It holds no socket, but the group changed under the
    /// check, so the caller lists it again (a server's short-lived child,
    /// found live on LMCache 0.5.6 under SGLang) or fails closed.
    Vanished(ProcessIdentity),
}

impl std::fmt::Display for ListenerCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenerCheck::Exposed(listener) => write!(
                f,
                "{}: an owned process listens on {}, not on loopback",
                capyctl_config::custom::LISTENER_EXPOSED,
                listener.address
            ),
            ListenerCheck::Unreadable(reason) => {
                write!(f, "the engine's listeners could not be read: {reason}")
            }
            ListenerCheck::Vanished(identity) => write!(
                f,
                "the engine's listeners could not be read: {} process {} exited while \
                 its sockets were read",
                identity.role, identity.pid
            ),
        }
    }
}

/// The listening sockets one `/proc/net/tcp` or `tcp6` table names, by
/// inode. Rows in any other state, and rows that do not parse, are skipped
/// (a header, or a socket that is not listening).
pub fn listening_sockets(table: &str) -> BTreeMap<u64, SocketAddr> {
    let mut found = BTreeMap::new();
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
        let (Some(local), Some(state), Some(inode)) = (fields.get(1), fields.get(3), fields.get(9))
        else {
            continue;
        };
        if *state != TCP_LISTEN {
            continue;
        }
        let (Some(address), Ok(inode)) = (socket_address(local), inode.parse::<u64>()) else {
            continue;
        };
        if inode != 0 {
            found.insert(inode, address);
        }
    }
    found
}

/// `0100007F:1F90` (IPv4) or 32 hex digits and a port (IPv6). The kernel
/// prints each 32-bit word of the address as a native-endian integer, and
/// the port in host order.
fn socket_address(text: &str) -> Option<SocketAddr> {
    let (address, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let words: Vec<[u8; 4]> = (0..address.len() / 8)
        .map(|i| {
            address
                .get(i * 8..i * 8 + 8)
                .and_then(|word| u32::from_str_radix(word, 16).ok())
                .map(u32::to_ne_bytes)
        })
        .collect::<Option<_>>()?;
    let ip = match (address.len(), words.as_slice()) {
        (8, [word]) => IpAddr::V4(Ipv4Addr::from(*word)),
        (32, [a, b, c, d]) => {
            let mut bytes = [0u8; 16];
            for (i, word) in [a, b, c, d].into_iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(word);
            }
            let v6 = Ipv6Addr::from(bytes);
            // An IPv4-mapped address is that IPv4 address.
            match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(v6),
            }
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// ADR 0031 §7: only `127.0.0.1` and `::1` are loopback here. Another
/// `127.x` address is not what `{host}` renders.
pub fn is_loopback(address: &SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(v4) => v4 == Ipv4Addr::LOCALHOST,
        IpAddr::V6(v6) => v6 == Ipv6Addr::LOCALHOST,
    }
}

/// The socket inodes one process holds (`/proc/<pid>/fd/*` links
/// `socket:[<inode>]`).
fn socket_inodes(fd_dir: &Path) -> std::io::Result<BTreeSet<u64>> {
    let mut inodes = BTreeSet::new();
    for entry in std::fs::read_dir(fd_dir)? {
        let Ok(target) = std::fs::read_link(entry?.path()) else {
            // A descriptor closed between the listing and the read.
            continue;
        };
        if let Some(inode) = target
            .to_str()
            .and_then(|text| text.strip_prefix("socket:["))
            .and_then(|text| text.strip_suffix(']'))
            .and_then(|text| text.parse().ok())
        {
            inodes.insert(inode);
        }
    }
    Ok(inodes)
}

/// Every TCP listener the processes `identities` hold, read under
/// `proc_root` (`/proc` on a host). `presence` is asked about a process
/// whose entries are missing and about each process after its sockets were
/// read. A process verified `Gone` then exited during the read
/// ([`ListenerCheck::Vanished`]); one whose presence is anything but `Alive`
/// otherwise, or any other read failure, makes the reading unreadable rather
/// than clean.
pub fn owned_listeners(
    proc_root: &Path,
    identities: &[ProcessIdentity],
    presence: &dyn Fn(&ProcessIdentity) -> Presence,
) -> Result<Vec<OwnedListener>, ListenerCheck> {
    if identities.is_empty() {
        return Err(ListenerCheck::Unreadable("no owned process to read".into()));
    }
    let mut listeners = BTreeSet::new();
    for identity in identities {
        let process = proc_root.join(identity.pid.to_string());
        let unreadable = |what: &str, error: std::io::Error| {
            ListenerCheck::Unreadable(format!(
                "{what} of {} process {}: {error}",
                identity.role, identity.pid
            ))
        };
        // A member whose `/proc/<pid>` entry is missing and that is verified
        // gone exited during the read; any other failure is unknown.
        let vanished = |error: &std::io::Error| {
            error.kind() == std::io::ErrorKind::NotFound && presence(identity) == Presence::Gone
        };
        let inodes = match socket_inodes(&process.join("fd")) {
            Ok(inodes) => inodes,
            Err(error) if vanished(&error) => {
                return Err(ListenerCheck::Vanished(identity.clone()))
            }
            Err(error) => return Err(unreadable("the descriptors", error)),
        };
        let mut tables = BTreeMap::new();
        for table in ["tcp", "tcp6"] {
            match std::fs::read_to_string(process.join("net").join(table)) {
                Ok(text) => tables.extend(listening_sockets(&text)),
                Err(error) if vanished(&error) => {
                    return Err(ListenerCheck::Vanished(identity.clone()))
                }
                // A kernel without IPv6 has no `tcp6`.
                Err(error) if table == "tcp6" && error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(unreadable("the socket table", error)),
            }
        }
        listeners.extend(inodes.iter().filter_map(|inode| {
            tables.get(inode).map(|address| OwnedListener {
                address: *address,
                role: identity.role.clone(),
                pid: identity.pid,
            })
        }));
        match presence(identity) {
            Presence::Alive => {}
            // What was read may be another process's: it proves nothing.
            Presence::Gone => return Err(ListenerCheck::Vanished(identity.clone())),
            Presence::Unknown => {
                return Err(ListenerCheck::Unreadable(format!(
                    "{} process {} changed while its sockets were read",
                    identity.role, identity.pid
                )))
            }
        }
    }
    Ok(listeners.into_iter().collect())
}

/// ADR 0031 §7: the check itself: `Ok` when every listener the owned
/// processes hold is on `127.0.0.1` or `::1`.
pub fn check_loopback(
    proc_root: &Path,
    identities: &[ProcessIdentity],
    presence: &dyn Fn(&ProcessIdentity) -> Presence,
) -> Result<(), ListenerCheck> {
    match owned_listeners(proc_root, identities, presence)?
        .into_iter()
        .find(|listener| !is_loopback(&listener.address))
    {
        Some(listener) => Err(ListenerCheck::Exposed(listener)),
        None => Ok(()),
    }
}

/// How many times [`check_engine_launch`] lists a group again after a
/// helper exited during the read.
const HELPER_REREADS: usize = 8;

/// ADR 0035 §5: the check on a built-in engine's launch on several GPUs of
/// one host, over its owned processes. A helper (ADR 0027: a compile worker
/// that exits on its own) found gone during the read is left out and the
/// rest read again; any other process that vanished, or a group that keeps
/// changing, is returned as it is (nothing is proven either way).
pub fn check_engine_launch(
    proc_root: &Path,
    identities: &[ProcessIdentity],
    presence: &dyn Fn(&ProcessIdentity) -> Presence,
) -> Result<(), ListenerCheck> {
    let mut checked = identities.to_vec();
    let mut rereads = 0;
    loop {
        match check_loopback(proc_root, &checked, presence) {
            Err(ListenerCheck::Vanished(identity))
                if identity.is_helper() && rereads < HELPER_REREADS =>
            {
                rereads += 1;
                checked.retain(|p| !p.same_process(&identity));
            }
            other => return other,
        }
    }
}

/// ADR 0035 §5, §13: the launch failure of a built-in engine's launch whose
/// owned process listens off loopback, naming the process role, address and
/// port.
pub fn engine_listener_exposed(listener: &OwnedListener) -> String {
    format!(
        "{}: the {} process {} listens on address {} port {}, not on loopback",
        capyctl_domain::local_tp::ENGINE_LISTENER_EXPOSED,
        listener.role,
        listener.pid,
        listener.address.ip(),
        listener.address.port()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/proc/net/tcp` as Linux 6.x writes it: a LISTEN row on each address,
    /// one established connection.
    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1111 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2222 1 0000000000000000 100 0 0 10 0\n\
   2: 0A00A8C0:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3333 1 0000000000000000 100 0 0 10 0\n\
   3: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 4444 1 0000000000000000 20 4 30 10 -1\n";

    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000000000000000000001000000:1F93 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5555 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000000000000000000000000000:1F94 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 6666 1 0000000000000000 100 0 0 10 0\n\
   2: 0000000000000000FFFF00000100007F:1F95 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 7777 1 0000000000000000 100 0 0 10 0\n";

    fn at(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    // T44 (ADR 0031 §7): 127.0.0.1 and ::1 pass; 0.0.0.0, :: and a LAN
    // address are exposed; a connection that is not listening is not read.
    #[test]
    #[cfg(target_endian = "little")]
    fn the_tables_parse_and_only_loopback_passes() {
        let v4 = listening_sockets(TCP);
        assert_eq!(
            v4,
            BTreeMap::from([
                (1111, at("127.0.0.1:8080")),
                (2222, at("0.0.0.0:8081")),
                (3333, at("192.168.0.10:8082")),
            ])
        );
        let v6 = listening_sockets(TCP6);
        assert_eq!(
            v6,
            BTreeMap::from([
                (5555, at("[::1]:8083")),
                (6666, at("[::]:8084")),
                (7777, at("127.0.0.1:8085")),
            ])
        );
        let passing: Vec<_> = v4
            .values()
            .chain(v6.values())
            .filter(|address| is_loopback(address))
            .copied()
            .collect();
        assert_eq!(
            passing,
            vec![at("127.0.0.1:8080"), at("[::1]:8083"), at("127.0.0.1:8085")]
        );
        assert!(
            !is_loopback(&at("127.0.0.2:1")),
            "only what {{host}} renders"
        );
    }

    #[test]
    fn malformed_rows_are_skipped() {
        assert!(listening_sockets("header\nnot a row\n  0: zz:1F90 0 0A a b c d e 1\n").is_empty());
    }

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: "api".into(),
            pid,
            boot_id: "b".into(),
            start_ticks: 1,
        }
    }

    /// A fake `/proc/<pid>` with `fd` links to `sockets` and the two tables.
    fn fake_proc(root: &Path, pid: u32, sockets: &[u64]) {
        let process = root.join(pid.to_string());
        std::fs::create_dir_all(process.join("fd")).unwrap();
        std::fs::create_dir_all(process.join("net")).unwrap();
        for (fd, inode) in sockets.iter().enumerate() {
            std::os::unix::fs::symlink(
                format!("socket:[{inode}]"),
                process.join("fd").join(fd.to_string()),
            )
            .unwrap();
        }
        std::os::unix::fs::symlink("/dev/null", process.join("fd").join("99")).unwrap();
        std::fs::write(process.join("net/tcp"), TCP).unwrap();
        std::fs::write(process.join("net/tcp6"), TCP6).unwrap();
    }

    // T44 (ADR 0031 §7): only the sockets the owned processes hold count; a
    // process that changed during the read proves nothing.
    #[test]
    #[cfg(target_endian = "little")]
    fn only_owned_sockets_are_checked() {
        let root = tempfile::tempdir().unwrap();
        fake_proc(root.path(), 10, &[1111, 5555, 4444]);
        fake_proc(root.path(), 11, &[2222]);
        let alive = |_: &ProcessIdentity| Presence::Alive;
        assert_eq!(check_loopback(root.path(), &[identity(10)], &alive), Ok(()));
        assert_eq!(
            check_loopback(root.path(), &[identity(10), identity(11)], &alive),
            Err(ListenerCheck::Exposed(OwnedListener {
                address: at("0.0.0.0:8081"),
                role: "api".into(),
                pid: 11,
            }))
        );
        assert!(matches!(
            check_loopback(root.path(), &[identity(10)], &|_| Presence::Unknown),
            Err(ListenerCheck::Unreadable(_))
        ));
        assert!(matches!(
            check_loopback(root.path(), &[identity(12)], &alive),
            Err(ListenerCheck::Unreadable(_))
        ));
        assert_eq!(
            ListenerCheck::Exposed(OwnedListener {
                address: at("0.0.0.0:8081"),
                role: "api".into(),
                pid: 11,
            })
            .to_string(),
            "custom_listener_exposed: an owned process listens on 0.0.0.0:8081, not on loopback"
        );
    }

    // T46 (ADR 0035 §5): a built-in engine's launch on several GPUs names the
    // process that listens off loopback (its role and pid), the address and
    // the port; a helper that exited during the read is left out and the
    // group read again; a rank that vanished proves nothing.
    #[test]
    #[cfg(target_endian = "little")]
    fn a_built_in_launch_names_the_exposed_process() {
        let root = tempfile::tempdir().unwrap();
        let named = |role: &str, pid: u32| ProcessIdentity {
            role: role.into(),
            ..identity(pid)
        };
        fake_proc(root.path(), 20, &[1111]);
        fake_proc(root.path(), 21, &[5555]);
        fake_proc(root.path(), 22, &[6666]);
        let alive = |_: &ProcessIdentity| Presence::Alive;
        let group = [named("api", 20), named("rank-0", 21), named("rank-1", 22)];
        let Err(ListenerCheck::Exposed(listener)) =
            check_engine_launch(root.path(), &group, &alive)
        else {
            panic!("exposed");
        };
        assert_eq!(
            engine_listener_exposed(&listener),
            "engine_listener_exposed: the rank-1 process 22 listens on address :: port 8084, \
             not on loopback"
        );
        assert_eq!(
            check_engine_launch(root.path(), &group[..2], &alive),
            Ok(())
        );
        // A helper gone during the read is left out; a rank is not.
        let gone_23 = |identity: &ProcessIdentity| {
            if identity.pid == 23 {
                Presence::Gone
            } else {
                Presence::Alive
            }
        };
        let with_helper = [named("api", 20), named("rank-0", 21), named("helper-0", 23)];
        assert_eq!(
            check_engine_launch(root.path(), &with_helper, &gone_23),
            Ok(())
        );
        let with_rank = [named("api", 20), named("rank-1", 23)];
        assert_eq!(
            check_engine_launch(root.path(), &with_rank, &gone_23),
            Err(ListenerCheck::Vanished(named("rank-1", 23)))
        );
    }

    // T45 T44 (ADR 0033 §3; found live on LMCache 0.5.6 under SGLang): a
    // member whose `/proc/<pid>` vanished and that is verified gone exited
    // during the read: it holds no socket, and the check says so
    // (`Vanished`) for the caller to list the group again. So does a member
    // read whole and gone right after. A missing entry of a process alive or
    // of unknown presence, or any other read failure, stays unknown.
    #[test]
    #[cfg(target_endian = "little")]
    fn a_member_that_vanished_is_named_not_read() {
        let root = tempfile::tempdir().unwrap();
        fake_proc(root.path(), 10, &[1111]);
        let only_10 = |identity: &ProcessIdentity| {
            if identity.pid == 10 {
                Presence::Alive
            } else {
                Presence::Gone
            }
        };
        assert_eq!(
            check_loopback(root.path(), &[identity(10), identity(13)], &only_10),
            Err(ListenerCheck::Vanished(identity(13)))
        );
        assert_eq!(
            check_loopback(root.path(), &[identity(10)], &only_10),
            Ok(())
        );
        for presence in [Presence::Alive, Presence::Unknown] {
            assert!(matches!(
                check_loopback(root.path(), &[identity(10), identity(13)], &|_| presence),
                Err(ListenerCheck::Unreadable(_))
            ));
        }
        // Read whole, then gone: what was read proves nothing.
        assert_eq!(
            check_loopback(root.path(), &[identity(10)], &|_| Presence::Gone),
            Err(ListenerCheck::Vanished(identity(10)))
        );
        // Its table gone too, after its descriptors were listed.
        fake_proc(root.path(), 14, &[2222]);
        std::fs::remove_file(root.path().join("14/net/tcp")).unwrap();
        assert_eq!(
            check_loopback(root.path(), &[identity(14)], &only_10),
            Err(ListenerCheck::Vanished(identity(14)))
        );
        // A descriptor directory that is not a directory is not a vanished
        // process.
        let odd = root.path().join("15");
        std::fs::create_dir_all(odd.join("net")).unwrap();
        std::fs::write(odd.join("fd"), "").unwrap();
        assert!(matches!(
            check_loopback(root.path(), &[identity(15)], &only_10),
            Err(ListenerCheck::Unreadable(_))
        ));
    }
}
