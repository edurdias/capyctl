//! ADR 0028 §5, §6, §11: group plans, per-member owners, all-or-nothing
//! reservation, rendezvous and worker ports, and per-host checkpoint digests.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;

use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::group::{
    member_id, GroupEngine, GroupIdentityError, GroupPlan, GroupTopology, MemberKey, MemberPlan,
    MemberRole,
};
use capyctl_domain::resources::*;
use capyctl_domain::{DeploymentId, LifecycleState, OperationId};
use capyctl_scheduler::residency::{AdmissionContext, ResourceError};
use capyctl_store::groups::{
    member_owner_id, parse_member_owner_id, GroupReservation, GroupSettlement, GroupStoreError,
    MemberGone, MemberRow, MemberState,
};
use capyctl_store::instances::instance_owner_id;
use capyctl_store::resource_ledger::{GrantRequest, ResourceStoreError};
use capyctl_store::{AcceptDeployment, Store};
use rusqlite::{params, Connection};

// ---- fixtures -------------------------------------------------------------

/// The deployments every fixture store accepts, addressed in tests by name.
const NAMES: &[&str] = &["g", "g1", "g2", "g3", "d"];
const NOW_MS: i64 = 1_000;

fn gib(n: i64) -> i64 {
    n << 30
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// The memory domain of one host in the shared ledger.
fn domain(host: &str) -> String {
    format!("{host}/system")
}

/// The GPU of one host in the shared ledger.
fn device(host: &str) -> String {
    format!("{host}/gpu0")
}

/// A file-backed store shared across threads: every call opens its own
/// connection, as the coordinator's concurrent workers do. Tests address
/// deployments by name; the fixture maps each to its accepted id.
#[derive(Clone)]
struct Fixture {
    _dir: Arc<tempfile::TempDir>,
    path: PathBuf,
    ids: Arc<BTreeMap<String, String>>,
    hosts: Arc<Vec<(String, i64)>>,
}

/// Fresh observations of every member host, gathered by the caller at
/// `NOW_MS`, with each host's `max_parked`. `swapped` hands the first two
/// members each other's context.
struct Fresh {
    max_parked: usize,
    swapped: bool,
}
fn ctx() -> Fresh {
    Fresh {
        max_parked: 4,
        swapped: false,
    }
}

fn store_with(hosts: &[(&str, i64)]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let store = Store::open(&path).unwrap();
    let mut ids = BTreeMap::new();
    for name in NAMES {
        let id = DeploymentId::new();
        store
            .accept_deployment(AcceptDeployment {
                id,
                name: (*name).into(),
                kind: "model".into(),
                route_model_id: None,
                desired_state: LifecycleState::Stopped,
                schema_version: 1,
                idempotency_key: (*name).into(),
                initial_operation_id: OperationId(format!("op-{name}")),
            })
            .unwrap();
        ids.insert((*name).to_owned(), id.to_string());
    }
    // The host-scoped key registry the publication path fills (SPEC §7): host-a
    // is the embedded host, named by its configured name while its namespace
    // id is opaque; the others are enrolled hosts named by their ids.
    let sql = Connection::open(&path).unwrap();
    for (host, _) in hosts {
        let (namespace, kind) = if *host == "host-a" {
            ("ns-embedded", "embedded")
        } else {
            (*host, "remote")
        };
        sql.execute(
            "INSERT INTO host_resource_namespaces(host_id,policy_key,kind) VALUES(?1,?2,?3)",
            params![namespace, host, kind],
        )
        .unwrap();
        for (kind, local, key) in [
            ("domain", "system", domain(host)),
            ("device", "gpu0", device(host)),
        ] {
            sql.execute(
                "INSERT INTO host_resource_keys(host_id,kind,local_id,ledger_key) VALUES(?1,?2,?3,?4)",
                params![namespace, kind, local, key],
            )
            .unwrap();
        }
    }
    Fixture {
        _dir: Arc::new(dir),
        path,
        ids: Arc::new(ids),
        hosts: Arc::new(hosts.iter().map(|(h, b)| ((*h).into(), *b)).collect()),
    }
}

fn two_host_store(a: i64, b: i64) -> Fixture {
    store_with(&[("host-a", a), ("host-b", b)])
}

fn shared_store_three_hosts(a: i64, b: i64, c: i64) -> Fixture {
    store_with(&[("host-a", a), ("host-b", b), ("host-c", c)])
}

impl Fixture {
    fn open(&self) -> Store {
        Store::open(&self.path).unwrap()
    }
    fn id(&self, name: &str) -> String {
        self.ids[name].clone()
    }
    /// The real owner id of a name-addressed member owner.
    fn real_owner(&self, owner: &str) -> String {
        let (name, instance, rank) = parse_member_owner_id(owner).expect("member owner");
        member_owner_id(&self.id(&name), instance, rank)
    }
    /// A parked owner of instance `instance` of `d`, charged 1 GiB on `host`.
    fn park_owner(&self, host: &str, instance: u32) {
        let id = self.id("d");
        let footprint = format!(
            r#"{{"version":1,"phase":"parked","allocations":[["{}",{},0]],"devices":[]}}"#,
            domain(host),
            gib(1)
        );
        Connection::open(&self.path)
            .unwrap()
            .execute(
                "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index) VALUES(?1,?2,?3,?4)",
                params![instance_owner_id(&id, instance), footprint, id, instance],
            )
            .unwrap();
    }

    fn reserve_group(
        &self,
        r: &GroupReservation,
        plan: PlanFor,
        fresh: Fresh,
    ) -> Result<GroupPlan, GroupStoreError> {
        let store = self.open();
        let id = self.id(&r.deployment_id);
        let epoch = store.resource_snapshot().unwrap().epoch;
        let members = r
            .members
            .iter()
            .enumerate()
            .map(|(rank, (host, wanted))| {
                (
                    host.clone(),
                    GrantRequest {
                        // One grant per reservation attempt: a retry in the
                        // same generation is a new grant at a later epoch.
                        id: format!("grant-{id}-{rank}-{epoch}"),
                        owner_id: member_owner_id(&id, r.instance_index, rank as u32),
                        deployment_id: id.clone(),
                        operation_id: format!("op-{}", r.deployment_id),
                        revision: 1,
                        generation: 1,
                        expected_epoch: epoch,
                        next: wanted.next.clone(),
                    },
                )
            })
            .collect();
        let real = GroupReservation {
            deployment_id: id,
            members,
            ..r.clone()
        };
        // One context per member host, from that host's own observation,
        // limit and `max_parked` (ADR 0028 §12).
        let per_host: Vec<(String, [MemoryObservation; 1], [MemoryLimit; 1])> = r
            .members
            .iter()
            .map(|(host, _)| {
                let bytes = self
                    .hosts
                    .iter()
                    .find(|(known, _)| known == host)
                    .map_or(0, |(_, bytes)| *bytes);
                (
                    host.clone(),
                    [MemoryObservation {
                        domain: domain(host),
                        capacity_bytes: bytes,
                        available_bytes: bytes,
                        sampled_at_ms: NOW_MS,
                    }],
                    [MemoryLimit {
                        domain: domain(host),
                        managed_bytes: bytes,
                        free_reserve_bytes: 0,
                        reserve_absorbs_unmanaged: false,
                        host_kv_bytes: None,
                        parked_bytes: None,
                    }],
                )
            })
            .collect();
        let mut contexts: BTreeMap<String, AdmissionContext<'_>> = per_host
            .iter()
            .map(|(host, observed, limit)| {
                (
                    host.clone(),
                    AdmissionContext::new(observed, limit, NOW_MS, 60_000, fresh.max_parked),
                )
            })
            .collect();
        if fresh.swapped {
            // The first two members' contexts exchanged: each names the other
            // host's domain.
            let (a, b) = (&r.members[0].0, &r.members[1].0);
            let (first, second) = (contexts[a], contexts[b]);
            contexts.insert(a.clone(), second);
            contexts.insert(b.clone(), first);
        }
        let hosts: Vec<String> = r.members.iter().map(|(host, _)| host.clone()).collect();
        let plan_for = move |port: u16, workers: &BTreeMap<String, u16>| {
            build_plan(plan.0, &hosts, port, workers)
        };
        store.reserve_group(&real, plan_for, &contexts)
    }
    fn settle_member(
        &self,
        name: &str,
        instance: u32,
        generation: i64,
        rank: u32,
        evidence: MemberGone,
    ) -> Result<GroupSettlement, GroupStoreError> {
        self.open()
            .settle_member(&self.id(name), instance, generation, rank, evidence)
    }
    fn mark_member_uncertain(
        &self,
        name: &str,
        instance: u32,
        generation: i64,
        rank: u32,
    ) -> Result<(), GroupStoreError> {
        self.open()
            .mark_member_uncertain(&self.id(name), instance, generation, rank)
    }
    fn mark_member_dispatching(&self, name: &str, rank: u32) -> Result<(), GroupStoreError> {
        self.open()
            .mark_member_dispatching(&self.id(name), 0, 1, rank)
    }
    fn mark_member_launched(
        &self,
        name: &str,
        rank: u32,
        identities: &[ProcessIdentity],
    ) -> Result<(), GroupStoreError> {
        self.open()
            .mark_member_launched(&self.id(name), 0, 1, rank, identities)
    }
    fn group_plan(
        &self,
        name: &str,
        instance: u32,
    ) -> Result<Option<(GroupPlan, Vec<MemberRow>)>, GroupStoreError> {
        self.open().group_plan(&self.id(name), instance)
    }
    fn exclude_rendezvous_port(&self, host: &str, port: u16, until_ms: i64) {
        self.open()
            .exclude_rendezvous_port(host, port, until_ms)
            .unwrap()
    }
    fn owner_bytes(&self, owner: &str) -> i64 {
        self.open()
            .resource_snapshot()
            .unwrap()
            .owners
            .get(&self.real_owner(owner))
            .map(|f| f.allocations.iter().map(|a| a.bytes).sum())
            .unwrap_or(0)
    }
    fn endpoint_leased(&self, host: &str, port: u16) -> bool {
        self.open().endpoint_port_leased(host, port).unwrap()
    }
    fn record_digest(
        &self,
        name: &str,
        revision: i64,
        host: &str,
        digest: &str,
    ) -> Result<(), capyctl_store::checkpoint_digests::CheckpointDigestError> {
        self.open()
            .record_digest(&self.id(name), revision, host, digest)
    }
    fn digests_for(
        &self,
        name: &str,
        revision: i64,
    ) -> Result<BTreeMap<String, String>, capyctl_store::checkpoint_digests::CheckpointDigestError>
    {
        self.open().digests_for(&self.id(name), revision)
    }
}

/// A reservation of `members` (host, bytes) for instance 0 of `name`, headed by
/// the first host. The fixture fills each grant's fence when it reserves.
fn reservation(name: &str, members: &[(&str, i64)]) -> GroupReservation {
    GroupReservation {
        deployment_id: name.into(),
        instance_index: 0,
        members: members
            .iter()
            .map(|(host, bytes)| {
                (
                    (*host).to_owned(),
                    GrantRequest {
                        id: String::new(),
                        owner_id: String::new(),
                        deployment_id: name.into(),
                        operation_id: String::new(),
                        revision: 1,
                        generation: 1,
                        expected_epoch: 0,
                        next: PhaseFootprint {
                            phase: ResourcePhase::Cold,
                            allocations: vec![Allocation {
                                domain: domain(host),
                                bytes: *bytes,
                                host_kv_bytes: 0,
                            }],
                            devices: vec![],
                        },
                    },
                )
            })
            .collect(),
        head_host: members[0].0.into(),
        port_range: 25000..=25099,
        worker_ports: BTreeMap::new(),
    }
}

fn peer(host: &str) -> IpAddr {
    let last = match host {
        "host-a" => 1,
        "host-b" => 2,
        _ => 3,
    };
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
}

fn build_plan(
    engine: GroupEngine,
    hosts: &[String],
    port: u16,
    workers: &BTreeMap<String, u16>,
) -> Result<GroupPlan, GroupIdentityError> {
    let members = hosts
        .iter()
        .enumerate()
        .map(|(rank, host)| MemberPlan {
            member: MemberKey {
                host_id: host.clone(),
                member_id: member_id(rank as u32),
            },
            rank: rank as u32,
            role: if rank == 0 {
                MemberRole::Head
            } else {
                MemberRole::Worker
            },
            profile_name: "p".into(),
            profile_fingerprint: "fp".into(),
            checkpoint_fingerprint: "ck".into(),
            model_path: "/models/m".into(),
            devices: vec!["gpu0".into()],
            peer_address: peer(host),
            service_port: (rank == 0).then_some(8000),
            worker_port: workers.get(host.as_str()).copied(),
        })
        .collect();
    GroupPlan::new(
        engine,
        members,
        GroupTopology {
            tensor_parallel: 2,
            pipeline_parallel: 1,
            local_ranks: 1,
        },
        port,
        1,
    )
}

/// The plan the caller builds from the drawn ports and the members' recorded
/// paths and digests (R7); the fixture supplies the members' hosts.
struct PlanFor(GroupEngine);
fn plan_for(_name: &str) -> PlanFor {
    PlanFor(GroupEngine::Vllm)
}
fn sglang_plan_for(_name: &str) -> PlanFor {
    PlanFor(GroupEngine::Sglang)
}

/// Gone evidence from the member's own host for a member that never launched.
fn gone(host: &str, rank: u32) -> MemberGone {
    MemberGone {
        member: MemberKey {
            host_id: host.into(),
            member_id: member_id(rank),
        },
        identities: vec![],
    }
}

fn process(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: "engine".into(),
        pid,
        boot_id: "boot-b".into(),
        start_ticks: 7,
    }
}

// ---- tests ----------------------------------------------------------------

// T27: all members reserve in one transaction or none do.
#[test]
fn group_reservation_is_all_or_nothing() {
    let store = two_host_store(gib(100), gib(10));
    let r = reservation("g", &[("host-a", gib(80)), ("host-b", gib(80))]);
    assert!(matches!(
        store.reserve_group(&r, plan_for("g"), ctx()),
        Err(GroupStoreError::Admission(ResourceStoreError::Admission(
            ResourceError::Insufficient
        )))
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 0)), 0);
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), 0);
    assert!(store.group_plan("g", 0).unwrap().is_none());
}

// T27, Review Focus 5: two groups sharing host B never hold host B's share while failing elsewhere.
#[test]
fn concurrent_groups_do_not_deadlock_or_leak() {
    let store = shared_store_three_hosts(gib(100), gib(100), gib(100));
    let one = reservation("g1", &[("host-a", gib(80)), ("host-b", gib(80))]);
    let two = reservation("g2", &[("host-c", gib(80)), ("host-b", gib(80))]);
    let (x, y) = std::thread::scope(|s| {
        let h1 = s.spawn(|| store.clone().reserve_group(&one, plan_for("g1"), ctx()));
        let h2 = s.spawn(|| store.clone().reserve_group(&two, plan_for("g2"), ctx()));
        (h1.join().unwrap(), h2.join().unwrap())
    });
    assert!(x.is_ok() ^ y.is_ok(), "exactly one fits on host B");
    let loser = if x.is_ok() { "g2" } else { "g1" };
    assert_eq!(store.owner_bytes(&member_owner_id(loser, 0, 0)), 0);
    // The loser's share of host B is not held either, and a retry against the
    // committed ledger is refused as not fitting, not as a lost race.
    assert_eq!(store.owner_bytes(&member_owner_id(loser, 0, 1)), 0);
    let retry = if loser == "g1" { &one } else { &two };
    let loser_plan = if loser == "g1" {
        plan_for("g1")
    } else {
        plan_for("g2")
    };
    assert!(matches!(
        store.reserve_group(retry, loser_plan, ctx()),
        Err(GroupStoreError::Admission(ResourceStoreError::Admission(
            ResourceError::Insufficient
        )))
    ));
    assert_eq!(store.owner_bytes(&member_owner_id(loser, 0, 0)), 0);
}

// T27: the rendezvous port comes from the head's range and is held until every member settles.
#[test]
fn rendezvous_ports_are_allocated_and_held_until_complete() {
    let store = two_host_store(gib(400), gib(400));
    let mut r = reservation("g1", &[("host-a", gib(10)), ("host-b", gib(10))]);
    r.port_range = 25000..=25001;
    let p1 = store
        .reserve_group(&r, plan_for("g1"), ctx())
        .unwrap()
        .rendezvous_port();
    let r2 = GroupReservation {
        deployment_id: "g2".into(),
        ..r.clone()
    };
    let p2 = store
        .reserve_group(&r2, plan_for("g2"), ctx())
        .unwrap()
        .rendezvous_port();
    assert_ne!(p1, p2);
    let r3 = GroupReservation {
        deployment_id: "g3".into(),
        ..r.clone()
    };
    assert!(matches!(
        store.reserve_group(&r3, plan_for("g3"), ctx()),
        Err(GroupStoreError::PortsExhausted)
    ));
    store
        .settle_member("g1", 0, 1, 1, gone("host-b", 1))
        .unwrap();
    assert!(matches!(
        store.reserve_group(&r3, plan_for("g3"), ctx()),
        Err(GroupStoreError::PortsExhausted)
    ));
    assert!(matches!(
        store
            .settle_member("g1", 0, 1, 0, gone("host-a", 0))
            .unwrap(),
        GroupSettlement::Complete
    ));
    assert_eq!(
        store
            .reserve_group(&r3, plan_for("g3"), ctx())
            .unwrap()
            .rendezvous_port(),
        p1
    );
}

// T27, ADR 0028 §5 (R15): a port a Prepare found held outside CapyCTL is
// skipped on that head until the exclusion expires.
#[test]
fn externally_held_rendezvous_port_is_skipped_until_it_expires() {
    let store = two_host_store(gib(400), gib(400));
    let mut r = reservation("g1", &[("host-a", gib(10)), ("host-b", gib(10))]);
    r.port_range = 25000..=25001;
    store.exclude_rendezvous_port("host-a", 25000, now_ms() + 3_600_000);
    // Another head's exclusion of the same port does not matter here.
    store.exclude_rendezvous_port("host-b", 25001, now_ms() + 3_600_000);
    assert_eq!(
        store
            .reserve_group(&r, plan_for("g1"), ctx())
            .unwrap()
            .rendezvous_port(),
        25001
    );
    let r2 = GroupReservation {
        deployment_id: "g2".into(),
        ..r.clone()
    };
    assert!(matches!(
        store.reserve_group(&r2, plan_for("g2"), ctx()),
        Err(GroupStoreError::PortsExhausted)
    ));
    store.exclude_rendezvous_port("host-a", 25000, now_ms() - 1);
    assert_eq!(
        store
            .reserve_group(&r2, plan_for("g2"), ctx())
            .unwrap()
            .rendezvous_port(),
        25000
    );
}

// T22: a SGLang worker's loopback port is leased on its own host and released on Complete.
#[test]
fn sglang_worker_ports_are_leased_per_host() {
    let store = two_host_store(gib(400), gib(400));
    let mut r = reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]);
    r.worker_ports = BTreeMap::from([("host-b".into(), 8100..=8100)]);
    let plan = store
        .reserve_group(&r, sglang_plan_for("g"), ctx())
        .unwrap();
    assert_eq!(plan.members()[1].worker_port, Some(8100));
    assert!(store.endpoint_leased("host-b", 8100));
    assert!(!store.endpoint_leased("host-a", 8100));
    // The worker's range is full while the lease is held.
    let r2 = GroupReservation {
        deployment_id: "g1".into(),
        ..r.clone()
    };
    assert!(matches!(
        store.reserve_group(&r2, sglang_plan_for("g1"), ctx()),
        Err(GroupStoreError::PortsExhausted)
    ));
    store
        .settle_member("g", 0, 1, 0, gone("host-a", 0))
        .unwrap();
    assert!(store.endpoint_leased("host-b", 8100));
    store
        .settle_member("g", 0, 1, 1, gone("host-b", 1))
        .unwrap();
    assert!(!store.endpoint_leased("host-b", 8100));
}

// T32: an uncertain member keeps its charge; settling others does not free it.
#[test]
fn uncertain_member_keeps_its_charge() {
    let store = two_host_store(gib(100), gib(100));
    store
        .reserve_group(
            &reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]),
            plan_for("g"),
            ctx(),
        )
        .unwrap();
    store.mark_member_uncertain("g", 0, 1, 1).unwrap();
    assert!(matches!(
        store
            .settle_member("g", 0, 1, 0, gone("host-a", 0))
            .unwrap(),
        GroupSettlement::Partial { .. }
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
    let (_, rows) = store.group_plan("g", 0).unwrap().unwrap();
    assert_eq!(rows[0].state, MemberState::Settled);
    assert_eq!(rows[1].state, MemberState::Uncertain);
}

// T32, ADR 0028 §11: a member settles only on its own host's evidence, and a
// launched member only on its recorded identities.
#[test]
fn settlement_needs_the_members_own_evidence() {
    let store = two_host_store(gib(100), gib(100));
    store
        .reserve_group(
            &reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]),
            plan_for("g"),
            ctx(),
        )
        .unwrap();
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, gone("host-a", 1)),
        Err(GroupStoreError::Conflict)
    ));
    // ADR 0028 §8: the Launch is fenced as dispatched before it is sent.
    store.mark_member_dispatching("g", 1).unwrap();
    store
        .mark_member_launched(
            "g",
            1,
            &[process(41), process(42)].map(|mut p| {
                p.role = format!("engine-{}", p.pid);
                p
            }),
        )
        .unwrap();
    // Empty evidence no longer proves a launched member gone.
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, gone("host-b", 1)),
        Err(GroupStoreError::Conflict)
    ));
    let mut partial = gone("host-b", 1);
    partial.identities = vec![ProcessIdentity {
        role: "engine-41".into(),
        ..process(41)
    }];
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, partial),
        Err(GroupStoreError::Conflict)
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
    let mut exact = gone("host-b", 1);
    exact.identities = vec![
        ProcessIdentity {
            role: "engine-42".into(),
            ..process(42)
        },
        ProcessIdentity {
            role: "engine-41".into(),
            ..process(41)
        },
    ];
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, exact.clone()).unwrap(),
        GroupSettlement::Partial { unsettled } if unsettled == vec![0]
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), 0);
    // A retried settlement is idempotent.
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, exact).unwrap(),
        GroupSettlement::Partial { .. }
    ));
}

// T27: two members naming one host id are refused before any write.
#[test]
fn one_host_twice_is_refused() {
    let store = two_host_store(gib(400), gib(400));
    let r = reservation("g", &[("host-a", gib(10)), ("host-a", gib(10))]);
    assert!(matches!(
        store.reserve_group(&r, plan_for("g"), ctx()),
        Err(GroupStoreError::Plan)
    ));
}

// T27, ADR 0028 §4: the plan is written durably with the reservation.
#[test]
fn group_plan_is_stored_with_its_members() {
    let store = two_host_store(gib(400), gib(400));
    let plan = store
        .reserve_group(
            &reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]),
            plan_for("g"),
            ctx(),
        )
        .unwrap();
    let (stored, rows) = store.group_plan("g", 0).unwrap().unwrap();
    assert_eq!(stored, plan);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].host_id, "host-b");
    assert_eq!(rows[1].owner_id, member_owner_id(&store.id("g"), 0, 1));
    assert_eq!(rows[1].state, MemberState::Reserved);
    // A second unsettled plan for the same instance is a conflict.
    assert!(matches!(
        store.reserve_group(
            &reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]),
            plan_for("g"),
            ctx(),
        ),
        Err(GroupStoreError::Conflict)
    ));
}

// T33: owner ids round-trip and never collide with instance owners.
#[test]
fn member_owner_ids_round_trip() {
    let id = member_owner_id("d", 0, 1);
    assert_eq!(parse_member_owner_id(&id), Some(("d".into(), 0, 1)));
    assert_eq!(parse_member_owner_id(&instance_owner_id("d", 2)), None);
    assert_eq!(parse_member_owner_id("d"), None);
    assert_eq!(
        parse_member_owner_id("deployment:d/instance:0/member:01"),
        None
    );
}

// T14: digests are recorded per host; a single-host deployment reads its own row as before.
#[test]
fn digests_are_per_host() {
    let store = two_host_store(gib(10), gib(10));
    let c = format!("sha256:{}", "c".repeat(64));
    let d = format!("sha256:{}", "d".repeat(64));
    store.record_digest("g", 1, "host-a", &c).unwrap();
    store.record_digest("g", 1, "host-b", &d).unwrap();
    let all = store.digests_for("g", 1).unwrap();
    assert_eq!(all["host-a"], c);
    assert_eq!(all["host-b"], d);
    assert!(store.record_digest("g", 1, "host-a", "sha256:c").is_err());
    assert!(store.digests_for("g", 2).unwrap().is_empty());
}

// T27 T30, ADR 0028 §5, §11: every key a member's footprint charges belongs to
// that member's own host. A footprint charged on another host, or on a key the
// host-scoped registry does not know, is refused before anything is held, so no
// member can later be released on one host's evidence while its bytes sit on
// another.
#[test]
fn a_members_footprint_is_charged_on_its_own_host_only() {
    let store = two_host_store(gib(400), gib(400));
    let refused = |r: &GroupReservation| {
        assert!(
            matches!(
                store.reserve_group(r, plan_for("g"), ctx()),
                Err(GroupStoreError::Plan)
            ),
            "{r:?}"
        );
        assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 0)), 0);
        assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), 0);
        assert!(store.group_plan("g", 0).unwrap().is_none());
    };
    // The head on host-a with its memory charged on host-b's domain.
    let mut elsewhere = reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]);
    elsewhere.members[0].1.next.allocations[0].domain = domain("host-b");
    refused(&elsewhere);
    // A key no host registered.
    let mut unknown = reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]);
    unknown.members[1].1.next.allocations[0].domain = "nowhere/system".into();
    refused(&unknown);
    // The worker on host-b claiming host-a's GPU.
    let mut device_elsewhere = reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]);
    device_elsewhere.members[1].1.next.devices = vec![DeviceClaim {
        device: device("host-a"),
        sharing: Sharing::Shared,
    }];
    refused(&device_elsewhere);
    // Each member judged against the other host's context.
    assert!(matches!(
        store.reserve_group(
            &reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]),
            plan_for("g"),
            Fresh {
                max_parked: 4,
                swapped: true
            },
        ),
        Err(GroupStoreError::Plan)
    ));
    assert!(store.group_plan("g", 0).unwrap().is_none());
    // Each member on its own host's domain and GPU is reserved.
    let mut own = reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]);
    for (host, grant) in &mut own.members {
        grant.next.devices = vec![DeviceClaim {
            device: device(host),
            sharing: Sharing::Shared,
        }];
    }
    store.reserve_group(&own, plan_for("g"), ctx()).unwrap();
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(10));
}

// T27, ADR 0028 §12: `max_parked` counts the parked owners of each member's own
// host. One parked owner on each of two hosts allowing one each does not refuse
// a cold group that fits both; a host already at its limit still does.
#[test]
fn max_parked_is_judged_on_each_member_host() {
    let store = shared_store_three_hosts(gib(100), gib(100), gib(100));
    store.park_owner("host-a", 0);
    store.park_owner("host-b", 1);
    let one_each = || Fresh {
        max_parked: 1,
        swapped: false,
    };
    store
        .reserve_group(
            &reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]),
            plan_for("g"),
            one_each(),
        )
        .unwrap();
    // A second parked owner on host-c puts that host over its own limit.
    store.park_owner("host-c", 2);
    store.park_owner("host-c", 3);
    assert!(matches!(
        store.reserve_group(
            &reservation("g1", &[("host-b", gib(10)), ("host-c", gib(10))]),
            plan_for("g1"),
            one_each(),
        ),
        Err(GroupStoreError::Admission(ResourceStoreError::Admission(
            ResourceError::CategoryLimit
        )))
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g1", 0, 0)), 0);
}

// T27 T22, ADR 0028 §5, SPEC §3: a host's rendezvous range may overlap its
// endpoint range. A rendezvous draw skips a port an endpoint lease holds on the
// head, and an endpoint lease skips a port an unsettled plan holds there as its
// rendezvous port, so the two never double-allocate one host port.
#[test]
fn rendezvous_ports_and_endpoint_leases_never_share_a_host_port() {
    let store = shared_store_three_hosts(gib(400), gib(400), gib(400));
    // g1's SGLang worker on host-a leases host-a's 25000.
    let mut one = reservation("g1", &[("host-b", gib(10)), ("host-a", gib(10))]);
    one.worker_ports = BTreeMap::from([("host-a".into(), 25000..=25000)]);
    store
        .reserve_group(&one, sglang_plan_for("g1"), ctx())
        .unwrap();
    assert!(store.endpoint_leased("host-a", 25000));
    // g2, headed by host-a, draws past that lease.
    let mut two = reservation("g2", &[("host-a", gib(10)), ("host-c", gib(10))]);
    two.port_range = 25000..=25001;
    assert_eq!(
        store
            .reserve_group(&two, plan_for("g2"), ctx())
            .unwrap()
            .rendezvous_port(),
        25001
    );
    // g3's SGLang worker on host-a leases past g2's rendezvous port.
    let mut three = reservation("g3", &[("host-c", gib(10)), ("host-a", gib(10))]);
    three.worker_ports = BTreeMap::from([("host-a".into(), 25001..=25002)]);
    let plan = store
        .reserve_group(&three, sglang_plan_for("g3"), ctx())
        .unwrap();
    assert_eq!(plan.members()[1].worker_port, Some(25002));
    assert!(!store.endpoint_leased("host-a", 25001));
}

// T30 T32 T33, ADR 0028 §8, §11: a Launch whose reply is lost leaves no
// identities recorded, but the member was durably fenced as dispatched before
// the Launch was sent. Empty gone evidence then proves nothing: the member may
// be running. It keeps its charge, through `uncertain` too, until the host's
// own record of its identities is recorded and those are proven gone. A member
// never dispatched still settles on empty evidence, and identities are never
// recorded for a member that was not fenced first.
#[test]
fn a_dispatched_member_never_settles_on_empty_evidence() {
    let store = two_host_store(gib(100), gib(100));
    store
        .reserve_group(
            &reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]),
            plan_for("g"),
            ctx(),
        )
        .unwrap();
    let launched = [process(41)];
    // Identities belong only to a Launch fenced as dispatched first.
    assert!(matches!(
        store.mark_member_launched("g", 1, &launched),
        Err(GroupStoreError::Conflict)
    ));
    store.mark_member_dispatching("g", 1).unwrap();
    // A retried fence is idempotent.
    store.mark_member_dispatching("g", 1).unwrap();
    let (_, rows) = store.group_plan("g", 0).unwrap().unwrap();
    assert_eq!(
        (rows[0].state, rows[0].dispatched),
        (MemberState::Reserved, false)
    );
    assert_eq!(
        (rows[1].state, rows[1].dispatched),
        (MemberState::Dispatching, true)
    );
    // Launch sent, member spawned, reply lost: empty evidence is refused.
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, gone("host-b", 1)),
        Err(GroupStoreError::Conflict)
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
    // The host becomes unreachable: still dispatched, still refused.
    store.mark_member_uncertain("g", 0, 1, 1).unwrap();
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, gone("host-b", 1)),
        Err(GroupStoreError::Conflict)
    ));
    let (_, rows) = store.group_plan("g", 0).unwrap().unwrap();
    assert_eq!(
        (rows[1].state, rows[1].dispatched),
        (MemberState::Uncertain, true)
    );
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
    // A settled or uncertain member is never fenced for a new Launch.
    store.mark_member_uncertain("g", 0, 1, 0).unwrap();
    assert!(matches!(
        store.mark_member_dispatching("g", 0),
        Err(GroupStoreError::Conflict)
    ));
    // The never-dispatched head settles on its host's empty evidence.
    assert!(matches!(
        store
            .settle_member("g", 0, 1, 0, gone("host-a", 0))
            .unwrap(),
        GroupSettlement::Partial { unsettled } if unsettled == vec![1]
    ));
    // The host reconnects and reconciles its journal: the identities it
    // recorded at spawn are recorded here, then proven gone.
    store.mark_member_launched("g", 1, &launched).unwrap();
    let (_, rows) = store.group_plan("g", 0).unwrap().unwrap();
    assert_eq!(rows[1].state, MemberState::Launched);
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, gone("host-b", 1)),
        Err(GroupStoreError::Conflict)
    ));
    let mut exact = gone("host-b", 1);
    exact.identities = launched.to_vec();
    assert!(matches!(
        store.settle_member("g", 0, 1, 1, exact).unwrap(),
        GroupSettlement::Complete
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), 0);
}

// T15, T27 (R15): a retry in the same generation after a Prepare refusal
// redraws its rendezvous port; the fully settled plan it replaces is gone.
#[test]
fn a_settled_plan_of_the_same_generation_is_replaced() {
    let store = two_host_store(gib(100), gib(100));
    let r = reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]);
    let first = store.reserve_group(&r, plan_for("g"), ctx()).unwrap();
    assert_eq!(first.rendezvous_port(), 25000);
    // The head's Prepare found the port held outside CapyCTL; nothing was
    // dispatched, so every member settles on empty evidence.
    store.exclude_rendezvous_port("host-a", 25000, now_ms() + 600_000);
    store
        .settle_member("g", 0, 1, 0, gone("host-a", 0))
        .unwrap();
    assert!(matches!(
        store
            .settle_member("g", 0, 1, 1, gone("host-b", 1))
            .unwrap(),
        GroupSettlement::Complete
    ));
    let second = store.reserve_group(&r, plan_for("g"), ctx()).unwrap();
    assert_eq!(second.rendezvous_port(), 25001);
    let (stored, rows) = store.group_plan("g", 0).unwrap().unwrap();
    assert_eq!(stored, second);
    assert!(rows
        .iter()
        .all(|row| row.state == MemberState::Reserved && !row.dispatched));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 0)), gib(50));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
}

// T30, T33 (ADR 0028 §8, §11): a plan an interrupted activation left with no
// member dispatched is released on empty evidence before the next one
// reserves, its worker lease with it.
#[test]
fn a_never_dispatched_leftover_is_released_before_reserving() {
    let store = two_host_store(gib(100), gib(100));
    let mut r = reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]);
    r.worker_ports = BTreeMap::from([("host-b".into(), 8100..=8100)]);
    store
        .reserve_group(&r, sglang_plan_for("g"), ctx())
        .unwrap();
    store.mark_member_uncertain("g", 0, 1, 1).unwrap();
    // The leftover still holds the instance.
    assert!(matches!(
        store.reserve_group(&r, sglang_plan_for("g"), ctx()),
        Err(GroupStoreError::Conflict)
    ));
    assert!(matches!(
        store.release_undispatched_group("g", 0, 1).unwrap(),
        GroupSettlement::Complete
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 0)), 0);
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), 0);
    assert!(!store.endpoint_leased("host-b", 8100));
    // A repeated release reports the settled plan again.
    assert!(matches!(
        store.release_undispatched_group("g", 0, 1).unwrap(),
        GroupSettlement::Complete
    ));
    let plan = store
        .reserve_group(&r, sglang_plan_for("g"), ctx())
        .unwrap();
    assert_eq!(plan.members()[1].worker_port, Some(8100));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
}

// T33 (R23): a plan with a dispatched member is never released on empty
// evidence; every member keeps its charge.
#[test]
fn a_dispatched_leftover_is_never_released_on_empty_evidence() {
    let store = two_host_store(gib(100), gib(100));
    store
        .reserve_group(
            &reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]),
            plan_for("g"),
            ctx(),
        )
        .unwrap();
    store.mark_member_dispatching("g", 1).unwrap();
    assert!(matches!(
        store.release_undispatched_group("g", 0, 1),
        Err(GroupStoreError::Conflict)
    ));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 0)), gib(50));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
    let (_, rows) = store.group_plan("g", 0).unwrap().unwrap();
    assert_eq!(rows[0].state, MemberState::Reserved);
    assert_eq!(rows[1].state, MemberState::Dispatching);
    // No plan at that generation: nothing to release.
    assert!(matches!(
        store.release_undispatched_group("g", 0, 2),
        Err(GroupStoreError::Conflict)
    ));
}

impl Fixture {
    fn release_undispatched_group(
        &self,
        name: &str,
        instance: u32,
        generation: i64,
    ) -> Result<GroupSettlement, GroupStoreError> {
        self.open()
            .release_undispatched_group(&self.id(name), instance, generation)
    }
}

// T33 (Task 8 concern, owner-id audit): a group instance's members are not its
// instance owner. Status lists each member owner, which parses as the member it
// is, and never labels the instance with one of them.
#[test]
fn a_group_instance_is_not_labelled_with_a_member_owner() {
    let store = two_host_store(gib(100), gib(100));
    store
        .reserve_group(
            &reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]),
            plan_for("g"),
            ctx(),
        )
        .unwrap();
    let id = store.id("g");
    let snapshot = store.open().snapshot().unwrap();
    let deployment = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
    assert!(!deployment.instances.is_empty());
    for instance in &deployment.instances {
        assert_eq!(instance.reservation_owner, None, "{instance:?}");
    }
    let members: Vec<(String, u32, u32)> = snapshot
        .reservations
        .iter()
        .filter_map(|r| parse_member_owner_id(&r.owner_id))
        .collect();
    assert_eq!(members, [(id.clone(), 0, 0), (id, 0, 1)]);
}
