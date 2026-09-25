//! ADR 0019 (discrete GPU design §7, owner decision 3): on a multi-GPU host
//! mllm picks the GPU. A deployment that pins no device is resolved once per
//! GPU; placement puts each instance on the GPU with room, the launch runs
//! the revision as resolved on that GPU, and a stopped instance keeps its
//! last GPU as a preference (ADR 0013 §4).
//!
//! CPU-only with Fake devices: every launch is recorded from scripted
//! evidence; no engine runs. Passing here never qualifies a native engine
//! recipe on a discrete GPU (SPEC §18).
use mllm_domain::completion::{
    CleanupEvidence, CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity,
};
use mllm_domain::resources::{MemoryLimit, MemoryObservation};
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::dispatch::CoordinatorSession;
use mllm_store::ordinary_lifecycle::placement::StartScope;
use mllm_store::Store;
use rusqlite::Connection;
use serde_json::{json, Value};

const NOW: i64 = 10_000;
const DEADLINE: i64 = 200_000;
const GIB: i64 = 1 << 30;

struct Gpus {
    _dir: tempfile::TempDir,
    store: Store,
    sql: Connection,
    session: CoordinatorSession,
    config: Value,
    host: Value,
}

/// The embedded discrete host: host RAM in `system`, and two GPUs of 22 and
/// 30 GiB managed, each its own device-memory domain.
fn two_gpus() -> Gpus {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gpus.sqlite3");
    let store = Store::open(&path).unwrap();
    let sql = Connection::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let (mut config, mut host) = (
        golden["input"]["deployment"].clone(),
        golden["input"]["host"].clone(),
    );
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "22GiB",
                 "free_reserve": "1GiB", "parked_limit": "2GiB"},
        "gpu1": {"memory": "device", "device": "gpu1", "managed_limit": "30GiB",
                 "free_reserve": "1GiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({
        "gpu0": {"domain": "gpu0", "sharing": "shared",
                 "physical_gpu_uuid": "GPU-00000000-0000-0000-0000-000000000000"},
        "gpu1": {"domain": "gpu1", "sharing": "shared",
                 "physical_gpu_uuid": "GPU-11111111-1111-1111-1111-111111111111"}
    });
    let policy = mllm_config::effective::normalize_host_policy(&host).unwrap();
    let observed: Vec<_> = ["system", "gpu0", "gpu1"]
        .iter()
        .map(|d| observation(d, 1))
        .collect();
    store
        .import_resource_policy(&session, &policy, &observed, 1)
        .unwrap();
    // The picker chooses the GPU: no device is named and no resources are
    // declared; the budget is derived from the 12 GiB request.
    let object = config.as_object_mut().unwrap();
    object.remove("resources");
    object.remove("host");
    config["devices"] = json!([]);
    config["engine_config"] = json!({"memory": {"request": "12GiB", "kv_cache": "4GiB"}});
    config["residency"] = json!("restart_only");
    config["instances"] = json!(2);
    Gpus {
        _dir: dir,
        store,
        sql,
        session,
        config,
        host,
    }
}

fn observation(domain: &str, at: i64) -> MemoryObservation {
    MemoryObservation {
        domain: domain.into(),
        capacity_bytes: 1 << 40,
        available_bytes: 1 << 40,
        sampled_at_ms: at,
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

impl Gpus {
    fn deploy(&self, patch: Value) -> String {
        self.deploy_as("deploy", patch)
    }

    fn deploy_as(&self, name: &str, patch: Value) -> String {
        let mut config = self.config.clone();
        config["name"] = json!(name);
        config["routes"] = json!([name]);
        for (field, value) in patch.as_object().unwrap() {
            config[field] = value.clone();
        }
        self.store
            .create_stopped_managed_configuration(
                &self.session,
                "owner",
                name,
                &json!({ "config": config }).to_string(),
                &self.host,
                NOW,
            )
            .unwrap()
            .deployment_id
    }

    fn start(&self, id: &str, key: &str, scope: StartScope) {
        self.store
            .accept_scoped_start_command(
                &self.session,
                "owner",
                id,
                scope,
                self.store.current_revision(id).unwrap().unwrap(),
                key,
                NOW,
                DEADLINE,
                None,
            )
            .unwrap();
    }

    /// Every planned start: (step, instance, the device its frozen launch
    /// selects).
    fn planned(&self, id: &str) -> Vec<(String, u32, String)> {
        self.sql
            .prepare(
                "SELECT s.id,b.instance_index,json_extract(json_extract(s.step_json,'$.effective_json'),'$.selected_devices[0].id')
                   FROM lifecycle_steps s
                   JOIN operations o ON o.id=s.operation_id AND o.kind='initialize'
                   JOIN runtime_bindings b ON b.id=s.binding_id
                  WHERE s.deployment_id=?1 AND s.state='planned' ORDER BY b.instance_index",
            )
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The device each instance row records, by index.
    fn devices(&self, id: &str) -> Vec<Option<String>> {
        self.sql
            .prepare("SELECT device FROM deployment_instances WHERE deployment_id=?1 ORDER BY instance_index")
            .unwrap()
            .query_map([id], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn admission(&self) -> (Vec<MemoryObservation>, Vec<MemoryLimit>, i64, usize) {
        let policy = self.store.resource_policy("lab").unwrap().unwrap();
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
        let observations = ["system", "gpu0", "gpu1"]
            .iter()
            .map(|d| observation(d, NOW))
            .collect();
        (
            observations,
            limits,
            policy.controls.observation_ttl_ms,
            policy.controls.max_parked as usize,
        )
    }

    /// Arm, associate and complete one planned start on scripted evidence.
    fn ready(&self, step: &str, pid: u32) {
        let (observations, limits, ttl, max_parked) = self.admission();
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

    /// Stop one instance and complete the stop on scripted gone evidence.
    fn stop(&self, id: &str, instance: u32, key: &str) {
        let revision = self.store.current_revision(id).unwrap().unwrap();
        let stop = self
            .store
            .accept_instance_stop_command(
                &self.session,
                "owner",
                id,
                instance,
                revision,
                key,
                NOW,
                DEADLINE,
            )
            .unwrap()
            .unwrap();
        let step: String = self
            .sql
            .query_row(
                "SELECT id FROM lifecycle_steps WHERE operation_id=?1",
                [stop.operation_id()],
                |r| r.get(0),
            )
            .unwrap();
        let (_, context) = self
            .store
            .arm_ordinary_cleanup_with_context(&self.session, &step, NOW)
            .unwrap();
        let context = context.unwrap();
        self.store
            .complete_cleanup(
                &self.session,
                &step,
                &CleanupEvidence {
                    binding_id: context.binding_id,
                    incarnation: context.incarnation,
                    identities: context.identities,
                    observed_at_ms: NOW,
                    receipt: "scripted host observed the owned group gone".into(),
                },
                NOW,
                self.admission().2,
            )
            .unwrap();
    }

    /// Bytes charged on `domain` of the host, by owner.
    fn charged(&self, domain: &str) -> i64 {
        self.store
            .resource_snapshot()
            .unwrap()
            .owners
            .values()
            .flat_map(|f| f.allocations.iter())
            .filter(|a| a.domain == domain)
            .map(|a| a.bytes)
            .sum()
    }
}

// Discrete GPU design §7: the deployment is resolved once per GPU, and the
// host's own resolution is the lowest-index GPU's.
// T27 T03
#[test]
fn an_unpinned_deployment_is_resolved_on_every_gpu() {
    let t = two_gpus();
    let id = t.deploy(json!({}));
    let rows: Vec<(String, String)> = t
        .sql
        .prepare(
            "SELECT device,json_extract(effective_json,'$.selected_devices[0].id') FROM host_device_effective_revisions
              WHERE deployment_id=?1 ORDER BY device",
        )
        .unwrap()
        .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("gpu0".into(), "gpu0".into()),
            ("gpu1".into(), "gpu1".into())
        ]
    );
    let host: String = t
        .sql
        .query_row(
            "SELECT json_extract(effective_json,'$.selected_devices[0].id') FROM host_effective_revisions WHERE deployment_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(host, "gpu0");
    // A pinned deployment is resolved on its GPU only: no GPU choice.
    let pinned = t.deploy_as(
        "pinned",
        json!({"devices": [{"id": "gpu1", "sharing": "shared"}], "instances": 1}),
    );
    let choices: i64 = t
        .sql
        .query_row(
            "SELECT COUNT(*) FROM host_device_effective_revisions WHERE deployment_id=?1",
            [&pinned],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(choices, 0);
    t.start(&pinned, "start-pinned", StartScope::All);
    let planned = t.planned(&pinned);
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].2, "gpu1");
    assert_eq!(t.devices(&pinned), vec![None]);
}

// T27: two 12 GiB instances on 22 and 30 GiB cards: the first lands on the
// card with more room (gpu1), the second on gpu0 where room is left; each
// frozen launch selects its GPU, is admitted and charged there, and a
// stopped instance prefers its last GPU when it starts again.
// T27 T16 T05
#[test]
fn instances_land_on_the_gpu_with_room_and_keep_it() {
    let t = two_gpus();
    let id = t.deploy(json!({}));
    t.start(&id, "start", StartScope::All);
    let planned = t.planned(&id);
    assert_eq!(
        planned
            .iter()
            .map(|(_, k, device)| (*k, device.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "gpu1"), (1, "gpu0")],
    );
    assert_eq!(
        t.devices(&id),
        vec![Some("gpu1".to_string()), Some("gpu0".to_string())]
    );
    for (n, (step, _, _)) in planned.iter().enumerate() {
        t.ready(step, 100 + 10 * n as u32);
    }
    assert_eq!(t.charged("gpu1"), 12 * GIB, "instance 0 is charged on gpu1");
    assert_eq!(t.charged("gpu0"), 12 * GIB, "instance 1 is charged on gpu0");

    // Both stop; instance 1 starts alone. Both cards are empty, and gpu1 has
    // more room, but the instance prefers the GPU it last ran on.
    t.stop(&id, 0, "stop-0");
    t.stop(&id, 1, "stop-1");
    assert_eq!((t.charged("gpu0"), t.charged("gpu1")), (0, 0));
    t.start(&id, "start-1", StartScope::Instance(1));
    let planned = t.planned(&id);
    assert_eq!(planned.len(), 1);
    assert_eq!((planned[0].1, planned[0].2.as_str()), (1, "gpu0"));
    t.ready(&planned[0].0, 300);
    assert_eq!((t.charged("gpu0"), t.charged("gpu1")), (12 * GIB, 0));
}

// Discrete GPU design §7 (W10 per GPU): with both cards full, the switch
// planner releases instances on one GPU only, the GPU whose release is
// smallest and then least recently used; an instance on the other card is
// never released for it.
// T27 T16
#[test]
fn eviction_frees_one_gpu_by_least_recent_use() {
    let t = two_gpus();
    let one = |name: &str, request: &str| {
        t.deploy_as(
            name,
            json!({"instances": 1,
                   "engine_config": {"memory": {"request": request, "kv_cache": "4GiB"}}}),
        )
    };
    let a = one("a", "12GiB");
    t.start(&a, "start-a", StartScope::All);
    let b = one("b", "12GiB");
    t.start(&b, "start-b", StartScope::All);
    for (n, id) in [&a, &b].into_iter().enumerate() {
        let planned = t.planned(id);
        t.ready(&planned[0].0, 100 + 10 * n as u32);
    }
    assert_eq!(t.devices(&a), vec![Some("gpu1".to_string())]);
    assert_eq!(t.devices(&b), vec![Some("gpu0".to_string())]);
    // 20 GiB fits either card only once its one instance is released.
    let c = one("c", "20GiB");
    let victims = |a_used: i64, b_used: i64| {
        let (a, b) = (a.clone(), b.clone());
        let activity = move |deployment: &str, _: i64| {
            Some(if deployment == a {
                a_used
            } else if deployment == b {
                b_used
            } else {
                0
            })
        };
        match t
            .store
            .plan_switch(
                &t.session,
                &c,
                None,
                false,
                None,
                &std::collections::BTreeSet::new(),
                &activity,
            )
            .unwrap()
        {
            mllm_store::ordinary_lifecycle::switching::SwitchPlan::Evict { victims, .. } => victims
                .into_iter()
                .map(|v| v.deployment_id)
                .collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        victims(5, 1),
        vec![b.clone()],
        "gpu0's instance was used last longest ago"
    );
    assert_eq!(
        victims(1, 5),
        vec![a.clone()],
        "gpu1's instance was used last longest ago"
    );
}
