//! ADR 0028 §2, §3: a multi-node group deployment resolved on every named
//! host at deploy time: the head's resolution is canonical, every member keeps
//! its rank-ordered host list, and a named host that cannot run the group
//! refuses the whole deploy with its closed code before anything is stored.
//!
//! CPU-only: nothing launches. Passing here never qualifies a group recipe;
//! the live MN rows do (SPEC §18).
use capyctl_config::remote_resources::scope_host_document;
use capyctl_domain::resources::MemoryObservation;
use capyctl_store::dispatch::CoordinatorSession;
use capyctl_store::managed_configuration::{
    HostTarget, ManagedConfigurationError, ManagedConfigurationReceipt,
};
use capyctl_store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const NOW: i64 = 10_000;
/// Host id, enrolled name and peer address; the head is host B, so the
/// canonical resolution is the head's, not the first host id.
const HOSTS: [(&str, &str, &str); 2] = [
    ("host-a", "spark-a", "192.0.2.10"),
    ("host-b", "spark-b", "192.0.2.11"),
];

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    sql: Connection,
    session: CoordinatorSession,
    config: Value,
    hosts: Vec<(String, String, Value)>,
}

fn golden() -> (Value, Value) {
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    (
        golden["input"]["deployment"].clone(),
        golden["input"]["host"].clone(),
    )
}

/// Two enrolled hosts with the golden host policy and their peer addresses,
/// and a TP 2 group naming host B (spark-b) as its head. The golden
/// deployment names device `gpu0`: one device per host (R20).
fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.sqlite3");
    let store = Store::open(&path).unwrap();
    let sql = Connection::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let (mut config, host) = golden();
    let mut hosts = Vec::new();
    for (id, name, peer) in HOSTS {
        sql.execute(
            "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES(?1,?2,'key',0)",
            params![id, name],
        )
        .unwrap();
        let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
        store
            .import_remote_resource_policy(&session, id, &policy, &[observed()], 1)
            .unwrap();
        let mut document = host.clone();
        document["resource_policy"]["groups"] = json!({ "peer_address": peer });
        hosts.push((id.to_owned(), name.to_owned(), document));
    }
    config.as_object_mut().unwrap().remove("host");
    config["topology"] = json!({"tensor_parallel": 2});
    config["placement"] = json!({"hosts": ["spark-b", "spark-a"]});
    World {
        _dir: dir,
        store,
        sql,
        session,
        config,
        hosts,
    }
}

fn observed() -> MemoryObservation {
    MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 1 << 40,
        available_bytes: 1 << 40,
        sampled_at_ms: 1,
    }
}

impl World {
    fn document(&mut self, id: &str) -> &mut Value {
        &mut self.hosts.iter_mut().find(|(h, ..)| h == id).unwrap().2
    }

    /// The registry's view of each enrolled host: its published document
    /// scoped to its ledger keys and composed with its current controls.
    fn targets(&self) -> Vec<HostTarget> {
        self.hosts
            .iter()
            .map(|(id, name, document)| {
                let trusted = scope_host_document(id, document).unwrap();
                let policy = self.store.resource_policy(id).unwrap().unwrap();
                HostTarget {
                    host_id: id.clone(),
                    host_name: name.clone(),
                    trusted_host: capyctl_config::effective::compose_current_resource_controls(
                        &trusted,
                        &policy.context,
                        &policy.controls,
                    )
                    .unwrap(),
                    scoped: true,
                }
            })
            .collect()
    }

    fn deploy_on(
        &self,
        key: &str,
        targets: &[HostTarget],
    ) -> Result<ManagedConfigurationReceipt, ManagedConfigurationError> {
        self.store.create_managed_configuration_on_hosts(
            &self.session,
            "owner",
            key,
            &json!({ "config": self.config }).to_string(),
            targets,
            &[],
            NOW,
        )
    }

    fn deploy(&self, key: &str) -> Result<ManagedConfigurationReceipt, ManagedConfigurationError> {
        self.deploy_on(key, &self.targets())
    }

    /// The closed reason a refused deploy names; nothing was stored.
    fn refusal(&self, key: &str) -> String {
        let error = self.deploy(key).unwrap_err();
        assert_eq!(self.store.deployment_count().unwrap(), 0, "{error}");
        match error {
            ManagedConfigurationError::Rejected(error) => error.to_string(),
            other => panic!("expected a configuration refusal, got {other:?}"),
        }
    }
}

// T03, T14: a group resolves on every named host; the head's resolution is the
// canonical revision, and each host's recipe keeps the rank-ordered host list.
#[test]
fn a_group_resolves_on_every_named_host_with_the_head_canonical() {
    let w = world();
    let receipt = w.deploy("group").unwrap();
    let canonical: String = w
        .sql
        .query_row(
            "SELECT json_extract(effective_json,'$.host.name') FROM effective_revisions WHERE deployment_id=?1",
            [&receipt.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(canonical, "host-b", "the head's resolution is canonical");
    let rows: Vec<(String, String, String)> = w
        .sql
        .prepare(
            "SELECT host_id,outcome,source_json FROM host_effective_revisions WHERE deployment_id=?1 ORDER BY host_id",
        )
        .unwrap()
        .query_map([&receipt.deployment_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    for (host, outcome, source) in rows {
        assert_eq!(outcome, "resolved", "{host}");
        let source: Value = serde_json::from_str(&source).unwrap();
        assert_eq!(source["placement"]["hosts"], json!(["spark-b", "spark-a"]));
        assert_eq!(source["topology"]["tensor_parallel"], 2);
        let spec = capyctl_config::instances::parse_instance_spec(&source).unwrap();
        assert_eq!(spec.group.unwrap().head(), "spark-b");
    }
    let accepted: String = w
        .sql
        .query_row(
            "SELECT config_json FROM managed_configuration_sources WHERE deployment_id=?1",
            [&receipt.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let accepted: Value = serde_json::from_str(&accepted).unwrap();
    assert_eq!(
        accepted["placement"]["hosts"],
        json!(["spark-b", "spark-a"])
    );
}

// T03 (ADR 0028 §5, amendment of 2026-10-07): a group whose memory derives
// from its weights is accepted provisional; the head's measurement, with the
// checkpoint's layout, re-resolves every member to its own share of the
// weights, and each member is charged that share, not the whole checkpoint.
#[test]
fn a_measured_group_charges_each_member_its_share_of_the_weights() {
    const GIB: i64 = 1 << 30;
    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut w = world();
    let config = w.config.as_object_mut().unwrap();
    config.remove("resources");
    config.remove("residency");
    w.config["model"]["content_fingerprint"] = json!(DIGEST);
    let receipt = w.deploy("group").unwrap();
    let layout = capyctl_domain::member_weights::CheckpointLayout {
        sharded_bytes: 18 * GIB,
        layer_count: 24,
        largest_layer_bytes: 3 * GIB / 4,
    };
    let outcome = w
        .store
        .record_checkpoint_measurement(
            &w.session,
            &receipt.deployment_id,
            receipt.revision,
            "host-b",
            DIGEST,
            20 * GIB,
            None,
            Some(layout),
            capyctl_config::effective::DigestProvenance::Measured,
            None,
            None,
            NOW,
        )
        .unwrap();
    assert!(matches!(
        outcome,
        capyctl_store::checkpoint_digests::RecordOutcome::Recorded { .. }
    ));
    let members = w
        .store
        .group_member_resolutions(
            &receipt.deployment_id,
            receipt.revision,
            &["host-b".to_owned(), "host-a".to_owned()],
        )
        .unwrap();
    assert_eq!(members.len(), 2);
    for member in members {
        let memory = member.effective.engine_config.memory();
        // Half of the 18 GiB the layers split, and the 2 GiB kept whole.
        assert_eq!(memory.weights_bytes, Some(11 * GIB), "{}", member.host_id);
        assert_eq!(memory.checkpoint_weights_bytes(), Some(20 * GIB));
        assert_eq!(memory.member.unwrap().layout, Some(layout));
        let ready = member.effective.resources.ready.allocations[0].bytes;
        assert_eq!(
            ready,
            11 * GIB
                + 4 * GIB
                + memory.margin_bytes
                + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES
        );
        assert!(member.cold.allocations[0].bytes < 20 * GIB * 9 / 4);
    }
}

// T14: one mismatched build, or a profile one named host lacks, refuses the deploy.
#[test]
fn every_named_host_runs_the_same_profile_build() {
    let mut w = world();
    w.document("host-a")["runtime_profiles"]["local"]["build_fingerprint"] = json!("other");
    let refusal = w.refusal("build");
    assert!(refusal.contains("group_profile_mismatch"), "{refusal}");
    assert!(refusal.contains("host-a"), "{refusal}");
    let mut w = world();
    let profile = w.document("host-a")["runtime_profiles"]["local"].take();
    w.document("host-a")["runtime_profiles"] = json!({ "other": profile });
    let refusal = w.refusal("missing");
    assert!(refusal.contains("group_profile_mismatch"), "{refusal}");
}

// T14: a named host without a peer address cannot be in a group.
#[test]
fn a_named_host_without_a_peer_address_is_refused() {
    let mut w = world();
    w.document("host-a")["resource_policy"]
        .as_object_mut()
        .unwrap()
        .remove("groups");
    let refusal = w.refusal("peer");
    assert!(refusal.contains("peer_address_missing"), "{refusal}");
    assert!(refusal.contains("spark-a"), "{refusal}");
}

// T14, T37: an engine env name must be approved on every named host.
#[test]
fn engine_env_is_approved_on_every_named_host() {
    let mut w = world();
    w.config["engine_config"]["env"] = json!({"VLLM_GROUP_PROBE": "1"});
    w.document("host-b")["runtime_profiles"]["local"]["security"]["approved_env"] =
        json!(["VLLM_GROUP_*"]);
    let refusal = w.refusal("env");
    assert!(
        refusal.contains("engine_env_not_approved:VLLM_GROUP_PROBE"),
        "{refusal}"
    );
    w.document("host-a")["runtime_profiles"]["local"]["security"]["approved_env"] =
        json!(["VLLM_GROUP_*"]);
    w.deploy("env-approved").unwrap();
}

// T34 (main #65): the capability gate's decision is made for every member; a
// deep SGLang group whose modelopt weights no rank can reload is refused.
#[test]
fn a_member_the_engine_cannot_park_refuses_the_group() {
    let mut w = world();
    for (id, ..) in HOSTS {
        let profile = &mut w.document(id)["runtime_profiles"]["local"];
        profile["engine"] = json!("sglang");
        profile["args"] = json!([]);
        profile["security"]["admin_credential_ref"] = json!("secret://engine-admin");
    }
    w.config["engine_config"]["quantization"] = json!("modelopt_fp4");
    let refusal = w.refusal("modelopt");
    assert!(
        refusal.contains("capability_missing:deep_park"),
        "{refusal}"
    );
    // T39: a single-host deploy of the same document is still accepted (its
    // host refuses the launch), so the refusal above is the group's gate.
    let mut single = w.config.clone();
    single.as_object_mut().unwrap().remove("topology");
    single["placement"] = json!({"hosts": ["spark-b"]});
    w.store
        .create_managed_configuration_on_hosts(
            &w.session,
            "owner",
            "modelopt-single",
            &json!({ "config": single }).to_string(),
            &w.targets(),
            &[],
            NOW,
        )
        .unwrap();
    w.config["name"] = json!("toy-restart");
    w.config["routes"] = json!(["toy-restart"]);
    w.config["residency"] = json!("restart_only");
    w.deploy("modelopt-restart").unwrap();
}

// T14: a named host the server cannot resolve against refuses the group; it is
// never deployed on the hosts that remain.
#[test]
fn a_named_host_without_a_target_refuses_the_group() {
    let w = world();
    let head_only: Vec<HostTarget> = w
        .targets()
        .into_iter()
        .filter(|t| t.host_id == "host-b")
        .collect();
    assert!(w.deploy_on("head-only", &head_only).is_err());
    assert_eq!(w.store.deployment_count().unwrap(), 0);
}

impl World {
    /// Deployment `name` from `config`, accepted on every enrolled host.
    fn create(&self, name: &str, config: &Value) -> ManagedConfigurationReceipt {
        self.store
            .create_managed_configuration_on_hosts(
                &self.session,
                "owner",
                name,
                &json!({ "config": config }).to_string(),
                &self.targets(),
                &[],
                NOW,
            )
            .unwrap()
    }

    /// Group `g` (head host B, worker host A) as its activation leaves it at
    /// generation 1: the head's member `head`, the worker's member `worker`.
    fn group_with_members(&self, head: &str, worker: &str) -> String {
        let id = self.deploy("g").unwrap().deployment_id;
        self.sql
            .execute(
                "INSERT INTO group_plans(deployment_id,instance_index,generation,plan_json,rendezvous_host,rendezvous_port,state)
                 VALUES(?1,0,1,'{}','host-b',25000,?2)",
                params![id, if head == "settled" && worker == "settled" { "settled" } else { "active" }],
            )
            .unwrap();
        for (rank, host, state) in [(0, "host-b", head), (1, "host-a", worker)] {
            let dispatched = state != "reserved";
            let identities =
                (state == "launched" || (state == "settled" && dispatched)).then_some("[]");
            self.sql
                .execute(
                    "INSERT INTO group_members(deployment_id,instance_index,generation,rank,host_id,owner_id,state,dispatched,identities_json)
                     VALUES(?1,0,1,?2,?3,'deployment:'||?1||'/instance:0/member:'||?2,?4,?5,?6)",
                    params![id, rank, host, state, dispatched, identities],
                )
                .unwrap();
        }
        id
    }

    /// A single-rank deployment `name` on host A (spark-a) alone.
    fn single_on_host_a(&self, name: &str) -> String {
        let mut config = self.config.clone();
        config.as_object_mut().unwrap().remove("topology");
        config["name"] = json!(name);
        config["routes"] = json!([name]);
        config["placement"] = json!({"hosts": ["spark-a"]});
        self.create(name, &config).deployment_id
    }

    /// Another group `name` over host B (head) and host A.
    fn another_group(&self, name: &str) -> String {
        let mut config = self.config.clone();
        config["name"] = json!(name);
        config["routes"] = json!([name]);
        self.create(name, &config).deployment_id
    }

    /// An on-demand start of `deployment`, placed without eviction.
    fn start(&self, deployment: &str) -> Result<(), capyctl_store::lifecycle::LifecycleError> {
        let revision: i64 = self
            .sql
            .query_row(
                "SELECT revision FROM deployments WHERE id=?1",
                [deployment],
                |r| r.get(0),
            )
            .unwrap();
        self.store
            .accept_scoped_start_command(
                &self.session,
                "owner",
                deployment,
                capyctl_store::ordinary_lifecycle::placement::StartScope::OnDemand,
                revision,
                &format!("start-{deployment}"),
                NOW,
                NOW + 190_000,
                None,
            )
            .map(drop)
    }

    /// Whether `deployment`'s start is refused because host A (`host-a`)
    /// cannot take another launch.
    fn refused_host_a_occupied(&self, deployment: &str) -> bool {
        matches!(
            self.start(deployment),
            Err(capyctl_store::lifecycle::LifecycleError::CapacityBlocked(Some(detail)))
                if detail == "host host-a: host_occupied"
        )
    }
}

// T16, T30 (ADR 0028 §5, §11; SPEC §§3.1, 7.3): a single-claim host running
// only a group's worker member cannot take another launch, whatever that
// member's state short of settled: uncertainty keeps accounting, so an
// uncertain or dispatching member occupies its host as a launched one does.
// Neither a single-rank start nor another group is placed there.
#[test]
fn a_host_running_a_group_worker_member_is_occupied() {
    for state in ["reserved", "dispatching", "launched", "uncertain"] {
        let w = world();
        w.group_with_members("settled", state);
        let single = w.single_on_host_a("single");
        assert!(w.refused_host_a_occupied(&single), "{state}");
        let other = w.another_group("other");
        assert!(w.refused_host_a_occupied(&other), "{state}");
    }
}

// T16, T30, T39: a settled member frees its host, and a host fencing per
// instance takes another launch beside a member, judged by memory alone.
// Each start runs in its own world (a started single-rank occupies host A).
#[test]
fn a_settled_member_or_a_per_instance_host_takes_another_launch() {
    let per_instance = |w: &World| {
        for (host, ..) in HOSTS {
            w.sql
                .execute(
                    "INSERT INTO host_launch_claims(host_id,mode,recorded_at_ms) VALUES(?1,'per_instance',1)",
                    [host],
                )
                .unwrap();
        }
    };
    for (members, fenced) in [("settled", false), ("launched", true), ("uncertain", true)] {
        for group in [false, true] {
            let w = world();
            w.group_with_members("settled", members);
            if fenced {
                per_instance(&w);
            }
            let next = if group {
                w.another_group("other")
            } else {
                w.single_on_host_a("single")
            };
            w.start(&next)
                .unwrap_or_else(|e| panic!("{members} {fenced} {group}: {e:?}"));
        }
    }
}

impl World {
    /// The worker member of group `id` (rank 1, on host A) settles on host
    /// A's evidence, and with it the plan.
    fn settle_worker(&self, id: &str) {
        self.sql
            .execute(
                "UPDATE group_members SET state='settled' WHERE deployment_id=?1 AND rank=1",
                [id],
            )
            .unwrap();
        self.sql
            .execute(
                "UPDATE group_plans SET state='settled' WHERE deployment_id=?1",
                [id],
            )
            .unwrap();
    }
}

// T30, T31 (SPEC §4.3; ADR 0028 §5, §11): a drain of host A, which runs only
// group g's worker member, names g whenever that member is not settled (an
// uncertain one included), and names nothing once it settled. A drain of the
// embedded host never names a group by a member.
#[test]
fn a_drain_names_a_group_by_its_unsettled_member() {
    use capyctl_store::host_drain::{DrainCandidate, DrainHost};
    for state in [
        "reserved",
        "dispatching",
        "launched",
        "uncertain",
        "settled",
    ] {
        let w = world();
        let g = w.group_with_members("settled", state);
        let named = w
            .store
            .drain_candidates(DrainHost::Remote("host-a"))
            .unwrap();
        let expected = if state == "settled" {
            vec![]
        } else {
            vec![DrainCandidate {
                deployment_id: g,
                revision: 1,
                instance: 0,
            }]
        };
        assert_eq!(named, expected, "{state}");
        assert!(w
            .store
            .drain_candidates(DrainHost::Remote("host-b"))
            .unwrap()
            .is_empty());
        assert!(w
            .store
            .drain_candidates(DrainHost::Embedded)
            .unwrap()
            .is_empty());
    }
}

// T30, T32 (ADR 0028 §11): a drain of host A stays pending while g's member
// there is not settled, even once the group's Stop ended: uncertainty keeps
// accounting. It clears when the member settles on host A's evidence.
#[test]
fn a_drain_holds_while_the_drained_member_is_unsettled() {
    let w = world();
    let g = w.group_with_members("settled", "uncertain");
    w.sql
        .execute(
            "INSERT INTO operations(id,deployment_id,kind,state) VALUES('stop-g',?1,'ordinary_cleanup','failed')",
            [&g],
        )
        .unwrap();
    w.store
        .record_host_drain("host-a", &["stop-g".to_string()], NOW)
        .unwrap();
    assert!(w.store.host_drain_pending("host-a").unwrap());
    assert!(w
        .store
        .hosts_with_pending_drain()
        .unwrap()
        .contains("host-a"));
    // A later drain of the host keeps the marker of the unsettled member.
    w.sql
        .execute(
            "INSERT INTO operations(id,kind,state) VALUES('stop-other','ordinary_cleanup','succeeded')",
            [],
        )
        .unwrap();
    w.store
        .record_host_drain("host-a", &["stop-other".to_string()], NOW + 1)
        .unwrap();
    assert!(w.store.host_drain_pending("host-a").unwrap());
    assert!(!w.store.host_drain_pending("host-b").unwrap());
    w.settle_worker(&g);
    assert!(!w.store.host_drain_pending("host-a").unwrap());
    assert!(w.store.hosts_with_pending_drain().unwrap().is_empty());
}

// T16, T30 (ADR 0018 §4; ADR 0028 §5, §11): retiring the profile on host A
// counts g's member there as a user of it: refused without drain, waiting
// with drain until the member settles, then confirmed.
#[test]
fn a_profile_retirement_counts_a_group_member_as_a_user() {
    use capyctl_store::profile_retirement::{RetirementProgress, RetirementStart};
    let w = world();
    let g = w.group_with_members("settled", "uncertain");
    let names = |start: RetirementStart| match start {
        RetirementStart::InUse(named) | RetirementStart::Draining(named) => named
            .into_iter()
            .map(|c| (c.deployment_id, c.instance))
            .collect::<Vec<_>>(),
        RetirementStart::Clear => vec![],
    };
    let refused = w
        .store
        .begin_profile_retirement("host-a", "local", "retire", NOW, NOW + 60_000, false)
        .unwrap();
    assert!(matches!(refused, RetirementStart::InUse(_)), "{refused:?}");
    assert_eq!(names(refused), [(g.clone(), 0)]);
    let draining = w
        .store
        .begin_profile_retirement("host-a", "local", "retire", NOW, NOW + 60_000, true)
        .unwrap();
    assert!(matches!(draining, RetirementStart::Draining(_)));
    assert!(matches!(
        w.store
            .profile_retirement_progress("host-a", "local", "retire", NOW + 1)
            .unwrap(),
        RetirementProgress::Waiting(named) if named.len() == 1
    ));
    w.settle_worker(&g);
    assert_eq!(
        w.store
            .profile_retirement_progress("host-a", "local", "retire", NOW + 2)
            .unwrap(),
        RetirementProgress::Settled
    );
    // Host B's head member settled: the profile there is not in use.
    assert_eq!(
        w.store
            .begin_profile_retirement("host-b", "local", "retire-b", NOW, NOW + 60_000, false)
            .unwrap(),
        RetirementStart::Clear
    );
}
