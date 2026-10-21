//! ADR 0031 §7: the loopback check. CapyCTL cannot validate a custom
//! engine's options, so once its probe passes the host lists the TCP
//! listening sockets the launch's owned processes hold: each process's socket
//! inodes (`/proc/<pid>/fd` links `socket:[<inode>]`) against the listening
//! rows of `/proc/<pid>/net/tcp` and `tcp6`. Any listener on an address other
//! than `127.0.0.1` or `::1` fails the launch `custom_listener_exposed`. A
//! listener opened before readiness and closed again, or opened later, is not
//! seen: this narrows the exposure, it does not prove it absent.
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use capyctl_domain::completion::ProcessIdentity;

/// The kernel's `TCP_LISTEN` state in `/proc/net/tcp`.
const TCP_LISTEN: &str = "0A";

/// Why the check did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerCheck {
    /// An owned process listens off loopback (ADR 0031 §10).
    Exposed(SocketAddr),
    /// The sockets could not be read, or a process changed while they were:
    /// nothing is proven either way.
    Unreadable(String),
}

impl std::fmt::Display for ListenerCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenerCheck::Exposed(address) => write!(
                f,
                "{}: an owned process listens on {address}, not on loopback",
                capyctl_config::custom::LISTENER_EXPOSED
            ),
            ListenerCheck::Unreadable(reason) => {
                write!(f, "the engine's listeners could not be read: {reason}")
            }
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
/// `proc_root` (`/proc` on a host). `still_owned` is asked about each process
/// after its sockets were read: a process that is no longer the recorded one
/// (gone, or its pid reused) makes the reading unreadable rather than clean.
pub fn owned_listeners(
    proc_root: &Path,
    identities: &[ProcessIdentity],
    still_owned: &dyn Fn(&ProcessIdentity) -> bool,
) -> Result<Vec<SocketAddr>, ListenerCheck> {
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
        let inodes = socket_inodes(&process.join("fd"))
            .map_err(|error| unreadable("the descriptors", error))?;
        let mut tables = BTreeMap::new();
        for table in ["tcp", "tcp6"] {
            match std::fs::read_to_string(process.join("net").join(table)) {
                Ok(text) => tables.extend(listening_sockets(&text)),
                // A kernel without IPv6 has no `tcp6`.
                Err(error) if table == "tcp6" && error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(unreadable("the socket table", error)),
            }
        }
        listeners.extend(inodes.iter().filter_map(|inode| tables.get(inode).copied()));
        if !still_owned(identity) {
            return Err(ListenerCheck::Unreadable(format!(
                "{} process {} changed while its sockets were read",
                identity.role, identity.pid
            )));
        }
    }
    Ok(listeners.into_iter().collect())
}

/// ADR 0031 §7: the check itself: `Ok` when every listener the owned
/// processes hold is on `127.0.0.1` or `::1`.
pub fn check_loopback(
    proc_root: &Path,
    identities: &[ProcessIdentity],
    still_owned: &dyn Fn(&ProcessIdentity) -> bool,
) -> Result<(), ListenerCheck> {
    match owned_listeners(proc_root, identities, still_owned)?
        .into_iter()
        .find(|address| !is_loopback(address))
    {
        Some(address) => Err(ListenerCheck::Exposed(address)),
        None => Ok(()),
    }
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
        assert_eq!(
            check_loopback(root.path(), &[identity(10)], &|_| true),
            Ok(())
        );
        assert_eq!(
            check_loopback(root.path(), &[identity(10), identity(11)], &|_| true),
            Err(ListenerCheck::Exposed(at("0.0.0.0:8081")))
        );
        assert!(matches!(
            check_loopback(root.path(), &[identity(10)], &|_| false),
            Err(ListenerCheck::Unreadable(_))
        ));
        assert!(matches!(
            check_loopback(root.path(), &[identity(12)], &|_| true),
            Err(ListenerCheck::Unreadable(_))
        ));
        assert_eq!(
            ListenerCheck::Exposed(at("0.0.0.0:8081")).to_string(),
            "custom_listener_exposed: an owned process listens on 0.0.0.0:8081, not on loopback"
        );
    }
}
