//! ADR 0028 §12: a multi-node group's park and wake bookkeeping, durably
//! (and §11's record of a stalled group, and §15's status of the group, on
//! the same Ready group).
//!
//! Each test brings a TP 2 group over host A (head) and host B to Ready
//! through the store alone (accept the start, reserve every member, arm,
//! fence and launch each member, record the head's readiness), then drives
//! its park or wake step with member reports as the controller collects them
//! from each member's own host.
//!
//! CPU-only: nothing launches. Passing here never qualifies a group recipe;
//! the live MN rows do (SPEC §18).
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};

use capyctl_config::remote_resources::scope_host_document;
use capyctl_domain::completion::{
    CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity, StepExecutionContext,
};
use capyctl_domain::group::{
    member_id, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan, MemberRole,
};
use capyctl_domain::resources::{MemoryLimit, MemoryObservation};
use capyctl_scheduler::residency::AdmissionContext;
use capyctl_store::dispatch::CoordinatorSession;
use capyctl_store::groups::{member_owner_id, GroupReservation, StoredCanary};
use capyctl_store::lifecycle::DeploymentFence;
use capyctl_store::managed_configuration::HostTarget;
use capyctl_store::ordinary_lifecycle::park::{
    MemberResidency, ResidencyArm, ResidencyReceipt, WakeScope,
};
use capyctl_store::resource_ledger::GrantRequest;
use capyctl_store::Store;
use rusqlite::Connection;
use serde_json::{json, Value};

/// The group's hosts in rank order: host A heads it.
const HOSTS: [&str; 2] = ["host-a", "host-b"];
/// When the fixture starts; every later step moves the clock forward.
const T0: i64 = 100_000;

const GIB: i64 = 1 << 30;
/// The golden deployment's per-member footprints: Ready, parked budget, and
/// the wake peak (parked ∪ wake).
const READY: i64 = 8 * GIB;
const PARKED: i64 = 2 * GIB;
const WAKE_PEAK: i64 = 10 * GIB;

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    sql: Connection,
    session: CoordinatorSession,
    config: Value,
    documents: Vec<(String, Value)>,
}

/// Two enrolled hosts with the golden host policy (`max_parked` per host as
/// given) and their peer addresses.
fn world(max_parked: [u32; 2]) -> World {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("park.sqlite3");
    let store = Store::open(&path).unwrap();
    let sql = Connection::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut documents = Vec::new();
    for (index, host) in HOSTS.iter().enumerate() {
        sql.execute(
            "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES(?1,?1,'key',0)",
            [host],
        )
        .unwrap();
        let mut document = golden["input"]["host"].clone();
        document["resource_policy"]["max_parked"] = json!(max_parked[index]);
        document["resource_policy"]["groups"] = json!({ "peer_address": peer(index).to_string() });
        let policy = capyctl_config::effective::normalize_host_policy(&document).unwrap();
        store
            .import_remote_resource_policy(
                &session,
                host,
                &policy,
                &[MemoryObservation {
                    domain: "unified".into(),
                    capacity_bytes: 1 << 40,
                    available_bytes: 1 << 40,
                    sampled_at_ms: 1,
                }],
                1,
            )
            .unwrap();
        documents.push(((*host).to_owned(), document));
    }
    config.as_object_mut().unwrap().remove("host");
    config["topology"] = json!({"tensor_parallel": 2});
    config["placement"] = json!({"hosts": HOSTS});
    World {
        _dir: dir,
        store,
        sql,
        session,
        config,
        documents,
    }
}

fn peer(index: usize) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10 + index as u8))
}

fn identity(role: &str, pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: role.into(),
        pid,
        boot_id: "boot".into(),
        start_ticks: 1,
    }
}

/// What a group's Launch on `rank` reported.
fn member_identities(rank: u32, base: u32) -> Vec<ProcessIdentity> {
    if rank == 0 {
        vec![identity("api", base), identity("worker-0", base + 1)]
    } else {
        vec![identity(&format!("worker-{rank}"), base + 10 * rank)]
    }
}

/// One member's report from its own host.
fn report(rank: u32, host: &str, resident_bytes: i64, observed_at_ms: i64) -> MemberResidency {
    MemberResidency {
        rank,
        host_id: host.into(),
        resident_bytes,
        observed_at_ms,
    }
}

/// A group Ready at `generation`.
struct Group {
    fence: DeploymentFence,
    generation: i64,
}

impl World {
    /// The limits a member host's own policy admits its member against.
    fn limits(&self, host: &str) -> Vec<MemoryLimit> {
        let controls = self.store.resource_policy(host).unwrap().unwrap().controls;
        controls
            .domains
            .iter()
            .map(|(domain, d)| MemoryLimit {
                domain: domain.clone(),
                managed_bytes: d.managed_limit,
                free_reserve_bytes: d.free_reserve,
                reserve_absorbs_unmanaged: d.memory
                    == capyctl_config::effective::DomainMemory::Device,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect()
    }

    /// `host`'s own observation at `now`, `available` bytes free.
    fn observed(&self, host: &str, now: i64, available: i64) -> Vec<MemoryObservation> {
        let controls = self.store.resource_policy(host).unwrap().unwrap().controls;
        controls
            .domains
            .keys()
            .map(|domain| MemoryObservation {
                domain: domain.clone(),
                capacity_bytes: 1 << 40,
                available_bytes: available,
                sampled_at_ms: now,
            })
            .collect()
    }

    fn ttl_and_max_parked(&self, host: &str) -> (i64, usize) {
        let controls = self.store.resource_policy(host).unwrap().unwrap().controls;
        (controls.observation_ttl_ms, controls.max_parked as usize)
    }

    /// Deploy group `name` on both hosts and bring it Ready at `now`
    /// (`service_port` the head's), every member reserved, fenced, launched
    /// and the head's readiness recorded.
    fn ready(&self, name: &str, now: i64, service_port: u16) -> Group {
        let targets: Vec<HostTarget> = self
            .documents
            .iter()
            .map(|(host, document)| {
                let trusted = scope_host_document(host, document).unwrap();
                let policy = self.store.resource_policy(host).unwrap().unwrap();
                HostTarget {
                    host_id: host.clone(),
                    host_name: host.clone(),
                    trusted_host: capyctl_config::effective::compose_current_resource_controls(
                        &trusted,
                        &policy.context,
                        &policy.controls,
                    )
                    .unwrap(),
                    scoped: true,
                }
            })
            .collect();
        let mut config = self.config.clone();
        config["name"] = json!(name);
        config["routes"] = json!([name]);
        let receipt = self
            .store
            .create_managed_configuration_on_hosts(
                &self.session,
                "owner",
                name,
                &json!({ "config": config }).to_string(),
                &targets,
                &[],
                now,
            )
            .unwrap();
        let fence = DeploymentFence {
            deployment_id: receipt.deployment_id.clone(),
            revision: receipt.revision,
            generation: receipt.generation,
        };
        let start = self
            .store
            .accept_start(&self.session, &fence, now, now + 60_000)
            .unwrap();
        let generation: i64 = self
            .sql
            .query_row(
                "SELECT generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
                [&fence.deployment_id],
                |r| r.get(0),
            )
            .unwrap();
        let hosts: Vec<String> = HOSTS.iter().map(|h| (*h).to_owned()).collect();
        let resolutions = self
            .store
            .group_member_resolutions(&fence.deployment_id, fence.revision, &hosts)
            .unwrap();
        let epoch = self.store.resource_snapshot().unwrap().epoch;
        let reservation = GroupReservation {
            deployment_id: fence.deployment_id.clone(),
            instance_index: 0,
            members: resolutions
                .iter()
                .map(|m| {
                    (
                        m.host_id.clone(),
                        GrantRequest {
                            id: ulid::Ulid::new().to_string(),
                            owner_id: member_owner_id(&fence.deployment_id, 0, m.rank),
                            deployment_id: fence.deployment_id.clone(),
                            operation_id: start.operation_id.clone(),
                            revision: fence.revision,
                            generation,
                            expected_epoch: epoch,
                            next: m.cold.clone(),
                        },
                    )
                })
                .collect(),
            head_host: HOSTS[0].into(),
            port_range: 25000..=25099,
            worker_ports: BTreeMap::new(),
        };
        let observed: BTreeMap<String, (Vec<MemoryObservation>, Vec<MemoryLimit>)> = HOSTS
            .iter()
            .map(|host| {
                (
                    (*host).to_owned(),
                    (self.observed(host, now, 1 << 40), self.limits(host)),
                )
            })
            .collect();
        let contexts: BTreeMap<String, AdmissionContext<'_>> = observed
            .iter()
            .map(|(host, (observations, limits))| {
                let (ttl, max_parked) = self.ttl_and_max_parked(host);
                (
                    host.clone(),
                    AdmissionContext::new(observations, limits, now, ttl, max_parked),
                )
            })
            .collect();
        let plan_for = |port: u16, _: &BTreeMap<String, u16>| {
            GroupPlan::new(
                GroupEngine::Vllm,
                HOSTS
                    .iter()
                    .enumerate()
                    .map(|(rank, host)| MemberPlan {
                        member: MemberKey {
                            host_id: (*host).to_owned(),
                            member_id: member_id(rank as u32),
                        },
                        rank: rank as u32,
                        role: if rank == 0 {
                            MemberRole::Head
                        } else {
                            MemberRole::Worker
                        },
                        profile_name: "local".into(),
                        profile_fingerprint: "vllm-build-1".into(),
                        checkpoint_fingerprint: "sha256:model".into(),
                        model_path: "/srv/models/toy".into(),
                        devices: vec!["gpu0".into()],
                        peer_address: peer(rank),
                        service_port: (rank == 0).then_some(service_port),
                        worker_port: None,
                    })
                    .collect(),
                GroupTopology {
                    tensor_parallel: 2,
                    pipeline_parallel: 1,
                    local_ranks: 1,
                },
                port,
                generation,
            )
        };
        self.store
            .reserve_group(&reservation, plan_for, &contexts)
            .unwrap();
        let context = self
            .store
            .arm_group_initialize(&self.session, &start.step_id, now)
            .unwrap();
        let base = u32::from(service_port);
        for rank in 0..2u32 {
            self.store
                .mark_member_dispatching(
                    &fence.deployment_id,
                    0,
                    generation,
                    rank,
                    &ulid::Ulid::new().to_string(),
                )
                .unwrap();
            self.store
                .mark_member_launched(
                    &fence.deployment_id,
                    0,
                    generation,
                    rank,
                    &member_identities(rank, base),
                )
                .unwrap();
        }
        let head = member_identities(0, base);
        self.store
            .record_owned_launch(
                &self.session,
                &start.step_id,
                &OwnedLaunchReceipt {
                    binding_id: context.binding_id.clone(),
                    incarnation: context.incarnation.clone(),
                    identities: head.clone(),
                    observed_at_ms: now,
                    receipt: "group head native readiness".into(),
                },
                now,
            )
            .unwrap();
        let (ttl, _) = self.ttl_and_max_parked(HOSTS[0]);
        self.store
            .complete_step(
                &self.session,
                &start.step_id,
                &CompletionEvidence {
                    token: context.token.clone(),
                    identities: head,
                    observed_at_ms: now,
                    control_receipt: Some("group head native readiness".into()),
                    milestones: vec![
                        Milestone::AllocationsRestored,
                        Milestone::WeightsUsable,
                        Milestone::CacheValid,
                        Milestone::ModelUsable,
                    ],
                },
                now,
                ttl,
            )
            .unwrap();
        assert_eq!(self.instance(&fence.deployment_id), ("ready".into(), true));
        Group { fence, generation }
    }

    fn park(&self, group: &Group, now: i64) -> ResidencyReceipt {
        self.store
            .accept_park_command(
                &self.session,
                "operator",
                &group.fence.deployment_id,
                group.fence.revision,
                &ulid::Ulid::new().to_string(),
                now,
                now + 60_000,
            )
            .unwrap()
    }

    fn wake(&self, group: &Group, now: i64) -> ResidencyReceipt {
        self.store
            .accept_restore_command(
                &self.session,
                "router",
                &group.fence.deployment_id,
                WakeScope::All,
                group.fence.revision,
                &ulid::Ulid::new().to_string(),
                now,
                now + 60_000,
            )
            .unwrap()
            .expect("a parked group wakes in place")
    }

    /// Arm a park at `now`: a park is judged on the ledger alone.
    fn arm_park(&self, step: &str, now: i64) -> ResidencyArm {
        self.store
            .arm_group_residency(&self.session, step, now, &BTreeMap::new())
            .unwrap()
    }

    /// Arm a wake at `now` against each host's own observation, `available`
    /// bytes free on each.
    fn arm_wake(&self, step: &str, now: i64, available: [i64; 2]) -> ResidencyArm {
        let observed: BTreeMap<String, (Vec<MemoryObservation>, Vec<MemoryLimit>)> = HOSTS
            .iter()
            .zip(available)
            .map(|(host, free)| {
                (
                    (*host).to_owned(),
                    (self.observed(host, now, free), self.limits(host)),
                )
            })
            .collect();
        let contexts: BTreeMap<String, AdmissionContext<'_>> = observed
            .iter()
            .map(|(host, (observations, limits))| {
                let (ttl, max_parked) = self.ttl_and_max_parked(host);
                (
                    host.clone(),
                    AdmissionContext::new(observations, limits, now, ttl, max_parked),
                )
            })
            .collect();
        self.store
            .arm_group_residency(&self.session, step, now, &contexts)
            .unwrap()
    }

    fn complete(
        &self,
        step: &str,
        reports: &[MemberResidency],
        now: i64,
    ) -> Result<(), capyctl_store::lifecycle::LifecycleError> {
        self.store
            .complete_group_residency(&self.session, step, reports, "head collective", now)
    }

    /// The bytes member `rank` of `group` is charged on its own host.
    fn charged(&self, group: &Group, rank: u32) -> i64 {
        let owner = member_owner_id(&group.fence.deployment_id, 0, rank);
        self.store
            .resource_snapshot()
            .unwrap()
            .owners
            .get(&owner)
            .map_or(0, |footprint| {
                footprint.allocations.iter().map(|a| a.bytes).sum()
            })
    }

    fn instance(&self, id: &str) -> (String, bool) {
        self.sql
            .query_row(
                "SELECT observed_state,dispatch_enabled=1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn step_state(&self, step: &str) -> String {
        self.store.residency_step_state(step).unwrap().unwrap()
    }
}

fn new_context(arm: ResidencyArm) -> StepExecutionContext {
    match arm {
        ResidencyArm::New(context) => *context,
        other => panic!("expected a fresh arm, got {other:?}"),
    }
}

/// A group parked through the store at `now`.
fn parked(w: &World, group: &Group, now: i64) {
    let park = w.park(group, now);
    new_context(w.arm_park(&park.step_id, now + 1));
    w.complete(
        &park.step_id,
        &[
            report(0, HOSTS[0], PARKED, now + 2),
            report(1, HOSTS[1], PARKED, now + 2),
        ],
        now + 3,
    )
    .unwrap();
    assert_eq!(w.instance(&group.fence.deployment_id).0, "parked");
}

// T20 (ADR 0028 §12, R12): a member's charge moves to its parked budget only
// on its own host's fresh report, and only once every member reported: an
// armed park charges nothing beyond Ready, a report from another host or one
// observed before the collective proves nothing, and the commit moves each
// member to its own parked budget and reads the instance parked.
#[test]
fn a_member_moves_to_its_parked_budget_only_on_its_own_fresh_report() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    let park = w.park(&g, T0 + 10);
    let armed = new_context(w.arm_park(&park.step_id, T0 + 20));
    assert_eq!(armed.issued_at_ms, T0 + 20);
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    let members = w
        .store
        .group_residency_members(&w.session, &park.step_id)
        .unwrap();
    assert_eq!(members.len(), 2);
    assert!(members.iter().all(|m| m.parked_bytes == PARKED));
    assert_eq!(members[1].host_id, HOSTS[1]);
    assert_eq!(members[1].identities, member_identities(1, 8100));

    // Rank 1 reported by host A, not its own host.
    let foreign = [
        report(0, HOSTS[0], PARKED, T0 + 30),
        report(1, HOSTS[0], PARKED, T0 + 30),
    ];
    assert!(w.complete(&park.step_id, &foreign, T0 + 40).is_err());
    // Rank 1's own report, but sampled before the collective was sent.
    let stale = [
        report(0, HOSTS[0], PARKED, T0 + 30),
        report(1, HOSTS[1], PARKED, T0 + 5),
    ];
    assert!(w.complete(&park.step_id, &stale, T0 + 40).is_err());
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    assert_eq!(w.step_state(&park.step_id), "armed");
    assert_eq!(w.instance(&g.fence.deployment_id).0, "ready");

    let own = [
        report(0, HOSTS[0], PARKED, T0 + 30),
        report(1, HOSTS[1], PARKED - GIB, T0 + 30),
    ];
    w.complete(&park.step_id, &own, T0 + 40).unwrap();
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (PARKED, PARKED));
    assert_eq!(w.instance(&g.fence.deployment_id), ("parked".into(), false));
    assert_eq!(w.step_state(&park.step_id), "completed");
}

// T20, T32 (ADR 0028 §12, AGENTS.md: uncertainty retains accounting): a
// member that never reported keeps its full charge; the step only becomes
// uncertain, every charge as it was, dispatch closed, and a closure reason
// recorded so no re-proof reopens it before a stop proves the group gone.
// No stop has fenced it yet, so its group stop is owed (R42).
#[test]
fn a_silent_member_stays_charged_and_uncertain() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    let park = w.park(&g, T0 + 10);
    new_context(w.arm_park(&park.step_id, T0 + 20));
    let only_head = [report(0, HOSTS[0], PARKED, T0 + 30)];
    assert!(w.complete(&park.step_id, &only_head, T0 + 40).is_err());
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    assert!(w
        .store
        .mark_residency_uncertain(&w.session, &park.step_id, "rank 1 reported nothing")
        .unwrap());
    w.store
        .mark_member_uncertain(&g.fence.deployment_id, 0, g.generation, 1)
        .unwrap();
    assert_eq!(w.step_state(&park.step_id), "uncertain");
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    assert_eq!(w.instance(&g.fence.deployment_id), ("ready".into(), false));
    let closed: bool = w
        .sql
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM dispatch_closures WHERE deployment_id=?1 AND instance_index=0 AND generation=?2)",
            rusqlite::params![g.fence.deployment_id, g.generation],
            |r| r.get(0),
        )
        .unwrap();
    assert!(closed, "a re-proof must not reopen a half-parked group");
    let (_, rows) = w
        .store
        .group_plan_at(&g.fence.deployment_id, 0, g.generation)
        .unwrap()
        .unwrap();
    assert_eq!(rows[1].state.as_str(), "uncertain");
    let owed = w.store.group_residency_stops_due(&w.session).unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].operation_id, park.operation_id);
    assert_eq!(owed[0].generation, g.generation);
    assert!(!owed[0].failed, "an uncertainty alone is not a failure");
}

// T20, T32 (Review Focus 4): the head's collective succeeded but rank 1 stays
// resident: it keeps its full Ready charge (never its parked budget), so does
// the head, and the failure is recorded on the plan with its rank and code,
// so the owed stop runs under the failure principal.
#[test]
fn a_resident_rank_keeps_its_full_charge_and_its_failure_is_recorded() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    let park = w.park(&g, T0 + 10);
    new_context(w.arm_park(&park.step_id, T0 + 20));
    let resident = [
        report(0, HOSTS[0], PARKED, T0 + 30),
        report(1, HOSTS[1], READY, T0 + 30),
    ];
    assert!(w.complete(&park.step_id, &resident, T0 + 40).is_err());
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    w.store
        .record_group_failure_code(
            &g.fence.deployment_id,
            0,
            g.generation,
            1,
            "group_member_failed",
        )
        .unwrap();
    // The first failure is kept.
    w.store
        .record_group_failure_code(
            &g.fence.deployment_id,
            0,
            g.generation,
            0,
            "group_wake_mismatch",
        )
        .unwrap();
    assert_eq!(
        w.store
            .group_failure(&g.fence.deployment_id, 0, g.generation)
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        w.store
            .group_failure_code(&g.fence.deployment_id, 0, g.generation)
            .unwrap()
            .as_deref(),
        Some("group_member_failed")
    );
    w.store
        .mark_residency_uncertain(&w.session, &park.step_id, "rank 1 still resident")
        .unwrap();
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    assert_ne!(w.instance(&g.fence.deployment_id).0, "parked");
    let owed = w.store.group_residency_stops_due(&w.session).unwrap();
    assert_eq!(owed.len(), 1);
    assert!(owed[0].failed);
}

// T20, T27 (ADR 0028 §12, SPEC §7.3): a wake charges each member its wake
// peak on its own host, all or nothing: when host B's own observation cannot
// admit its member, the wake waits and host A's member is not charged either.
// Once both hosts admit it, both are charged the peak at arm, and the commit
// on each member's own resident report brings both to Ready. A head refusal
// returns both from the peak to the parked budget.
#[test]
fn a_wake_charges_each_hosts_peak_all_or_nothing() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    parked(&w, &g, T0 + 10);
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (PARKED, PARKED));

    let wake = w.wake(&g, T0 + 100);
    let roomy = 1 << 40;
    match w.arm_wake(&wake.step_id, T0 + 110, [roomy, 4 * GIB]) {
        ResidencyArm::Blocked(why) => assert!(why.contains(HOSTS[1]), "{why}"),
        other => panic!("expected the wake to wait for host B, got {other:?}"),
    }
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (PARKED, PARKED));
    assert_eq!(w.step_state(&wake.step_id), "planned");

    new_context(w.arm_wake(&wake.step_id, T0 + 120, [roomy, roomy]));
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (WAKE_PEAK, WAKE_PEAK));
    w.store
        .refuse_residency(&w.session, &wake.step_id, "head refused: unchanged")
        .unwrap();
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (PARKED, PARKED));
    assert_eq!(w.instance(&g.fence.deployment_id).0, "parked");

    let wake = w.wake(&g, T0 + 200);
    new_context(w.arm_wake(&wake.step_id, T0 + 210, [roomy, roomy]));
    // Rank 1 still at its parked budget: not woken on its own host.
    let asleep = [
        report(0, HOSTS[0], READY, T0 + 220),
        report(1, HOSTS[1], PARKED, T0 + 220),
    ];
    assert!(w.complete(&wake.step_id, &asleep, T0 + 230).is_err());
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (WAKE_PEAK, WAKE_PEAK));
    let resident = [
        report(0, HOSTS[0], READY, T0 + 220),
        report(1, HOSTS[1], READY, T0 + 220),
    ];
    w.complete(&wake.step_id, &resident, T0 + 230).unwrap();
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    assert_eq!(w.instance(&g.fence.deployment_id), ("ready".into(), true));
}

// T20, T27 (ADR 0028 §12): `max_parked` counts a parked group once on each
// host. With host B allowing one parked owner, the first group parks (its
// one member there); a second group over the same hosts is then refused
// `parked_capacity` naming host B only, and stays Ready, fully charged.
#[test]
fn max_parked_counts_a_group_once_on_each_host() {
    let w = world([16, 1]);
    let g1 = w.ready("g1", T0, 8100);
    let g2 = w.ready("g2", T0 + 1, 8101);
    parked(&w, &g1, T0 + 10);
    let park = w.park(&g2, T0 + 100);
    match w.arm_park(&park.step_id, T0 + 110) {
        ResidencyArm::Refused(code) => assert_eq!(code, "parked_capacity"),
        other => panic!("expected parked_capacity, got {other:?}"),
    }
    let reason: String = w
        .sql
        .query_row(
            "SELECT group_concat(evidence,' | ') FROM journal_entries WHERE operation_id=?1",
            [&park.operation_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(reason.contains("host host-b (rank 1)"), "{reason}");
    assert!(!reason.contains("host host-a"), "{reason}");
    assert_eq!((w.charged(&g2, 0), w.charged(&g2, 1)), (READY, READY));
    assert_eq!(w.instance(&g2.fence.deployment_id), ("ready".into(), true));
}

// T20 (ADR 0028 §12, decided 2026-10-06): the wake canary's reference is
// recorded once per active plan; a later recording keeps the first.
#[test]
fn the_first_canary_reference_is_kept() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    let first = StoredCanary {
        prompt: "canary".into(),
        tokens: vec![1, 2, 3, 4, 5, 6, 7, 8],
    };
    let second = StoredCanary {
        prompt: "canary".into(),
        tokens: vec![9; 8],
    };
    assert!(w
        .store
        .record_canary_reference(&g.fence.deployment_id, 0, g.generation, &first)
        .unwrap());
    assert!(!w
        .store
        .record_canary_reference(&g.fence.deployment_id, 0, g.generation, &second)
        .unwrap());
    assert_eq!(
        w.store
            .canary_reference(&g.fence.deployment_id, 0, g.generation)
            .unwrap(),
        Some(first)
    );
}

// T31 T32 (ADR 0028 §11, decided 2026-10-06; R42 pattern): a stalled group's
// failure is recorded once, in one transaction: dispatch closes with a
// recorded reason, `group_stalled` is kept at rank 0 and named in status,
// every member keeps its full charge, and its stop is owed until one is
// accepted. A report for another generation, or for a group already
// failing, names nothing and records nothing.
#[test]
fn a_stalled_group_is_recorded_failed_once_with_every_charge_kept() {
    use capyctl_store::ordinary_lifecycle::group_stall::GroupStallStop;
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    let id = g.fence.deployment_id.clone();
    let serving = w.store.serving_instances(&id).unwrap();
    assert!(serving.len() == 1 && serving[0].group, "{serving:?}");
    assert_eq!(
        w.store.stalled_group(&id, 0, g.generation + 1).unwrap(),
        None
    );
    let stalled = w
        .store
        .stalled_group(&id, 0, g.generation)
        .unwrap()
        .expect("the instance's current Ready group");
    assert_eq!(stalled.plan.generation(), g.generation);
    assert_eq!(stalled.revision, g.fence.revision);
    assert!(!w
        .store
        .record_group_stall(&w.session, &id, 0, g.generation + 1)
        .unwrap());
    assert!(w
        .store
        .group_stall_stops_due(&w.session)
        .unwrap()
        .is_empty());
    assert_eq!(w.instance(&id), ("ready".into(), true));
    assert!(w
        .store
        .record_group_stall(&w.session, &id, 0, g.generation)
        .unwrap());
    assert_eq!((w.charged(&g, 0), w.charged(&g, 1)), (READY, READY));
    assert_eq!(w.instance(&id), ("ready".into(), false));
    let closed: bool = w
        .sql
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM dispatch_closures WHERE deployment_id=?1 AND instance_index=0 AND generation=?2)",
            rusqlite::params![id, g.generation],
            |r| r.get(0),
        )
        .unwrap();
    assert!(closed, "no re-proof or switch reopens a stalled group");
    assert_eq!(
        w.store.group_failure(&id, 0, g.generation).unwrap(),
        Some(0)
    );
    assert_eq!(
        w.store
            .group_failure_code(&id, 0, g.generation)
            .unwrap()
            .as_deref(),
        Some("group_stalled")
    );
    let last_error: Option<String> = w
        .sql
        .query_row(
            "SELECT last_error FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last_error.as_deref(), Some("group_stalled"));
    assert_eq!(w.store.stalled_group(&id, 0, g.generation).unwrap(), None);
    assert!(!w
        .store
        .record_group_stall(&w.session, &id, 0, g.generation)
        .unwrap());
    assert_eq!(
        w.store.group_stall_stops_due(&w.session).unwrap(),
        vec![GroupStallStop {
            deployment_id: id.clone(),
            instance_index: 0,
            generation: g.generation,
        }]
    );
}

/// Instance 0 of `group` as the status snapshot serializes it.
fn instance_status(w: &World, group: &Group) -> Value {
    let snapshot = serde_json::to_value(w.store.snapshot().unwrap()).unwrap();
    snapshot["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == group.fence.deployment_id)
        .map(|d| d["instances"][0].clone())
        .expect("the group's instance")
}

// T21 T03 (ADR 0028 §13, §15): a group instance's status comes from its
// plan, not from a placed host (a group records none): the engine, the
// topology, the head's rendezvous address, the unauthenticated peer
// transport, and every member with its host, rank, role, state, recorded
// processes, its own charge on its own host and its residency.
#[test]
fn a_group_status_lists_every_member_from_its_plan() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    let s = instance_status(&w, &g);
    assert_eq!(s["host_id"], Value::Null, "{s}");
    assert_eq!(s["engine"], "vllm", "{s}");
    assert_eq!(
        s["topology"],
        json!({"tensor_parallel": 2, "pipeline_parallel": 1})
    );
    assert_eq!(s["rendezvous"], "192.0.2.10:25000");
    assert_eq!(s["peer_transport"], "unauthenticated");
    assert_eq!(s["plan_generation"], g.generation.to_string());
    assert_eq!(
        s["members"],
        json!([
            {"host": "host-a", "node_rank": 0, "role": "head", "state": "launched",
             "processes": 2, "reservation": {"phase": "ready", "bytes": READY.to_string()},
             "residency": "deep", "last_error": null},
            {"host": "host-b", "node_rank": 1, "role": "worker", "state": "launched",
             "processes": 1, "reservation": {"phase": "ready", "bytes": READY.to_string()},
             "residency": "deep", "last_error": null},
        ])
    );
}

// T20 T32 (ADR 0028 §11, §12, §15; Task 8 concern): a parked group shows
// each member parked at its own budget on its own host; a member whose host
// is unreachable shows its host, rank and the full charge it keeps, so the
// operator sees why capacity is held.
#[test]
fn a_group_status_shows_parked_and_uncertain_members_with_their_charge() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    parked(&w, &g, T0 + 10);
    let s = instance_status(&w, &g);
    for rank in 0..2 {
        assert_eq!(
            s["members"][rank]["reservation"],
            json!({"phase": "parked", "bytes": PARKED.to_string()}),
            "{s}"
        );
    }
    let id = &g.fence.deployment_id;
    w.store
        .mark_member_uncertain(id, 0, g.generation, 1)
        .unwrap();
    let member = instance_status(&w, &g)["members"][1].clone();
    assert_eq!(member["host"], "host-b");
    assert_eq!(member["node_rank"], 1);
    assert_eq!(member["state"], "uncertain");
    assert_eq!(member["last_error"], "group_member_uncertain");
    assert_eq!(
        member["reservation"],
        json!({"phase": "parked", "bytes": PARKED.to_string()})
    );
}

// T32 T33 (ADR 0028 §11, §16): the rank a group failed at reads `failed`
// with the recorded code (`group_member_failed`, or the group's own code at
// rank 0 such as `group_stalled`); a member proven gone by its own host
// holds nothing and counts no process, and the failed rank keeps showing
// why once it settled.
#[test]
fn a_group_status_names_the_failed_rank_and_what_settled() {
    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    let id = g.fence.deployment_id.clone();
    w.store
        .record_group_failure(&id, 0, g.generation, 1)
        .unwrap();
    let s = instance_status(&w, &g);
    assert_eq!(s["last_error"], "group_member_failed");
    assert_eq!(s["members"][1]["state"], "failed");
    assert_eq!(s["members"][1]["last_error"], "group_member_failed");
    assert_eq!(s["members"][0]["state"], "launched");
    assert_eq!(s["members"][0]["last_error"], Value::Null);
    for rank in 0..2u32 {
        w.store
            .settle_member(
                &id,
                0,
                g.generation,
                rank,
                capyctl_store::groups::MemberGone {
                    member: MemberKey {
                        host_id: HOSTS[rank as usize].into(),
                        member_id: member_id(rank),
                    },
                    identities: member_identities(rank, 8100),
                },
            )
            .unwrap();
    }
    let s = instance_status(&w, &g);
    assert_eq!(s["members"][0]["state"], "settled", "{s}");
    assert_eq!(s["members"][1]["state"], "failed", "{s}");
    assert_eq!(s["members"][1]["last_error"], "group_member_failed");
    for rank in 0..2 {
        assert_eq!(s["members"][rank]["reservation"], Value::Null, "{s}");
        assert_eq!(s["members"][rank]["processes"], 0, "{s}");
    }

    let w = world([16, 16]);
    let g = w.ready("g", T0, 8100);
    assert!(w
        .store
        .record_group_stall(&w.session, &g.fence.deployment_id, 0, g.generation)
        .unwrap());
    let s = instance_status(&w, &g);
    assert_eq!(s["members"][0]["state"], "failed");
    assert_eq!(s["members"][0]["last_error"], "group_stalled");
    assert_eq!(s["members"][1]["last_error"], Value::Null);
}

// T37 (ADR 0028 §2.1, R10): a group whose engine environment every host
// approves shows neither the value nor the name in status.
#[test]
fn a_group_status_carries_no_environment_value() {
    let mut w = world([16, 16]);
    for (_, document) in &mut w.documents {
        document["runtime_profiles"]["local"]["security"]["approved_env"] =
            json!(["GROUP_STATUS_FLAG"]);
    }
    w.config["engine_config"]["env"] = json!({"GROUP_STATUS_FLAG": "zq-env-value"});
    let g = w.ready("g", T0, 8100);
    let snapshot = serde_json::to_string(&w.store.snapshot().unwrap()).unwrap();
    assert!(!snapshot.contains("zq-env-value"), "{snapshot}");
    assert!(!snapshot.contains("GROUP_STATUS_FLAG"), "{snapshot}");
    assert_eq!(instance_status(&w, &g)["peer_transport"], "unauthenticated");
}
