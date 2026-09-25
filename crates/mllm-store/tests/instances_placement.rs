//! ADR 0013 §3–§7, §9 (unit I2): a deployment's instances placed across two
//! enrolled hosts and driven through the store's own lifecycle — resolution
//! against every allowed host, placement at start, per-instance fences and
//! accounting, count changes while running, the owner decision Q8 restart and
//! the owner decision Q5 on-demand choice.
//!
//! CPU-only: every launch is recorded from scripted evidence; no engine runs.
//! Passing here never qualifies a native engine recipe (SPEC §18).
use mllm_config::remote_resources::{ledger_key, scope_host_document};
use mllm_domain::completion::{
    CleanupEvidence, CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity,
};
use mllm_domain::resources::{MemoryLimit, MemoryObservation};
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::dispatch::CoordinatorSession;
use mllm_store::managed_configuration::{HostRefusal, HostTarget, ManagedConfigurationReceipt};
use mllm_store::ordinary_lifecycle::placement::StartScope;
use mllm_store::ordinary_lifecycle::reconcile::Reconciled;
use mllm_store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const NOW: i64 = 10_000;
const DEADLINE: i64 = 200_000;
const HOSTS: [(&str, &str); 2] = [("host-a", "spark-a"), ("host-b", "spark-b")];

struct TwoHosts {
    _dir: tempfile::TempDir,
    store: Store,
    sql: Connection,
    session: CoordinatorSession,
    config: Value,
    targets: Vec<HostTarget>,
}

fn golden() -> (Value, Value) {
    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    (
        golden["input"]["deployment"].clone(),
        golden["input"]["host"].clone(),
    )
}

/// Two enrolled hosts with the golden host policy, each with its own resource
/// namespace and its own current policy, and the deployment document written
/// for an allowed set spanning both: devices stated by sharing mode only.
fn two_hosts(managed_limit: &str) -> TwoHosts {
    two_hosts_sharing(managed_limit, "shared")
}

fn two_hosts_sharing(managed_limit: &str, sharing: &str) -> TwoHosts {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("instances.sqlite3");
    let store = Store::open(&path).unwrap();
    let sql = Connection::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let (mut config, mut host) = golden();
    host["resource_policy"]["domains"]["unified"]["managed_limit"] = json!(managed_limit);
    host["resource_policy"]["devices"]["gpu0"]["sharing"] = json!(sharing);
    host["resource_policy"]["device_sharing"] = json!(sharing);
    for (id, name) in HOSTS {
        sql.execute(
            "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES(?1,?2,'key',0)",
            params![id, name],
        )
        .unwrap();
        let policy = mllm_config::effective::normalize_host_policy(&host).unwrap();
        store
            .import_remote_resource_policy(&session, id, &policy, &[observed("unified", 1)], 1)
            .unwrap();
    }
    let targets = HOSTS
        .iter()
        .map(|(id, name)| target(&store, id, name, &host))
        .collect();
    config.as_object_mut().unwrap().remove("host");
    config["devices"] = json!([{ "sharing": sharing }]);
    for phase in ["cold", "ready", "parking", "wake"] {
        config["resources"][phase]["devices"] = json!([{ "sharing": sharing }]);
    }
    config["placement"] = json!({"hosts": ["spark-a", "spark-b"]});
    TwoHosts {
        _dir: dir,
        store,
        sql,
        session,
        config,
        targets,
    }
}

fn observed(domain: &str, at: i64) -> MemoryObservation {
    MemoryObservation {
        domain: domain.into(),
        capacity_bytes: 1 << 40,
        available_bytes: 1 << 40,
        sampled_at_ms: at,
    }
}

/// The registry's view of one enrolled host: its document scoped to its
/// ledger keys, composed with its current persisted resource controls.
fn target(store: &Store, id: &str, name: &str, host: &Value) -> HostTarget {
    let trusted = scope_host_document(id, host).unwrap();
    let policy = store.resource_policy(id).unwrap().unwrap();
    HostTarget {
        host_id: id.into(),
        host_name: name.into(),
        trusted_host: mllm_config::effective::compose_current_resource_controls(
            &trusted,
            &policy.context,
            &policy.controls,
        )
        .unwrap(),
        scoped: true,
    }
}

impl TwoHosts {
    fn deploy(&self, key: &str, patch: Value) -> ManagedConfigurationReceipt {
        let mut config = self.config.clone();
        for (field, value) in patch.as_object().unwrap() {
            config[field] = value.clone();
        }
        self.store
            .create_managed_configuration_on_hosts(
                &self.session,
                "owner",
                key,
                &json!({ "config": config }).to_string(),
                &self.targets,
                &[],
                NOW,
            )
            .unwrap()
    }

    fn replace(
        &self,
        id: &str,
        key: &str,
        expected: i64,
        patch: Value,
    ) -> ManagedConfigurationReceipt {
        let mut config = self.config.clone();
        for (field, value) in patch.as_object().unwrap() {
            config[field] = value.clone();
        }
        self.store
            .replace_managed_configuration_on_hosts(
                &self.session,
                "owner",
                key,
                id,
                &json!({ "config": config, "expected_revision": expected }).to_string(),
                &self.targets,
                &[],
                NOW,
            )
            .unwrap()
    }

    fn start(&self, id: &str, key: &str, scope: StartScope, eligible: Option<&BTreeSet<String>>) {
        self.store
            .accept_scoped_start_command(
                &self.session,
                "owner",
                id,
                scope,
                self.revision(id),
                key,
                NOW,
                DEADLINE,
                eligible,
            )
            .unwrap();
    }

    fn revision(&self, id: &str) -> i64 {
        self.store.current_revision(id).unwrap().unwrap()
    }

    /// Every planned start: (step, instance, host).
    fn planned(&self, id: &str) -> Vec<(String, u32, String)> {
        self.sql
            .prepare(
                "SELECT s.id,b.instance_index,i.host_id FROM lifecycle_steps s
                   JOIN operations o ON o.id=s.operation_id AND o.kind='initialize'
                   JOIN runtime_bindings b ON b.id=s.binding_id
                   JOIN deployment_instances i ON i.deployment_id=b.deployment_id AND i.instance_index=b.instance_index
                  WHERE s.deployment_id=?1 AND s.state='planned' ORDER BY b.instance_index",
            )
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn admission(&self, host: &str) -> (Vec<MemoryObservation>, Vec<MemoryLimit>, i64, usize) {
        let policy = self.store.resource_policy(host).unwrap().unwrap();
        let limits = policy
            .controls
            .domains
            .iter()
            .map(|(domain, d)| MemoryLimit {
                domain: domain.clone(),
                managed_bytes: d.managed_limit,
                free_reserve_bytes: d.free_reserve,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect();
        (
            vec![observed(&ledger_key(host, "domain", "unified"), NOW)],
            limits,
            policy.controls.observation_ttl_ms,
            policy.controls.max_parked as usize,
        )
    }

    /// Arm, associate and complete one planned start on scripted evidence.
    fn ready(&self, step: &str, host: &str, pid: u32) {
        let (observations, limits, ttl, max_parked) = self.admission(host);
        let (_, context) = self
            .store
            .arm_initialize_with_context(
                &self.session,
                step,
                AdmissionContext::new(&observations, &limits, NOW, ttl, max_parked),
            )
            .unwrap();
        let context = context.unwrap();
        let group = vec![identity("api", pid), identity("worker-0", pid + 1)];
        self.store
            .record_owned_launch(
                &self.session,
                step,
                &OwnedLaunchReceipt {
                    binding_id: context.binding_id.clone(),
                    incarnation: context.incarnation.clone(),
                    identities: group.clone(),
                    observed_at_ms: NOW,
                    receipt: "scripted host native model probe".into(),
                },
                NOW,
            )
            .unwrap();
        self.store
            .complete_step(
                &self.session,
                step,
                &CompletionEvidence {
                    token: context.token,
                    identities: group,
                    observed_at_ms: NOW,
                    control_receipt: Some("scripted host native model probe".into()),
                    milestones: vec![
                        Milestone::AllocationsRestored,
                        Milestone::WeightsUsable,
                        Milestone::CacheValid,
                        Milestone::ModelUsable,
                    ],
                },
                NOW,
                ttl,
            )
            .unwrap();
    }

    /// Complete an accepted stop on scripted gone evidence.
    fn cleaned(&self, step: &str) {
        let (_, context) = self
            .store
            .arm_ordinary_cleanup_with_context(&self.session, step, NOW)
            .unwrap();
        let context = context.unwrap();
        let ttl = self.admission(HOSTS[0].0).2;
        self.store
            .complete_cleanup(
                &self.session,
                step,
                &CleanupEvidence {
                    binding_id: context.binding_id,
                    incarnation: context.incarnation,
                    identities: context.identities,
                    observed_at_ms: NOW,
                    receipt: "scripted host observed the owned group gone".into(),
                },
                NOW,
                ttl,
            )
            .unwrap();
    }

    fn cleanup_step(&self, operation: &str) -> String {
        self.sql
            .query_row(
                "SELECT id FROM lifecycle_steps WHERE operation_id=?1",
                [operation],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn status(&self, id: &str) -> mllm_store::snapshot::DeploymentSnapshot {
        self.store
            .snapshot()
            .unwrap()
            .deployments
            .into_iter()
            .find(|d| d.id == id)
            .unwrap()
    }

    fn owners(&self) -> Vec<String> {
        self.store
            .resource_snapshot()
            .unwrap()
            .owners
            .into_keys()
            .collect()
    }

    fn reconcile(&self) -> Vec<Reconciled> {
        self.store
            .reconcile_instances(&self.session, NOW, None, true)
            .unwrap()
    }
}

fn identity(role: &str, pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: role.into(),
        pid,
        boot_id: "boot".into(),
        start_ticks: u64::from(pid) * 10,
    }
}

/// Start every instance and drive each planned start to Ready.
fn all_ready(t: &TwoHosts, id: &str, key: &str) {
    t.start(id, key, StartScope::All, None);
    for (n, (step, _, host)) in t.planned(id).into_iter().enumerate() {
        t.ready(&step, &host, 100 + 10 * n as u32);
    }
}

/// ADR 0013 §3: the deployment is resolved against every allowed host; each
/// resolving host is recorded with its own revision and source, a host that
/// cannot be resolved is recorded with its reason, and no host is chosen and
/// nothing is reserved at deploy time.
// T03 T14 T16
#[test]
fn deploy_resolves_on_every_allowed_host_and_records_refusals() {
    let t = two_hosts("32GiB");
    let mut config = t.config.clone();
    config["instances"] = json!(2);
    config["placement"] = json!({"hosts": ["spark-a", "spark-b", "spark-c"]});
    let receipt = t
        .store
        .create_managed_configuration_on_hosts(
            &t.session,
            "owner",
            "deploy",
            &json!({ "config": config }).to_string(),
            &t.targets,
            &[HostRefusal {
                host_id: "spark-c".into(),
                diagnostic: "host_unpublished".into(),
            }],
            NOW,
        )
        .unwrap();
    let status = t.status(&receipt.deployment_id);
    assert_eq!(
        status
            .hosts
            .iter()
            .map(|h| (
                h.host_id.as_str(),
                h.outcome.as_str(),
                h.diagnostic.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("host-a", "resolved", None),
            ("host-b", "resolved", None),
            ("spark-c", "refused", Some("host_unpublished")),
        ]
    );
    // Each host's revision names its own devices: the unnamed claim took the
    // host's one GPU under its own ledger key.
    for (id, _) in HOSTS {
        let source = t
            .store
            .host_configuration_source(&receipt.deployment_id, 1, id)
            .unwrap()
            .unwrap();
        assert_eq!(source["devices"][0]["id"], ledger_key(id, "device", "gpu0"));
    }
    assert!(status.instances.iter().all(|i| i.host_id.is_none()));
    assert!(t.owners().is_empty(), "nothing is reserved at deploy time");
    // An exact retry is the same command.
    let replay = t
        .store
        .create_managed_configuration_on_hosts(
            &t.session,
            "owner",
            "deploy",
            &json!({ "config": config }).to_string(),
            &t.targets,
            &[],
            NOW,
        )
        .unwrap();
    assert_eq!(replay, receipt);
}

/// ADR 0013 §4, §5, §6: an explicit start places each instance (spread: one
/// per host), draws each its own generation from the deployment's counter,
/// charges each its own resource owner and binding, and status reports each
/// instance and the aggregate.
// T05 T15 T16 T27 T29
#[test]
fn an_explicit_start_spreads_instances_with_their_own_fences_and_owners() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    t.start(&id, "start", StartScope::All, None);
    let planned = t.planned(&id);
    assert_eq!(
        planned
            .iter()
            .map(|(_, k, h)| (*k, h.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "host-a"), (1, "host-b")],
        "spread places one instance on each host, ties by host id"
    );
    let rows = t.store.deployment_instances(&id).unwrap();
    let generations: BTreeSet<_> = rows.iter().map(|r| r.generation.unwrap()).collect();
    assert_eq!(
        generations.len(),
        2,
        "each incarnation has its own generation"
    );
    assert_eq!(t.status(&id).observed_state, "queued");
    for (n, (step, _, host)) in planned.iter().enumerate() {
        t.ready(step, host, 100 + 10 * n as u32);
    }
    let owners: BTreeSet<String> = t.owners().into_iter().collect();
    assert_eq!(
        owners,
        BTreeSet::from([id.clone(), format!("deployment:{id}/instance:1")])
    );
    let status = t.status(&id);
    assert_eq!(status.observed_state, "ready");
    assert_eq!((status.desired_instances, status.ready_instances), (2, 2));
    assert!(status.conditions.is_empty());
    assert!(status
        .instances
        .iter()
        .all(|i| i.observed_state == "ready" && i.devices.is_some()));
}

/// Owner decision Q7: stopping one instance stops only it, with verified
/// cleanup and its own accounting released; the other keeps serving and the
/// deployment is `ready` and `degraded` meanwhile.
// T10 T16 T32
#[test]
fn stopping_one_instance_leaves_the_other_serving() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    all_ready(&t, &id, "start");
    let stop = t
        .store
        .accept_instance_stop_command(&t.session, "owner", &id, 1, 1, "stop-1", NOW, DEADLINE)
        .unwrap()
        .unwrap();
    let status = t.status(&id);
    assert_eq!(status.observed_state, "ready");
    assert_eq!(status.instances[1].observed_state, "stopping");
    assert_eq!(status.instances[0].observed_state, "ready");
    t.cleaned(&t.cleanup_step(stop.operation_id()));
    assert_eq!(
        t.owners(),
        vec![id.clone()],
        "only instance 1's owner released"
    );
    let status = t.status(&id);
    assert_eq!(status.conditions, vec!["degraded"]);
    assert_eq!(status.instances[1].observed_state, "stopped");
    assert!(t.store.dispatch_enabled(&id).unwrap());
    // Replayed, the stop is the same operation.
    assert_eq!(
        t.store
            .accept_instance_stop_command(&t.session, "owner", &id, 1, 1, "stop-1", NOW, DEADLINE)
            .unwrap()
            .unwrap()
            .operation_id(),
        stop.operation_id()
    );
}

/// ADR 0013 §7: a count increase while running is non-disruptive: the running
/// instances keep their incarnations (their own revision and generation) and
/// keep serving; the new instance is created stopped and starts on demand or
/// at the next start; reconciliation stops nothing.
// T09 T10 T18
#[test]
fn a_count_increase_while_running_keeps_running_instances() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    all_ready(&t, &id, "start");
    let before = t.store.deployment_instances(&id).unwrap();
    let receipt = t.replace(
        &id,
        "grow",
        1,
        json!({"instances": 3, "placement": {"hosts": ["spark-a", "spark-b"]}}),
    );
    assert_eq!(receipt.revision, 2);
    let after = t.store.deployment_instances(&id).unwrap();
    assert_eq!(after.len(), 3);
    assert_eq!(&after[..2], &before[..], "running instances untouched");
    assert!(
        t.reconcile().is_empty(),
        "a count-only revision stops nothing"
    );
    let status = t.status(&id);
    assert_eq!(status.observed_state, "ready");
    assert_eq!((status.desired_instances, status.ready_instances), (3, 2));
    assert_eq!(status.conditions, vec!["degraded"]);
    // The running instances still complete their own work on revision 1: a
    // stop of instance 0 is accepted against the deployment's revision 2.
    let stop = t
        .store
        .accept_instance_stop_command(&t.session, "owner", &id, 0, 2, "stop-0", NOW, DEADLINE)
        .unwrap()
        .unwrap();
    t.cleaned(&t.cleanup_step(stop.operation_id()));
    assert_eq!(t.owners(), vec![format!("deployment:{id}/instance:1")]);
}

/// ADR 0013 §7: a count decrease while running retires surplus instances in
/// the ADR order (a stopped one first) and drains a running surplus instance
/// with verified cleanup before its row is removed.
// T09 T10 T32
#[test]
fn a_count_decrease_retires_a_stopped_instance_first_then_drains_a_running_one() {
    let t = two_hosts("32GiB");
    let id = t
        .deploy(
            "deploy",
            json!({"instances": 3, "placement": {"hosts": ["spark-a", "spark-b"]}}),
        )
        .deployment_id;
    // Instances 0 and 1 run; 2 was never started.
    t.start(&id, "start", StartScope::Instance(0), None);
    t.start(&id, "start-1", StartScope::Instance(1), None);
    for (n, (step, _, host)) in t.planned(&id).into_iter().enumerate() {
        t.ready(&step, &host, 100 + 10 * n as u32);
    }
    t.replace(
        &id,
        "shrink",
        1,
        json!({"instances": 2, "placement": {"hosts": ["spark-a", "spark-b"]}}),
    );
    let rows = t.store.deployment_instances(&id).unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.index, r.state.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "active"), (1, "active")],
        "the stopped instance went first; nothing running was touched"
    );
    t.replace(
        &id,
        "shrink-more",
        2,
        json!({"instances": 1, "placement": {"hosts": ["spark-a", "spark-b"]}}),
    );
    let rows = t.store.deployment_instances(&id).unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.index, r.state.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "active"), (1, "retiring")]
    );
    let done = t.reconcile();
    let [Reconciled::Stopped {
        instance: 1,
        operation_id,
        reason: "retire",
        ..
    }] = done.as_slice()
    else {
        panic!("one retirement stop expected: {done:?}")
    };
    assert_eq!(t.status(&id).observed_state, "ready");
    t.cleaned(&t.cleanup_step(operation_id));
    assert!(matches!(
        t.reconcile().as_slice(),
        [Reconciled::Retired { instance: 1, .. }]
    ));
    assert_eq!(t.store.deployment_instances(&id).unwrap().len(), 1);
    assert_eq!(t.owners(), vec![id.clone()]);
}

/// Owner decision Q8: a revision other than a count change stops every
/// running instance with verified cleanup and restarts it on the new
/// revision. The orchestration is durable: every step is a recorded stop or a
/// pending start that the next pass picks up, whatever restarted in between.
// T08 T33 T10
#[test]
fn a_non_count_revision_stops_and_restarts_every_running_instance() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    all_ready(&t, &id, "start");
    let before = t.store.deployment_instances(&id).unwrap();
    let mut changed = t.config["engine_config"].clone();
    changed["memory"]["kv_cache"] = json!("2GiB");
    t.replace(
        &id,
        "reconfigure",
        1,
        json!({"instances": 2, "engine_config": changed}),
    );
    // Nothing stopped at acceptance; both instances still run revision 1.
    assert_eq!(t.status(&id).observed_state, "ready");
    let done = t.reconcile();
    let stops: Vec<_> = done
        .iter()
        .filter_map(|action| match action {
            Reconciled::Stopped {
                operation_id,
                reason: "revision",
                ..
            } => Some(operation_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(stops.len(), 2, "{done:?}");
    for operation in &stops {
        t.cleaned(&t.cleanup_step(operation));
    }
    assert!(t.owners().is_empty(), "stopped with verified cleanup first");
    // A restarted controller's first pass places both again on revision 2.
    let session = t.store.begin_coordinator_session().unwrap();
    let done = t
        .store
        .reconcile_instances(&session, NOW, None, true)
        .unwrap();
    assert_eq!(
        done.iter()
            .filter(|action| matches!(action, Reconciled::Started { .. }))
            .count(),
        2,
        "{done:?}"
    );
    let after = t.store.deployment_instances(&id).unwrap();
    for (old, new) in before.iter().zip(&after) {
        assert!(new.generation > old.generation);
    }
    let revisions: Vec<i64> = t
        .sql
        .prepare("SELECT revision FROM deployment_instances WHERE deployment_id=?1 ORDER BY instance_index")
        .unwrap()
        .query_map([&id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(revisions, vec![2, 2]);
}

/// Owner decision Q5 (ADR 0013 §9): on demand, the lowest-index instance the
/// operator did not stop is brought up, and the rest only where they fit;
/// with every instance stopped by the operator nothing is activated.
// T15 T10 T18
#[test]
fn on_demand_brings_up_the_first_instance_the_operator_did_not_stop() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    t.store.set_instance_operator_stopped(&id, 0, true).unwrap();
    t.start(&id, "on-demand", StartScope::OnDemand, None);
    assert_eq!(
        t.planned(&id)
            .iter()
            .map(|(_, k, _)| *k)
            .collect::<Vec<_>>(),
        vec![1],
        "instance 0 stays stopped; instance 1 comes up"
    );
    t.store.set_instance_operator_stopped(&id, 1, true).unwrap();
    assert!(t.store.on_demand_instance_stopped(&id).unwrap());
}

/// ADR 0013 §4 (W12): an ineligible host is never a candidate, and
/// `max_per_host` keeps a second instance off a host that already has one; an
/// explicit start that cannot place an instance keeps it pending with its
/// diagnostic until the start's deadline.
// T05 T23 T33
#[test]
fn ineligible_hosts_and_max_per_host_bound_placement() {
    let t = two_hosts("32GiB");
    let id = t
        .deploy("deploy", json!({"instances": 2, "placement": {"hosts": ["spark-a", "spark-b"], "max_per_host": 1}}))
        .deployment_id;
    let only_a: BTreeSet<String> = ["host-a".to_string()].into();
    t.start(&id, "start", StartScope::All, Some(&only_a));
    assert_eq!(
        t.planned(&id)
            .iter()
            .map(|(_, k, h)| (*k, h.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "host-a")]
    );
    let status = t.status(&id);
    assert_eq!(status.instances[1].observed_state, "queued");
    assert_eq!(
        status.instances[1].last_error.as_deref(),
        Some("placement: no_host_fits")
    );
    // Past the start's deadline the pending start ends with its diagnostic.
    let done = t
        .store
        .reconcile_instances(&t.session, DEADLINE, Some(&only_a), true)
        .unwrap();
    assert!(done
        .iter()
        .any(|action| matches!(action, Reconciled::Expired { instance: 1, .. })));
    assert_eq!(t.status(&id).instances[1].observed_state, "stopped");
}

/// Owner decision 2026-09-25 (ADR 0017): a start that places nothing because
/// no allowed host is eligible now (drain-only after version skew, draining,
/// revoked, offline) is refused `HostIneligible`, not `CapacityBlocked`;
/// releasing capacity would not help.
// T05 T23
#[test]
fn a_start_with_no_eligible_host_is_host_ineligible_not_capacity() {
    let t = two_hosts("32GiB");
    let id = t
        .deploy(
            "deploy",
            json!({"instances": 1, "placement": {"hosts": ["spark-a", "spark-b"]}}),
        )
        .deployment_id;
    let none: BTreeSet<String> = BTreeSet::new();
    let refused = t.store.accept_scoped_start_command(
        &t.session,
        "owner",
        &id,
        StartScope::All,
        t.revision(&id),
        "start",
        NOW,
        DEADLINE,
        Some(&none),
    );
    assert!(
        matches!(
            refused,
            Err(mllm_store::lifecycle::LifecycleError::HostIneligible)
        ),
        "{:?}",
        refused.map(|r| r.operation_id().to_owned())
    );
    assert_eq!(
        t.store.resolved_hosts(&id).unwrap(),
        vec!["host-a".to_string(), "host-b".to_string()]
    );
}

/// SPEC §7.3, ADR 0013 §4 step 2: two instances co-reside on one host only
/// when both device claims are shared and the host's managed limit holds
/// both; otherwise the second is refused with its reason, never over budget.
/// The embedded host can run several launches; an enrolled host's agent owns
/// one launch at a time, so a second instance is never placed on it.
// T24 T26 T23
#[test]
fn co_residence_needs_shared_devices_room_and_a_host_that_can_run_both() {
    let embedded = |sharing: &str, managed: &str| {
        let (mut config, mut host) = golden();
        host["resource_policy"]["domains"]["unified"]["managed_limit"] = json!(managed);
        host["resource_policy"]["devices"]["gpu0"]["sharing"] = json!(sharing);
        host["resource_policy"]["device_sharing"] = json!(sharing);
        config["devices"][0]["sharing"] = json!(sharing);
        for phase in ["cold", "ready", "parking", "wake"] {
            config["resources"][phase]["devices"][0]["sharing"] = json!(sharing);
        }
        config["instances"] = json!(2);
        config["placement"] = json!({"strategy": "pack"});
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embedded.sqlite3");
        let store = Store::open(&path).unwrap();
        let session = store.begin_coordinator_session().unwrap();
        let effective = mllm_config::effective::resolve_effective(&config, &host).unwrap();
        store
            .import_resource_policy(&session, &effective.host, &[observed("unified", 1)], 1)
            .unwrap();
        let id = store
            .create_stopped_managed_configuration(
                &session,
                "owner",
                "deploy",
                &json!({ "config": config }).to_string(),
                &host,
                NOW,
            )
            .unwrap()
            .deployment_id;
        store
            .accept_scoped_start_command(
                &session,
                "owner",
                &id,
                StartScope::All,
                1,
                "start",
                NOW,
                DEADLINE,
                None,
            )
            .unwrap();
        let status = store
            .snapshot()
            .unwrap()
            .deployments
            .into_iter()
            .find(|d| d.id == id)
            .unwrap();
        (dir, status)
    };
    let (_dir, both) = embedded("shared", "32GiB");
    assert!(both
        .instances
        .iter()
        .all(|i| i.observed_state == "queued" && i.host_id.as_deref() == Some("lab")));
    let (_dir, tight) = embedded("shared", "15GiB");
    assert_eq!(
        tight.instances[1].last_error.as_deref(),
        Some("placement: insufficient_capacity")
    );
    let (_dir, exclusive) = embedded("exclusive", "32GiB");
    assert_eq!(
        exclusive.instances[1].last_error.as_deref(),
        Some("placement: device_conflict")
    );
    // An enrolled host that already runs one launch takes no second one.
    let t = two_hosts("32GiB");
    let id = t
        .deploy(
            "deploy",
            json!({"instances": 2, "placement": {"hosts": ["spark-a"], "strategy": "pack"}}),
        )
        .deployment_id;
    t.start(&id, "start", StartScope::All, None);
    assert_eq!(t.planned(&id).len(), 1);
    assert_eq!(
        t.status(&id).instances[1].last_error.as_deref(),
        Some("placement: host_occupied")
    );
}

/// SPEC §§3.1, 7.3 (per-launch host claims), owner goal D10: once an enrolled
/// host advertises per-launch journal claims, other deployments co-reside on
/// it beside a running one, bounded by the fit (never over budget), while a
/// second instance of the same deployment is still refused `host_occupied`:
/// the host fences each deployment's commands by its highest generation. A
/// host that stops advertising the capability is single-claim again.
// T24 T26 T27 T23
#[test]
fn per_launch_hosts_take_other_deployments_but_not_a_second_instance() {
    let t = two_hosts("32GiB");
    let per_launch = |on: bool| {
        let sql = if on {
            "INSERT OR REPLACE INTO host_launch_claims VALUES('host-a','per_launch',1)"
        } else {
            "DELETE FROM host_launch_claims WHERE host_id='host-a'"
        };
        t.sql.execute(sql, []).unwrap();
    };
    let only_a = |key: &str, instances: u32| {
        t.deploy(
            key,
            json!({
                "name": key,
                "routes": [key],
                "instances": instances,
                "placement": {"hosts": ["spark-a"], "strategy": "pack"}
            }),
        )
        .deployment_id
    };
    let try_start = |id: &str, key: &str| {
        t.store.accept_scoped_start_command(
            &t.session,
            "owner",
            id,
            StartScope::All,
            t.revision(id),
            key,
            NOW,
            DEADLINE,
            None,
        )
    };
    // Single-claim host: a second deployment is refused beside the first.
    let first = only_a("first", 2);
    t.start(&first, "start-first", StartScope::All, None);
    assert_eq!(t.planned(&first).len(), 1);
    assert_eq!(
        t.status(&first).instances[1].last_error.as_deref(),
        Some("placement: host_occupied")
    );
    let second = only_a("second", 2);
    assert!(matches!(
        try_start(&second, "start-second"),
        Err(mllm_store::lifecycle::LifecycleError::CapacityBlocked)
    ));
    assert!(t.planned(&second).is_empty());

    // Per-launch host: another deployment is placed beside the first; its
    // own second instance is still not co-resident with its first.
    per_launch(true);
    try_start(&second, "start-second-again").unwrap();
    assert_eq!(t.planned(&second).len(), 1, "co-resident beside the first");
    assert_eq!(
        t.status(&second).instances[1].last_error.as_deref(),
        Some("placement: host_occupied")
    );
    // An older agent that stops advertising it takes nothing further, even
    // where the budget would allow it.
    let third = only_a("third", 1);
    per_launch(false);
    assert!(matches!(
        try_start(&third, "start-third"),
        Err(mllm_store::lifecycle::LifecycleError::CapacityBlocked)
    ));
    assert!(t.planned(&third).is_empty());
    // 10 GiB cold each against 32 GiB: the third fits, the fourth does not,
    // and it is the fit, not the claim, that refuses it.
    per_launch(true);
    try_start(&third, "start-third-again").unwrap();
    assert_eq!(t.planned(&third).len(), 1);
    let fourth = only_a("fourth", 1);
    assert!(matches!(
        try_start(&fourth, "start-fourth"),
        Err(mllm_store::lifecycle::LifecycleError::CapacityBlocked)
    ));
    assert!(t.planned(&fourth).is_empty(), "never over budget");
}

/// ADR 0013 §4, §5 (owner decision P1; per-instance host fencing): a host
/// whose journal fences each instance of a deployment on its own takes a
/// second instance of the same deployment beside the first, bounded by the
/// fit alone. Each instance keeps its own generation and resource owner, and
/// both become Ready there. A host that advertises only per-launch claims is
/// still refused the second instance `host_occupied`.
// T24 T26 T27 T34
#[test]
fn per_instance_hosts_take_a_second_instance_of_one_deployment() {
    let t = two_hosts("32GiB");
    let mode = |mode: &str| {
        t.sql
            .execute(
                "INSERT OR REPLACE INTO host_launch_claims VALUES('host-a',?1,1)",
                [mode],
            )
            .unwrap();
    };
    let pair = |key: &str| {
        t.deploy(
            key,
            json!({
                "name": key,
                "routes": [key],
                "instances": 2,
                "placement": {"hosts": ["spark-a"], "strategy": "pack"}
            }),
        )
        .deployment_id
    };
    mode("per_launch");
    let fenced = pair("fenced");
    t.start(&fenced, "start-fenced", StartScope::All, None);
    assert_eq!(t.planned(&fenced).len(), 1);
    assert_eq!(
        t.status(&fenced).instances[1].last_error.as_deref(),
        Some("placement: host_occupied")
    );

    mode("per_instance");
    let id = pair("pair");
    all_ready(&t, &id, "start-pair");
    let status = t.status(&id);
    assert_eq!(status.instances.len(), 2);
    for instance in &status.instances {
        assert_eq!(instance.host_id.as_deref(), Some("host-a"), "{status:?}");
        assert_eq!(instance.observed_state, "ready", "{status:?}");
        assert!(instance.last_error.is_none(), "{status:?}");
    }
    assert_ne!(
        status.instances[0].generation, status.instances[1].generation,
        "each incarnation draws its own generation"
    );
    let owners: BTreeSet<String> = t.owners().into_iter().filter(|o| o.contains(&id)).collect();
    assert_eq!(
        owners,
        BTreeSet::from([id.clone(), format!("deployment:{id}/instance:1")]),
        "each instance is charged under its own owner"
    );
}

/// ADR 0013 §6: the deployment is `failed` only when every instance failed.
// T29 T30
#[test]
fn a_deployment_fails_only_when_every_instance_failed() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    all_ready(&t, &id, "start");
    let fail = |instance: u32| {
        t.sql
            .execute(
                "UPDATE deployment_instances SET observed_state='stopped',dispatch_enabled=0,admission_enabled=0 WHERE deployment_id=?1 AND instance_index=?2",
                params![id, instance],
            )
            .unwrap();
        t.sql
            .execute(
                "UPDATE runtime_bindings SET state='released' WHERE deployment_id=?1 AND instance_index=?2",
                params![id, instance],
            )
            .unwrap();
        t.sql
            .execute(
                "DELETE FROM resource_owners WHERE deployment_id=?1 AND instance_index=?2",
                params![id, instance],
            )
            .unwrap();
    };
    fail(1);
    let status = t.status(&id);
    assert_eq!(status.instances[1].observed_state, "failed");
    assert_eq!(status.observed_state, "ready");
    fail(0);
    assert_eq!(t.status(&id).observed_state, "failed");
}

/// ADR 0013 §10 (unit I3): the router reads every serving instance with its
/// own generation, host, launch and gate, and a lease it opens is charged to
/// exactly the instance it chose. Once that instance's gate closes or its
/// incarnation is gone, a grant for it is refused and its binding is no
/// longer resolvable by that generation: a lease and its forward always name
/// the same incarnation.
// T18 T32 T38
#[test]
fn the_router_reads_each_instance_and_leases_the_one_it_chose() {
    use mllm_store::dispatch::{DispatchError, LeaseWrite, LeaseWriteOutcome};
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    t.start(&id, "start", StartScope::All, None);
    let planned = t.planned(&id);
    for (n, (step, _, host)) in planned.iter().enumerate() {
        t.ready(step, host, 100 + 10 * n as u32);
    }
    let serving = t.store.serving_instances(&id).unwrap();
    assert_eq!(serving.len(), 2);
    for (row, (step, index, host)) in serving.iter().zip(&planned) {
        assert_eq!(row.instance_index, *index);
        assert_eq!(row.host_id.as_deref(), Some(host.as_str()));
        assert_eq!(row.launch_command_id.as_deref(), Some(step.as_str()));
        assert!(row.dispatch_open);
        let (binding, open) = t
            .store
            .serving_binding_at(&id, row.generation)
            .unwrap()
            .unwrap();
        assert_eq!((binding.id.as_str(), open), (row.binding_id.as_str(), true));
    }
    assert_ne!(serving[0].generation, serving[1].generation);
    let grant = |generation: i64| {
        t.store
            .apply_request_lease_batch(
                &t.session,
                &[LeaseWrite::GrantInstance {
                    deployment_id: id.clone(),
                    generation,
                    max_per_deployment: 8,
                    max_total: 8,
                }],
            )
            .unwrap()
            .pop()
            .unwrap()
    };
    // The lease names instance 1, not the lowest-index open instance.
    let second = serving[1].generation;
    let Ok(LeaseWriteOutcome::Granted(ticket)) = grant(second) else {
        panic!("a grant for an open instance");
    };
    assert_eq!(ticket.generation(), second);
    let charged: u32 = t
        .sql
        .query_row(
            "SELECT instance_index FROM request_leases WHERE id=?1",
            [ticket.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(charged, 1);
    // An unknown generation names no incarnation.
    assert!(matches!(grant(second + 100), Err(DispatchError::Conflict)));
    assert!(t
        .store
        .serving_binding_at(&id, second + 100)
        .unwrap()
        .is_none());
    // Close instance 1's gate (its host re-proving readiness): no new lease
    // for it, the other instance still grants, and the router sees the gate.
    t.sql
        .execute(
            "UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id=?1 AND instance_index=1",
            [&id],
        )
        .unwrap();
    assert!(matches!(grant(second), Err(DispatchError::Closed)));
    assert!(matches!(
        grant(serving[0].generation),
        Ok(LeaseWriteOutcome::Granted(_))
    ));
    let serving = t.store.serving_instances(&id).unwrap();
    assert_eq!(
        serving.iter().map(|r| r.dispatch_open).collect::<Vec<_>>(),
        vec![true, false]
    );
    // The outstanding lease on instance 1 is untouched by the closure (T32).
    assert_eq!(t.store.pending_dispatches(&id).unwrap().len(), 2);
}

/// Deploy a one-instance deployment pinned to host-a under `key`.
fn only_a(t: &TwoHosts, key: &str, extra: Value) -> String {
    let mut patch = json!({
        "name": key,
        "routes": [key],
        "placement": {"hosts": ["spark-a"], "strategy": "pack"}
    });
    for (field, value) in extra.as_object().unwrap() {
        patch[field] = value.clone();
    }
    t.deploy(key, patch).deployment_id
}

fn plan(t: &TwoHosts, id: &str) -> mllm_store::ordinary_lifecycle::switching::SwitchPlan {
    t.store
        .plan_switch(
            &t.session,
            id,
            None,
            false,
            None,
            &BTreeSet::new(),
            &|_, _| None,
        )
        .unwrap()
}

fn generation(t: &TwoHosts, id: &str) -> i64 {
    t.sql
        .query_row(
            "SELECT generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

fn dispatch_open(t: &TwoHosts, id: &str) -> bool {
    t.sql
        .query_row(
            "SELECT dispatch_enabled=1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

/// W10 gap (e): an enrolled host whose agent holds one journal claim at a
/// time cannot take a second launch while it runs another, however much
/// memory it has. The switch planner frees it by releasing every launch
/// that occupies it; while an occupant is not an eligible READY victim (it
/// is still starting), the host stays refused `host_occupied`.
// T27 T24 T16
#[test]
fn the_switch_planner_frees_a_single_claim_host_by_releasing_its_occupant() {
    use mllm_store::ordinary_lifecycle::switching::SwitchPlan;
    let t = two_hosts("32GiB");
    let first = only_a(&t, "first", json!({}));
    t.start(&first, "start-first", StartScope::All, None);
    let second = only_a(&t, "second", json!({}));
    // The occupant is still starting: nothing to release, no plan.
    assert_eq!(plan(&t, &second), SwitchPlan::Impossible("host_occupied".into()));
    let (step, _, host) = t.planned(&first).remove(0);
    t.ready(&step, &host, 100);
    // Memory fits both, yet placement refuses: the host is occupied.
    assert!(matches!(
        t.store.accept_scoped_start_command(
            &t.session, "owner", &second, StartScope::All, t.revision(&second),
            "start-second", NOW, DEADLINE, None,
        ),
        Err(mllm_store::lifecycle::LifecycleError::CapacityBlocked)
    ));
    match plan(&t, &second) {
        SwitchPlan::Evict { host, instance, wake, victims, .. } => {
            assert_eq!((host.as_str(), instance, wake), ("host-a", 0, false));
            assert_eq!(victims.len(), 1);
            assert_eq!(victims[0].deployment_id, first);
            assert!(victims[0].last_ready);
        }
        other => panic!("expected an eviction plan, got {other:?}"),
    }
}

/// SPEC §6.5, ADR 0013 amendment 2026-09-23 (W10 gap d): a deployment that
/// declares `lifecycle.warm: true` is never chosen as a switch victim and is
/// never idled; status reports the commitment. The same deployment without
/// it is a victim and is idled.
// T16 T27 T33
#[test]
fn a_warm_residency_commitment_is_never_a_victim_nor_idled() {
    use mllm_store::ordinary_lifecycle::park::IdlePolicy;
    use mllm_store::ordinary_lifecycle::switching::SwitchPlan;
    // 10 GiB cold, 8 GiB Ready each: two never fit in 15 GiB.
    let t = two_hosts("15GiB");
    t.sql
        .execute(
            "INSERT OR REPLACE INTO host_launch_claims VALUES('host-a','per_instance',1)",
            [],
        )
        .unwrap();
    let warm = only_a(&t, "warm", json!({"lifecycle": {"warm": true}}));
    all_ready(&t, &warm, "start-warm");
    assert!(t.status(&warm).warm);
    let waiting = only_a(&t, "waiting", json!({}));
    assert_eq!(
        plan(&t, &waiting),
        SwitchPlan::Impossible("insufficient_capacity".into())
    );
    let idle = IdlePolicy {
        ready_idle_ms: Some(1),
        parked_idle_ms: Some(1),
    };
    let none = |_: &str, _: i64| None;
    assert!(t
        .store
        .apply_idle_policy(&t.session, NOW + 1_000_000, idle, &none, 0)
        .unwrap()
        .is_empty());
    assert!(dispatch_open(&t, &warm), "still serving");

    // Without the commitment the same shape is a victim and is idled.
    let t = two_hosts("15GiB");
    t.sql
        .execute(
            "INSERT OR REPLACE INTO host_launch_claims VALUES('host-a','per_instance',1)",
            [],
        )
        .unwrap();
    let cold = only_a(&t, "cold", json!({}));
    all_ready(&t, &cold, "start-cold");
    assert!(!t.status(&cold).warm);
    let waiting = only_a(&t, "waiting", json!({}));
    assert!(matches!(plan(&t, &waiting), SwitchPlan::Evict { .. }));
    assert_eq!(
        t.store
            .apply_idle_policy(&t.session, NOW + 1_000_000, idle, &none, 0)
            .unwrap()
            .len(),
        1
    );
}

/// W10 gap (a), SPEC §§10, 13.2: a switch reopens only a gate it closed that
/// nothing else has closed since. An engine exit recorded during the drain
/// window keeps it closed; without one the failed switch reopens it; and a
/// gate the switch never closed is never reopened by it.
// T17 T20 T32
#[test]
fn a_failed_switch_reopens_only_its_own_closure() {
    use mllm_store::ordinary_lifecycle::engine_exit::{EngineExit, ExitSource};
    let t = two_hosts("32GiB");
    let id = only_a(&t, "victim", json!({}));
    t.start(&id, "start", StartScope::All, None);
    let (step, _, host) = t.planned(&id).remove(0);
    t.ready(&step, &host, 100);
    let generation = generation(&t, &id);
    // Closed and reopened by the switch alone.
    assert!(t.store.close_for_switch(&t.session, &id, 0, generation).unwrap());
    assert!(!dispatch_open(&t, &id));
    assert!(t.store.reopen_after_switch(&t.session, &id, 0, generation).unwrap());
    assert!(dispatch_open(&t, &id));
    // Closed by the switch, then the engine exits during the drain.
    assert!(t.store.close_for_switch(&t.session, &id, 0, generation).unwrap());
    let exited = t
        .store
        .record_engine_exit(
            &t.session,
            ExitSource::Embedded,
            &EngineExit {
                deployment_id: id.clone(),
                generation,
                step_id: step.clone(),
                process: identity("api", 100),
                status: "signal 9".into(),
                observed_at_ms: NOW,
            },
        )
        .unwrap();
    assert!(exited.is_some());
    assert!(!t.store.reopen_after_switch(&t.session, &id, 0, generation).unwrap());
    assert!(!dispatch_open(&t, &id), "a failed switch reopened an exited engine");
    // A later switch failure for a gate it no longer holds changes nothing.
    assert!(!t.store.reopen_after_switch(&t.session, &id, 0, generation).unwrap());
    assert!(!dispatch_open(&t, &id));
}

/// SPEC §9.2 ("unknown work is not proof of safe quiescence for parking; an
/// uncertain drain must reconcile or fail within its bound"), §10 ("retain
/// conservative accounting until ... controlled cleanup"), ADR 0013 §8 rule 8:
/// a READY victim whose only lease is `uncertain` (its stream was cut, so the
/// router cannot prove the request ended) is not waited on by the switch
/// drain, because waiting can never settle it. It is not parked either: it is
/// released by a Stop, whose cleanup evidence is what settles the lease. The
/// lease stays charged throughout; nothing forgets it. An `inflight` lease
/// still blocks the drain and the release.
// T16 T18 T32
#[test]
fn an_uncertain_lease_neither_blocks_a_switch_drain_nor_lets_the_victim_park() {
    use mllm_store::dispatch::{LeaseWrite, LeaseWriteOutcome};
    use mllm_store::ordinary_lifecycle::switching::SwitchPlan;
    // 10 GiB cold, 8 GiB Ready each: two never fit in 15 GiB.
    let t = two_hosts("15GiB");
    t.sql
        .execute(
            "INSERT OR REPLACE INTO host_launch_claims VALUES('host-a','per_instance',1)",
            [],
        )
        .unwrap();
    let victim = only_a(&t, "victim", json!({}));
    all_ready(&t, &victim, "start-victim");
    let waiting = only_a(&t, "waiting", json!({}));
    let generation = generation(&t, &victim);
    let parks = |t: &TwoHosts| match plan(t, &waiting) {
        SwitchPlan::Evict { victims, .. } => {
            assert_eq!(victims.len(), 1);
            assert_eq!(victims[0].deployment_id, victim);
            victims[0].parks
        }
        other => panic!("expected an eviction plan, got {other:?}"),
    };
    // Without any lease the victim parks at its declared tier.
    assert!(parks(&t));
    let Ok(LeaseWriteOutcome::Granted(ticket)) = t
        .store
        .apply_request_lease_batch(
            &t.session,
            &[LeaseWrite::GrantInstance {
                deployment_id: victim.clone(),
                generation,
                max_per_deployment: 8,
                max_total: 8,
            }],
        )
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a grant for an open instance");
    };
    // An in-flight lease is outstanding: the drain waits for it.
    assert_eq!(
        t.store
            .switch_outstanding_leases(&victim, 0, generation)
            .unwrap(),
        1
    );
    t.sql
        .execute(
            "UPDATE request_leases SET disposition='uncertain' WHERE id=?1",
            [ticket.id()],
        )
        .unwrap();
    // Uncertain: waiting cannot settle it, so the drain does not count it.
    assert_eq!(
        t.store
            .switch_outstanding_leases(&victim, 0, generation)
            .unwrap(),
        0
    );
    // The planner still offers the victim, but it must stop, not park.
    assert!(!parks(&t));
    assert!(t
        .store
        .close_for_switch(&t.session, &victim, 0, generation)
        .unwrap());
    let release = t
        .store
        .accept_switch_release(
            &t.session,
            "switch",
            &victim,
            0,
            generation,
            "switch-release",
            NOW,
            true,
        )
        .unwrap();
    assert!(!release.parked, "unknown work is never parked");
    // The lease keeps its accounting until the Stop's cleanup evidence.
    let charged: i64 = t
        .sql
        .query_row(
            "SELECT COUNT(*) FROM request_leases WHERE id=?1 AND disposition='uncertain'",
            [ticket.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(charged, 1);
}

/// SPEC §10 step 4: an `inflight` lease on a closed victim refuses its
/// release; only a drained victim is released.
// T16 T32
#[test]
fn an_inflight_lease_still_refuses_a_switch_release() {
    use mllm_store::dispatch::{LeaseWrite, LeaseWriteOutcome};
    let t = two_hosts("32GiB");
    let victim = only_a(&t, "victim", json!({}));
    all_ready(&t, &victim, "start-victim");
    let generation = generation(&t, &victim);
    let granted = t
        .store
        .apply_request_lease_batch(
            &t.session,
            &[LeaseWrite::GrantInstance {
                deployment_id: victim.clone(),
                generation,
                max_per_deployment: 8,
                max_total: 8,
            }],
        )
        .unwrap()
        .pop()
        .unwrap();
    assert!(matches!(granted, Ok(LeaseWriteOutcome::Granted(_))));
    assert!(t
        .store
        .close_for_switch(&t.session, &victim, 0, generation)
        .unwrap());
    assert!(matches!(
        t.store.accept_switch_release(
            &t.session, "switch", &victim, 0, generation, "switch-release", NOW, true,
        ),
        Err(mllm_store::lifecycle::LifecycleError::Conflict)
    ));
}

/// Owner decision Q8: the restart of a replaced instance waits on its stop.
/// A verified cleanup that takes longer than the request deadline must not
/// expire the pending start: the window is re-anchored while the stop is in
/// flight, so the instance is placed again once its cleanup is proven.
// T08 T10 T33
#[test]
fn a_slow_revision_stop_does_not_expire_the_restart() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 1})).deployment_id;
    all_ready(&t, &id, "start");
    let mut changed = t.config["engine_config"].clone();
    changed["memory"]["kv_cache"] = json!("2GiB");
    t.replace(
        &id,
        "reconfigure",
        1,
        json!({"instances": 1, "engine_config": changed}),
    );
    let done = t.reconcile();
    let [Reconciled::Stopped {
        operation_id,
        reason: "revision",
        ..
    }] = done.as_slice()
    else {
        panic!("one revision stop expected: {done:?}")
    };
    // The stop is still in flight well past the 600 s request deadline.
    let late = NOW + 700_000;
    let done = t
        .store
        .reconcile_instances(&t.session, late, None, true)
        .unwrap();
    assert!(done.is_empty(), "nothing expires while the stop runs: {done:?}");
    t.cleaned(&t.cleanup_step(operation_id));
    let done = t
        .store
        .reconcile_instances(&t.session, late + 1, None, true)
        .unwrap();
    assert!(
        matches!(done.as_slice(), [Reconciled::Started { instance: 0, .. }]),
        "the restart is placed once the stop is proven: {done:?}"
    );
}

/// SPEC §6.3 (live M47), ADR 0013 §6: an operator's Stop that finds one
/// instance still launching without an association defers only that launch.
/// Every sibling that can be fenced is stopped at once: a Ready sibling must
/// not keep serving until the launch settles. The deferred Stop is carried out
/// on the launching instance once it settles, and the siblings' stops already
/// in flight are not refused for being in flight.
// T10 T16 T32
#[test]
fn a_deferred_stop_stops_ready_siblings_now() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    t.start(&id, "start", StartScope::All, None);
    let planned = t.planned(&id);
    let (ready_step, _, ready_host) = &planned[0];
    t.ready(ready_step, ready_host, 100);
    // Instance 1 arms but its processes are not associated yet.
    let (launch_step, _, launch_host) = &planned[1];
    let (observations, limits, ttl, max_parked) = t.admission(launch_host);
    let (_, context) = t
        .store
        .arm_initialize_with_context(
            &t.session,
            launch_step,
            AdmissionContext::new(&observations, &limits, NOW, ttl, max_parked),
        )
        .unwrap();
    let context = context.unwrap();
    let receipt = t
        .store
        .accept_administrative_stop_command(&t.session, "owner", &id, 1, "stop", NOW, DEADLINE)
        .unwrap();
    let kind: String = t
        .sql
        .query_row(
            "SELECT kind FROM operations WHERE id=?1",
            [receipt.operation_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "administrative_stop_deferred");
    let status = t.status(&id);
    assert_eq!(
        status.instances[0].observed_state, "stopping",
        "the Ready sibling is stopped now"
    );
    assert!(!t.store.dispatch_enabled(&id).unwrap(), "nothing serves");
    let sibling_stop: String = t
        .sql
        .query_row(
            "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              JOIN runtime_bindings b ON b.id=s.binding_id
             WHERE o.kind='ordinary_cleanup' AND b.instance_index=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // The launch settles Ready while the sibling's cleanup is still in flight.
    let group = vec![identity("api", 300), identity("worker-0", 301)];
    t.store
        .record_owned_launch(
            &t.session,
            launch_step,
            &OwnedLaunchReceipt {
                binding_id: context.binding_id.clone(),
                incarnation: context.incarnation.clone(),
                identities: group.clone(),
                observed_at_ms: NOW,
                receipt: "scripted host native model probe".into(),
            },
            NOW,
        )
        .unwrap();
    t.store
        .complete_step(
            &t.session,
            launch_step,
            &CompletionEvidence {
                token: context.token,
                identities: group,
                observed_at_ms: NOW,
                control_receipt: Some("scripted host native model probe".into()),
                milestones: vec![
                    Milestone::AllocationsRestored,
                    Milestone::WeightsUsable,
                    Milestone::CacheValid,
                    Milestone::ModelUsable,
                ],
            },
            NOW,
            ttl,
        )
        .unwrap();
    let closed = t.store.resolve_deferred_stops(&t.session, NOW).unwrap();
    assert_eq!(closed, vec![receipt.operation_id().to_string()]);
    let state: String = t
        .sql
        .query_row(
            "SELECT state FROM operations WHERE id=?1",
            [receipt.operation_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "succeeded", "the deferred Stop is carried out");
    let launch_stop: String = t
        .sql
        .query_row(
            "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              JOIN runtime_bindings b ON b.id=s.binding_id
             WHERE o.kind='ordinary_cleanup' AND b.instance_index=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    t.cleaned(&sibling_stop);
    t.cleaned(&launch_stop);
    assert!(t.owners().is_empty(), "both stopped with verified cleanup");
}

fn switch_closures(t: &TwoHosts, id: &str) -> i64 {
    t.sql
        .query_row(
            "SELECT COUNT(*) FROM dispatch_closures WHERE deployment_id=?1 AND reason='switch'",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

/// W10 gap (a), SPEC §10: a switch that ends (terminal record, or its caller
/// gave up) without reopening a victim it closed leaves no orphan closure. The
/// switch's own reason is removed and the gate reopens under the same rule a
/// failed switch's reopen uses: only when no other reason remains. A retired
/// coordinator session changes nothing.
// T17 T20 T33
#[test]
fn a_switch_that_ends_without_reopening_clears_its_closures() {
    use mllm_store::events::SwitchPhase;
    use mllm_store::ordinary_lifecycle::switching::{SwitchRecord, SwitchVictim};
    let t = two_hosts("32GiB");
    let id = only_a(&t, "victim", json!({}));
    t.start(&id, "start", StartScope::All, None);
    let (step, _, host) = t.planned(&id).remove(0);
    t.ready(&step, &host, 100);
    let generation = generation(&t, &id);
    let victims = [SwitchVictim {
        deployment_id: id.clone(),
        instance: 0,
        generation,
        parks: false,
        last_ready: true,
        serves_elsewhere: false,
    }];
    let record = |phase| SwitchRecord {
        phase,
        switch_id: "switch-1",
        target: "target",
        host: Some("host-a"),
        victims: &victims,
        detail: "",
        explicit: false,
    };
    // A terminal record without a reopen.
    t.store
        .record_switch(&t.session, &record(SwitchPhase::AdmissionClosed))
        .unwrap();
    assert!(t.store.close_for_switch(&t.session, &id, 0, generation).unwrap());
    t.store
        .record_switch(&t.session, &record(SwitchPhase::Failed))
        .unwrap();
    assert_eq!(switch_closures(&t, &id), 0, "no orphan closure");
    assert!(dispatch_open(&t, &id), "the victim serves again");

    // A caller that gave up: `end_switch` settles the listed victims.
    t.store
        .record_switch(&t.session, &record(SwitchPhase::AdmissionClosed))
        .unwrap();
    assert!(t.store.close_for_switch(&t.session, &id, 0, generation).unwrap());
    // A retired session cannot end it.
    let stale = t.session.clone();
    let session = t.store.begin_coordinator_session().unwrap();
    assert!(t.store.end_switch(&stale, "switch-1").is_err());
    // (The new session already dropped every switch reason; record one again.)
    t.store
        .record_switch(&session, &record(SwitchPhase::AdmissionClosed))
        .unwrap();
    t.sql
        .execute(
            "INSERT INTO dispatch_closures(deployment_id,instance_index,generation,reason) VALUES(?1,0,?2,'switch')",
            params![id, generation],
        )
        .unwrap();
    t.sql
        .execute(
            "INSERT INTO dispatch_closures(deployment_id,instance_index,generation,reason) VALUES(?1,0,?2,'host_session')",
            params![id, generation],
        )
        .unwrap();
    t.store.end_switch(&session, "switch-1").unwrap();
    assert_eq!(switch_closures(&t, &id), 0, "no orphan closure");
    assert!(
        !dispatch_open(&t, &id),
        "a host-session closure keeps the gate closed"
    );
}

/// Q5 (ADR 0013 §9): on demand, instances past the first only fill in where
/// they fit, and one whose start is then refused is skipped. What placing it
/// wrote (its host, devices and a freshly drawn generation) must be undone
/// with the refused start, or the command commits a placement no start
/// followed.
// T15 T16 T18
#[test]
fn a_refused_filling_start_leaves_its_instance_unplaced() {
    let t = two_hosts("32GiB");
    let id = t.deploy("deploy", json!({"instances": 2})).deployment_id;
    let before = t.store.deployment_instances(&id).unwrap();
    // Refuse any start of instance 1 at the point its binding is written.
    t.sql
        .execute_batch(
            "CREATE TRIGGER refuse_instance_one BEFORE INSERT ON runtime_bindings
             WHEN NEW.instance_index=1 BEGIN SELECT RAISE(ABORT,'refused'); END;",
        )
        .unwrap();
    t.start(&id, "on-demand", StartScope::OnDemand, None);
    assert_eq!(
        t.planned(&id)
            .iter()
            .map(|(_, k, _)| *k)
            .collect::<Vec<_>>(),
        vec![0]
    );
    let after = t.store.deployment_instances(&id).unwrap();
    assert_eq!(after[1], before[1], "instance 1 stays as it was");
}

/// W10 gap (a): closure reasons are per instance incarnation, so they must not
/// accumulate. A stop's fence move retires every reason recorded for the
/// incarnation it replaced, and the verified cleanup retires any left for the
/// stopped instance: a later start that keeps the stop's generation on the
/// same host must not find its gate held closed by a dead reason.
// T17 T32 T34
#[test]
fn closure_reasons_are_pruned_on_fence_move_and_cleanup() {
    use mllm_store::ordinary_lifecycle::engine_exit::{EngineExit, ExitSource};
    let t = two_hosts("32GiB");
    let id = only_a(&t, "victim", json!({}));
    t.start(&id, "start", StartScope::All, None);
    let (step, _, host) = t.planned(&id).remove(0);
    t.ready(&step, &host, 100);
    let exited = generation(&t, &id);
    let closures = || -> i64 {
        t.sql
            .query_row(
                "SELECT COUNT(*) FROM dispatch_closures WHERE deployment_id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap()
    };
    t.store
        .record_engine_exit(
            &t.session,
            ExitSource::Embedded,
            &EngineExit {
                deployment_id: id.clone(),
                generation: exited,
                step_id: step.clone(),
                process: identity("api", 100),
                status: "signal 9".into(),
                observed_at_ms: NOW,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(closures(), 1);
    let stop = t
        .store
        .accept_instance_stop_command(&t.session, "owner", &id, 0, 1, "stop", NOW, DEADLINE)
        .unwrap()
        .unwrap();
    assert_eq!(closures(), 0, "the fence move retires the old incarnation's reasons");
    // A reason recorded against the stop's own generation (the one a restart
    // on this host keeps) is retired by the verified cleanup.
    let stopped = generation(&t, &id);
    t.sql
        .execute(
            "INSERT INTO dispatch_closures(deployment_id,instance_index,generation,reason) VALUES(?1,0,?2,'host_session')",
            params![id, stopped],
        )
        .unwrap();
    t.cleaned(&t.cleanup_step(stop.operation_id()));
    assert_eq!(closures(), 0, "the verified cleanup retires what is left");
}
