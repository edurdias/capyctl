//! ADR 0028 §7 (owner decision 10): read, never change. The checks a host
//! runs before a group member may launch on it: the member's profile build and
//! checkpoint digest, its peer address, the ports it will open, and the host
//! tuning RDMA needs. Every fact is read from a file or a syscall; nothing
//! here writes a sysctl, a limit, a device permission or a firewall rule, and
//! a port probe binds and drops at once, holding nothing.
use capyctl_config::groups_policy::GroupsPolicy;
use capyctl_domain::group::{GroupPlan, MemberRole};
use std::{
    ffi::{CStr, CString},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
    os::unix::ffi::OsStrExt,
    path::Path,
};

/// ADR 0028 §7: the tuning items a host check names (spec §16).
const COMPACTION: &str = "compaction";
const MEMLOCK: &str = "memlock";
const INFINIBAND: &str = "infiniband";

/// Whether the service user may use the host's RDMA verbs devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfinibandAccess {
    /// No `uverbs*` device node exists.
    Absent,
    /// Device nodes exist, but none is readable and writable by this user.
    NoAccess,
    /// At least one device node is readable and writable by this user.
    ReadWrite,
}

/// ADR 0028 §7: what one host's checks read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// `/proc/sys/vm/compaction_proactiveness`; `None` when the kernel has none.
    pub compaction_proactiveness: Option<u64>,
    /// The agent's soft `RLIMIT_MEMLOCK` in bytes; `None` is unlimited.
    pub memlock_soft: Option<u64>,
    pub infiniband: InfinibandAccess,
    /// Every address assigned to a local interface.
    pub local_addresses: Vec<IpAddr>,
}

/// ADR 0028 §7: read this host's facts. `root` is `/` in production; the
/// `proc` and `dev` files are read under it. The memlock limit and the local
/// addresses are the process's own and the kernel's, whatever `root` is.
pub fn read_host_facts(root: &Path) -> HostFacts {
    read_host_facts_with(root, &interfaces())
}

/// [`read_host_facts`] with the local addresses taken from `interfaces`.
fn read_host_facts_with(root: &Path, interfaces: &[(String, IpAddr)]) -> HostFacts {
    // ADR 0028 §7 (owner decision 10): read, never change.
    let compaction_proactiveness =
        std::fs::read_to_string(root.join("proc/sys/vm/compaction_proactiveness"))
            .ok()
            .and_then(|text| text.trim().parse().ok());
    HostFacts {
        compaction_proactiveness,
        memlock_soft: memlock_soft(),
        infiniband: infiniband_access(&root.join("dev/infiniband")),
        local_addresses: interfaces.iter().map(|(_, address)| *address).collect(),
    }
}

/// The soft `RLIMIT_MEMLOCK`; `None` when unlimited. A limit that cannot be
/// read counts as none at all (zero), so it is reported, never assumed fine.
fn memlock_soft() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable rlimit for the call's duration.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } != 0 {
        return Some(0);
    }
    (limit.rlim_cur != libc::RLIM_INFINITY).then_some(limit.rlim_cur)
}

/// `uverbs*` under `dir`, checked with `access(R_OK|W_OK)` for this user.
fn infiniband_access(dir: &Path) -> InfinibandAccess {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return InfinibandAccess::Absent;
    };
    let mut found = InfinibandAccess::Absent;
    for entry in entries.flatten() {
        if !entry.file_name().as_bytes().starts_with(b"uverbs") {
            continue;
        }
        found = InfinibandAccess::NoAccess;
        let Ok(path) = CString::new(entry.path().as_os_str().as_bytes()) else {
            continue;
        };
        // SAFETY: `path` is a valid NUL-terminated string; `access` only reads it.
        if unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) } == 0 {
            return InfinibandAccess::ReadWrite;
        }
    }
    found
}

/// Every (interface name, address) pair assigned on this host, from
/// `getifaddrs`. Empty when it cannot be read.
fn interfaces() -> Vec<(String, IpAddr)> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a valid out pointer; the list is freed below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Vec::new();
    }
    let mut pairs = Vec::new();
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a node of the list getifaddrs returned, alive
        // until freeifaddrs; each address is read as the family it declares.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
            continue;
        }
        let address = match i32::from(unsafe { (*entry.ifa_addr).sa_family }) {
            libc::AF_INET => {
                let ipv4 = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
                IpAddr::V4(Ipv4Addr::from(u32::from_be(ipv4.sin_addr.s_addr)))
            }
            libc::AF_INET6 => {
                let ipv6 = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in6) };
                IpAddr::V6(Ipv6Addr::from(ipv6.sin6_addr.s6_addr))
            }
            _ => continue,
        };
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        pairs.push((name, address));
    }
    // SAFETY: `head` came from a successful getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(head) };
    pairs
}

/// R11 (ADR 0028 §10): the interface that owns `address` on this host, the
/// name a group launch's socket interface is resolved from. `None` when no
/// local interface holds it.
pub fn interface_for(address: IpAddr) -> Option<String> {
    interface_in(&interfaces(), address)
}

fn interface_in(interfaces: &[(String, IpAddr)], address: IpAddr) -> Option<String> {
    let address = address.to_canonical();
    interfaces
        .iter()
        .find(|(_, local)| local.to_canonical() == address)
        .map(|(name, _)| name.clone())
}

type InterfaceSource = dyn Fn() -> Vec<(String, IpAddr)> + Send + Sync;
type PortProbe = dyn Fn(IpAddr, u16) -> bool + Send + Sync;

/// ADR 0028 §7, §10 (R11): where a host's group checks read its interfaces
/// and probe its ports: `getifaddrs` and [`port_free`] on a real host. A test
/// host substitutes both, so a launch can be checked against documentation
/// addresses no machine holds.
#[derive(Clone)]
pub struct HostProbes {
    interfaces: std::sync::Arc<InterfaceSource>,
    port_free: std::sync::Arc<PortProbe>,
}

impl Default for HostProbes {
    fn default() -> Self {
        Self {
            interfaces: std::sync::Arc::new(interfaces),
            port_free: std::sync::Arc::new(port_free),
        }
    }
}

impl HostProbes {
    /// Probes reading `interfaces` and asking `port_free`. Tests only.
    #[doc(hidden)]
    pub fn with(
        interfaces: impl Fn() -> Vec<(String, IpAddr)> + Send + Sync + 'static,
        port_free: impl Fn(IpAddr, u16) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            interfaces: std::sync::Arc::new(interfaces),
            port_free: std::sync::Arc::new(port_free),
        }
    }
    /// This host's facts, its local addresses from these probes.
    pub fn facts(&self, root: &Path) -> HostFacts {
        read_host_facts_with(root, &(self.interfaces)())
    }
    /// R11: the interface holding `address`, if any.
    pub fn interface_for(&self, address: IpAddr) -> Option<String> {
        interface_in(&(self.interfaces)(), address)
    }
    /// Whether `port` is free for a listener on `address` ([`port_free`]).
    pub fn port_free(&self, address: IpAddr, port: u16) -> bool {
        (self.port_free)(address, port)
    }
}

/// R33 (ADR 0028 §5, §13): the ports SGLang 0.5.21 derives from its
/// rendezvous port `port` on the head when DP attention is on: six from
/// `port + 1`, or from `port - 7` when `port + 7` would pass 65535. `None`
/// when no such run of ports exists (no `u16` wrap).
pub fn dp_attention_ports(port: u16) -> Option<std::ops::RangeInclusive<u16>> {
    let base = if port.checked_add(7).is_none() {
        port.checked_sub(7)?
    } else {
        port.checked_add(1)?
    };
    Some(base..=base.checked_add(5)?)
}

/// R33: on a SGLang head with DP attention, the first derived port held
/// outside CapyCTL, as `rendezvous_port_in_use:<port>`. Nothing for any other
/// member, engine or a group without DP attention.
pub fn derived_ports_held(
    plan: &GroupPlan,
    host_id: &str,
    dp_attention: bool,
    port_free: impl Fn(IpAddr, u16) -> bool,
) -> Result<(), String> {
    let Some(member) = plan.members().iter().find(|m| m.member.host_id == host_id) else {
        return Ok(());
    };
    if !dp_attention
        || member.role != MemberRole::Head
        || plan.engine() != capyctl_domain::group::GroupEngine::Sglang
    {
        return Ok(());
    }
    let ports = dp_attention_ports(plan.rendezvous_port())
        .ok_or_else(|| format!("rendezvous_port_in_use:{}", plan.rendezvous_port()))?;
    match ports
        .into_iter()
        .find(|port| !port_free(member.peer_address, *port))
    {
        Some(port) => Err(format!("rendezvous_port_in_use:{port}")),
        None => Ok(()),
    }
}

/// ADR 0028 §7: the outcome of the host checks. `warnings` never stop a
/// member; a `refusal` is one closed code and stops it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckVerdict {
    pub warnings: Vec<String>,
    pub refusal: Option<String>,
}

/// Every tuning finding in item order: a warning for each gap, except that
/// under `require_rdma` the memlock and infiniband gaps are refusals.
/// Compaction only ever warns (R16).
pub fn tuning_findings(facts: &HostFacts, policy: &GroupsPolicy) -> Vec<String> {
    let gaps = [
        (
            COMPACTION,
            facts
                .compaction_proactiveness
                .is_some_and(|value| value != 0),
            false,
        ),
        (MEMLOCK, facts.memlock_soft.is_some(), policy.require_rdma),
        (
            INFINIBAND,
            facts.infiniband != InfinibandAccess::ReadWrite,
            policy.require_rdma,
        ),
    ];
    gaps.into_iter()
        .filter(|(_, gap, _)| *gap)
        .map(|(item, _, refuses)| {
            if refuses {
                format!("host_tuning_missing:{item}")
            } else {
                format!("host_tuning_warning:{item}")
            }
        })
        .collect()
}

/// ADR 0028 §7: judge the host tuning. Compaction warns; memlock below
/// unlimited and infiniband not read-write warn, or refuse under
/// `require_rdma`, the first refusal in the order memlock, infiniband.
pub fn evaluate(facts: &HostFacts, policy: &GroupsPolicy) -> CheckVerdict {
    let mut verdict = CheckVerdict::default();
    for finding in tuning_findings(facts, policy) {
        if finding.starts_with("host_tuning_missing:") {
            verdict.refusal.get_or_insert(finding);
        } else {
            verdict.warnings.push(finding);
        }
    }
    verdict
}

/// ADR 0028 §3: whether `policy` declares a peer address held by a local
/// interface.
pub fn peer_address_local(facts: &HostFacts, policy: &GroupsPolicy) -> bool {
    policy.peer_address.is_some_and(|peer| {
        let peer = peer.to_canonical();
        facts
            .local_addresses
            .iter()
            .any(|local| local.to_canonical() == peer)
    })
}

/// ADR 0028 §7: the checks one member runs on its host before it may launch.
/// In order: one rank per member (else `group_topology_invalid`, ADR 0028 §2);
/// the plan names this host (else `group_profile_mismatch`); the
/// member's peer address is the host's own declared one and is local
/// (`peer_address_not_local`); its profile resolves with the recorded build
/// (`group_profile_mismatch`); its model path measures to the recorded digest
/// (`group_checkpoint_mismatch`); on the head, the rendezvous port is free on
/// the peer address and the service port on loopback; on a SGLang worker, its
/// loopback port is free; then the tuning. The `Err` is one closed code
/// (spec §16).
pub fn prepare_member(
    plan: &GroupPlan,
    host_id: &str,
    facts: &HostFacts,
    policy: &GroupsPolicy,
    port_free: impl Fn(IpAddr, u16) -> bool,
    digest_of: impl Fn(&str) -> Option<String>,
    profile_fingerprint: impl Fn(&str) -> Option<String>,
) -> Result<CheckVerdict, String> {
    // ADR 0028 §2 (R35): in this version every member contributes exactly one
    // rank (W/N = 1); configuration refuses anything else, and a plan that
    // still names more is refused here, typed, before any other check.
    if plan.topology().local_ranks != 1 {
        return Err("group_topology_invalid".into());
    }
    let Some(member) = plan.members().iter().find(|m| m.member.host_id == host_id) else {
        return Err("group_profile_mismatch".into());
    };
    if policy.peer_address.map(|peer| peer.to_canonical())
        != Some(member.peer_address.to_canonical())
        || !peer_address_local(facts, policy)
    {
        return Err("peer_address_not_local".into());
    }
    if profile_fingerprint(&member.profile_name).as_deref()
        != Some(member.profile_fingerprint.as_str())
    {
        return Err("group_profile_mismatch".into());
    }
    // An unmeasured path has no digest, so it cannot agree with the record.
    if digest_of(&member.model_path).as_deref() != Some(member.checkpoint_fingerprint.as_str()) {
        return Err("group_checkpoint_mismatch".into());
    }
    // Review Focus 2: a port taken outside CapyCTL (a stale engine from before
    // a crash) refuses here, named, instead of hanging the launch.
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    if member.role == MemberRole::Head {
        let rendezvous = plan.rendezvous_port();
        if !port_free(member.peer_address, rendezvous) {
            return Err(format!("rendezvous_port_in_use:{rendezvous}"));
        }
        if let Some(port) = member
            .service_port
            .filter(|port| !port_free(loopback, *port))
        {
            return Err(format!("service_port_in_use:{port}"));
        }
    }
    if let Some(port) = member
        .worker_port
        .filter(|port| !port_free(loopback, *port))
    {
        return Err(format!("service_port_in_use:{port}"));
    }
    let verdict = evaluate(facts, policy);
    match verdict.refusal {
        Some(refusal) => Err(refusal),
        None => Ok(verdict),
    }
}

/// ADR 0028 §5, §7 (R15): whether `port` is free for a listener on `address`.
/// The torch store binds every interface, so the port must bind on `address`
/// and on both wildcards (`0.0.0.0` and `::`, the latter skipped on a host
/// without IPv6). Each probe binds and drops at once; nothing is held.
pub fn port_free(address: IpAddr, port: u16) -> bool {
    let bind = |ip: IpAddr| TcpListener::bind(SocketAddr::new(ip, port)).map(drop);
    if bind(address).is_err() || bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).is_err() {
        return false;
    }
    match bind(IpAddr::V6(Ipv6Addr::UNSPECIFIED)) {
        Ok(()) => true,
        Err(error) => matches!(
            error.raw_os_error(),
            Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_config::groups_policy::GroupsPolicy;
    use capyctl_domain::group::{
        member_id, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan, MemberRole,
    };
    use std::net::IpAddr;

    /// Host A heads at 192.0.2.10 (service port 8100, rendezvous 25000); host
    /// B works at 192.0.2.11, with loopback port 8101 under SGLang.
    fn sample_plan(engine: GroupEngine) -> GroupPlan {
        let member = |rank: u32, host: &str, peer: &str| MemberPlan {
            member: MemberKey {
                host_id: host.into(),
                member_id: member_id(rank),
            },
            rank,
            role: if rank == 0 {
                MemberRole::Head
            } else {
                MemberRole::Worker
            },
            profile_name: "local".into(),
            profile_fingerprint: "pinned".into(),
            checkpoint_fingerprint: "sha256:c".into(),
            model_path: "/models/toy".into(),
            devices: vec!["gpu0".into()],
            peer_address: peer.parse().unwrap(),
            service_port: (rank == 0).then_some(8100),
            worker_port: (rank != 0 && engine == GroupEngine::Sglang).then_some(8101),
        };
        GroupPlan::new(
            engine,
            vec![
                member(0, "host-a", "192.0.2.10"),
                member(1, "host-b", "192.0.2.11"),
            ],
            GroupTopology {
                tensor_parallel: 2,
                pipeline_parallel: 1,
                local_ranks: 1,
            },
            25000,
            1,
        )
        .unwrap()
    }

    /// Facts with nothing to find, on a host holding `address`.
    fn facts_for(address: &str) -> HostFacts {
        HostFacts {
            compaction_proactiveness: Some(0),
            memlock_soft: None,
            infiniband: InfinibandAccess::ReadWrite,
            local_addresses: vec![address.parse().unwrap()],
        }
    }

    fn policy_with(address: &str) -> GroupsPolicy {
        GroupsPolicy {
            peer_address: Some(address.parse().unwrap()),
            ..Default::default()
        }
    }

    // T29: findings warn by default and refuse only under require_rdma (compaction never refuses).
    #[test]
    fn findings_warn_by_default_and_refuse_under_require_rdma() {
        let facts = HostFacts {
            compaction_proactiveness: Some(20),
            memlock_soft: Some(8 << 20),
            infiniband: InfinibandAccess::NoAccess,
            local_addresses: vec!["192.0.2.10".parse().unwrap()],
        };
        let lax = evaluate(&facts, &GroupsPolicy::default());
        assert_eq!(
            lax.warnings,
            [
                "host_tuning_warning:compaction",
                "host_tuning_warning:memlock",
                "host_tuning_warning:infiniband"
            ]
        );
        assert_eq!(lax.refusal, None);
        let strict = evaluate(
            &facts,
            &GroupsPolicy {
                require_rdma: true,
                ..Default::default()
            },
        );
        assert_eq!(
            strict.refusal.as_deref(),
            Some("host_tuning_missing:memlock")
        );
        assert_eq!(strict.warnings, ["host_tuning_warning:compaction"]);
        // T29: with memlock unlimited the next refusal is infiniband.
        let rdma_only = HostFacts {
            memlock_soft: None,
            ..facts
        };
        let strict = evaluate(
            &rdma_only,
            &GroupsPolicy {
                require_rdma: true,
                ..Default::default()
            },
        );
        assert_eq!(
            strict.refusal.as_deref(),
            Some("host_tuning_missing:infiniband")
        );
    }

    // T29: facts are read from files and never written.
    #[test]
    fn facts_are_read_only() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proc/sys/vm")).unwrap();
        let path = root.path().join("proc/sys/vm/compaction_proactiveness");
        std::fs::write(&path, "0\n").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let facts = read_host_facts(root.path());
        assert_eq!(facts.compaction_proactiveness, Some(0));
        assert_eq!(facts.infiniband, InfinibandAccess::Absent);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0\n");
    }

    // T29: a uverbs device the service user may read and write is ReadWrite;
    // one it may not is NoAccess.
    #[test]
    fn infiniband_access_is_read_from_the_device_nodes() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev/infiniband");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("rdma_cm"), "").unwrap();
        assert_eq!(
            read_host_facts(root.path()).infiniband,
            InfinibandAccess::Absent
        );
        std::fs::write(dev.join("uverbs0"), "").unwrap();
        std::fs::set_permissions(dev.join("uverbs0"), std::fs::Permissions::from_mode(0o400))
            .unwrap();
        // Root may write anything; the check only means something otherwise.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(
                read_host_facts(root.path()).infiniband,
                InfinibandAccess::NoAccess
            );
        }
        std::fs::set_permissions(dev.join("uverbs0"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert_eq!(
            read_host_facts(root.path()).infiniband,
            InfinibandAccess::ReadWrite
        );
    }

    // R11: every local address maps back to the interface that owns it.
    #[test]
    fn local_addresses_name_their_interface() {
        let facts = read_host_facts(std::path::Path::new("/"));
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(facts.local_addresses.contains(&loopback));
        assert!(interface_for(loopback).is_some());
        assert_eq!(interface_for("192.0.2.254".parse().unwrap()), None);
    }

    // Review Focus 2: an occupied rendezvous port on the head refuses Prepare, naming the port.
    #[test]
    fn occupied_rendezvous_port_refuses_prepare() {
        let plan = sample_plan(GroupEngine::Vllm); // head host-a 192.0.2.10, port 25000
        let err = prepare_member(
            &plan,
            "host-a",
            &facts_for("192.0.2.10"),
            &policy_with("192.0.2.10"),
            |_, port| port != 25000,
            |_| Some("sha256:c".into()),
            |_| Some("pinned".into()),
        )
        .unwrap_err();
        assert_eq!(err, "rendezvous_port_in_use:25000");
        // The head's loopback service port is checked too.
        let err = prepare_member(
            &plan,
            "host-a",
            &facts_for("192.0.2.10"),
            &policy_with("192.0.2.10"),
            |_, port| port != 8100,
            |_| Some("sha256:c".into()),
            |_| Some("pinned".into()),
        )
        .unwrap_err();
        assert_eq!(err, "service_port_in_use:8100");
    }

    // Review Focus 2: an occupied SGLang worker loopback port refuses Prepare on the worker.
    #[test]
    fn occupied_worker_port_refuses_prepare() {
        let plan = sample_plan(GroupEngine::Sglang); // worker host-b 192.0.2.11, worker_port 8101
        let err = prepare_member(
            &plan,
            "host-b",
            &facts_for("192.0.2.11"),
            &policy_with("192.0.2.11"),
            |_, port| port != 8101,
            |_| Some("sha256:c".into()),
            |_| Some("pinned".into()),
        )
        .unwrap_err();
        assert_eq!(err, "service_port_in_use:8101");
    }

    // T30: a peer address not on this host, a digest mismatch and a profile mismatch are refused.
    #[test]
    fn prepare_refuses_address_digest_and_profile_mismatch() {
        let plan = sample_plan(GroupEngine::Vllm);
        let ok = |_, _| true;
        assert_eq!(
            prepare_member(
                &plan,
                "host-b",
                &facts_for("192.0.2.99"),
                &policy_with("192.0.2.11"),
                ok,
                |_| Some("sha256:c".into()),
                |_| Some("pinned".into())
            )
            .unwrap_err(),
            "peer_address_not_local"
        );
        // The policy names another address than the plan does.
        assert_eq!(
            prepare_member(
                &plan,
                "host-b",
                &facts_for("192.0.2.11"),
                &policy_with("192.0.2.12"),
                ok,
                |_| Some("sha256:c".into()),
                |_| Some("pinned".into())
            )
            .unwrap_err(),
            "peer_address_not_local"
        );
        assert_eq!(
            prepare_member(
                &plan,
                "host-b",
                &facts_for("192.0.2.11"),
                &policy_with("192.0.2.11"),
                ok,
                |_| Some("sha256:x".into()),
                |_| Some("pinned".into())
            )
            .unwrap_err(),
            "group_checkpoint_mismatch"
        );
        // An unmeasured path has no digest to agree with.
        assert_eq!(
            prepare_member(
                &plan,
                "host-b",
                &facts_for("192.0.2.11"),
                &policy_with("192.0.2.11"),
                ok,
                |_| None,
                |_| Some("pinned".into())
            )
            .unwrap_err(),
            "group_checkpoint_mismatch"
        );
        assert_eq!(
            prepare_member(
                &plan,
                "host-b",
                &facts_for("192.0.2.11"),
                &policy_with("192.0.2.11"),
                ok,
                |_| Some("sha256:c".into()),
                |_| Some("other".into())
            )
            .unwrap_err(),
            "group_profile_mismatch"
        );
        assert_eq!(
            prepare_member(
                &plan,
                "host-b",
                &facts_for("192.0.2.11"),
                &policy_with("192.0.2.11"),
                ok,
                |_| Some("sha256:c".into()),
                |_| None
            )
            .unwrap_err(),
            "group_profile_mismatch"
        );
        assert!(prepare_member(
            &plan,
            "host-c",
            &facts_for("192.0.2.12"),
            &policy_with("192.0.2.12"),
            ok,
            |_| Some("sha256:c".into()),
            |_| Some("pinned".into())
        )
        .is_err());
        // T29 T30: a clean member passes with its warnings; under require_rdma
        // a tuning gap refuses.
        let mut facts = facts_for("192.0.2.11");
        facts.compaction_proactiveness = Some(20);
        facts.memlock_soft = Some(64 << 10);
        let verdict = prepare_member(
            &plan,
            "host-b",
            &facts,
            &policy_with("192.0.2.11"),
            ok,
            |_| Some("sha256:c".into()),
            |_| Some("pinned".into()),
        )
        .unwrap();
        assert_eq!(
            verdict.warnings,
            [
                "host_tuning_warning:compaction",
                "host_tuning_warning:memlock"
            ]
        );
        let strict = GroupsPolicy {
            require_rdma: true,
            ..policy_with("192.0.2.11")
        };
        assert_eq!(
            prepare_member(
                &plan,
                "host-b",
                &facts,
                &strict,
                ok,
                |_| Some("sha256:c".into()),
                |_| Some("pinned".into())
            )
            .unwrap_err(),
            "host_tuning_missing:memlock"
        );
    }

    // Review Focus 2 (R15): a port held on the wildcard is not free on any
    // one address, and a port held on loopback is not free there; a free port
    // is, and probing it holds nothing afterwards.
    #[test]
    fn the_port_probe_sees_wildcard_and_named_holders() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let wildcard = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = wildcard.local_addr().unwrap().port();
        assert!(!port_free(loopback, port));
        drop(wildcard);
        let named = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = named.local_addr().unwrap().port();
        assert!(!port_free(loopback, port));
        drop(named);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(port_free(loopback, port));
        // Bind and drop: the probe left nothing bound.
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    /// `plan` on rendezvous port `port`.
    fn on_port(plan: &GroupPlan, port: u16) -> GroupPlan {
        GroupPlan::new(
            plan.engine(),
            plan.members().to_vec(),
            plan.topology(),
            port,
            plan.generation(),
        )
        .unwrap()
    }

    // R33 (ADR 0028 §5, §13), Review Focus 2: with DP attention a SGLang head
    // also needs the six ports SGLang 0.5.21 derives from the rendezvous port:
    // from P+1, or from P-7 when P+7 passes 65535. The first one held refuses,
    // named; nothing is probed on a worker, on another engine, or without DP
    // attention.
    #[test]
    fn dp_attention_probes_the_derived_head_ports() {
        assert_eq!(dp_attention_ports(25000), Some(25001..=25006));
        assert_eq!(dp_attention_ports(65528), Some(65529..=65534));
        assert_eq!(dp_attention_ports(65529), Some(65522..=65527));
        assert_eq!(dp_attention_ports(65535), Some(65528..=65533));
        assert_eq!(dp_attention_ports(3), Some(4..=9));
        let sglang = sample_plan(GroupEngine::Sglang);
        let probed = std::cell::RefCell::new(Vec::new());
        let held = |held: u16| {
            let probed = &probed;
            move |address: IpAddr, port: u16| {
                assert_eq!(address, "192.0.2.10".parse::<IpAddr>().unwrap());
                probed.borrow_mut().push(port);
                port != held
            }
        };
        assert_eq!(
            derived_ports_held(&sglang, "host-a", true, held(25003)),
            Err("rendezvous_port_in_use:25003".into())
        );
        assert_eq!(*probed.borrow(), [25001, 25002, 25003]);
        probed.borrow_mut().clear();
        assert_eq!(
            derived_ports_held(&on_port(&sglang, 65530), "host-a", true, held(65525)),
            Err("rendezvous_port_in_use:65525".into())
        );
        assert_eq!(*probed.borrow(), [65523, 65524, 65525]);
        probed.borrow_mut().clear();
        assert_eq!(
            derived_ports_held(&on_port(&sglang, 65530), "host-a", true, held(1)),
            Ok(())
        );
        assert_eq!(*probed.borrow(), (65523..=65528).collect::<Vec<_>>());
        probed.borrow_mut().clear();
        for (plan, host, dp) in [
            (&sglang, "host-a", false),
            (&sglang, "host-b", true),
            (&sample_plan(GroupEngine::Vllm), "host-a", true),
        ] {
            assert_eq!(derived_ports_held(plan, host, dp, held(25001)), Ok(()));
        }
        assert!(probed.borrow().is_empty(), "nothing extra is probed");
    }

    // T03, R35 (ADR 0028 §2): a plan giving a member more than one rank is
    // refused typed, before any other check, on every member.
    #[test]
    fn more_than_one_rank_per_member_is_refused() {
        let one = sample_plan(GroupEngine::Vllm);
        let mut members = one.members().to_vec();
        for member in &mut members {
            member.devices = vec!["gpu0".into(), "gpu1".into()];
        }
        let two = GroupPlan::new(
            GroupEngine::Vllm,
            members,
            GroupTopology {
                tensor_parallel: 4,
                pipeline_parallel: 1,
                local_ranks: 2,
            },
            one.rendezvous_port(),
            one.generation(),
        )
        .unwrap();
        for (host, address) in [("host-a", "192.0.2.10"), ("host-b", "192.0.2.11")] {
            assert_eq!(
                prepare_member(
                    &two,
                    host,
                    &facts_for(address),
                    &policy_with(address),
                    |_, _| true,
                    |_| Some("sha256:c".into()),
                    |_| Some("pinned".into()),
                ),
                Err("group_topology_invalid".into())
            );
        }
    }

    // R11, R33: substituted probes answer the checks instead of the host.
    #[test]
    fn substituted_probes_answer_for_the_host() {
        let probes = HostProbes::with(
            || vec![("eth9".into(), "192.0.2.11".parse().unwrap())],
            |_, port| port != 8101,
        );
        assert_eq!(
            probes
                .interface_for("192.0.2.11".parse().unwrap())
                .as_deref(),
            Some("eth9")
        );
        assert_eq!(probes.interface_for("192.0.2.12".parse().unwrap()), None);
        assert!(!probes.port_free("127.0.0.1".parse().unwrap(), 8101));
        assert_eq!(
            probes.facts(Path::new("/")).local_addresses,
            vec!["192.0.2.11".parse::<IpAddr>().unwrap()]
        );
    }
}
