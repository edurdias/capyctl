//! ADR 0035 §§1a, 2, 3: one model on N identical GPUs of one host, placed by
//! the embedded (standalone) host's placement: the GPU set with the largest
//! smallest headroom, the last set while it fits, persistence of the chosen
//! set and N, `topology: auto` choosing N from the cards' managed limits,
//! measured startup peaks kept per N, and status.
//!
//! CPU-only with Fake devices: every launch is recorded from scripted
//! evidence; no engine runs. Passing here never qualifies a multi-GPU launch
//! (live rows MG1-MG14 do).
use capyctl_domain::completion::{
    CleanupEvidence, CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity,
};
use capyctl_domain::resources::{MemoryLimit, MemoryObservation};
use capyctl_scheduler::residency::AdmissionContext;
use capyctl_store::dispatch::CoordinatorSession;
use capyctl_store::ordinary_lifecycle::placement::StartScope;
use capyctl_store::Store;
use rusqlite::Connection;
use serde_json::{json, Value};

const NOW: i64 = 10_000;
const DEADLINE: i64 = 200_000;
const GIB: i64 = 1 << 30;

struct Lab {
    _dir: tempfile::TempDir,
    store: Store,
    sql: Connection,
    session: CoordinatorSession,
    config: Value,
    host: Value,
    gpus: Vec<String>,
}

/// The embedded discrete host: host RAM in `system` and `count` identical
/// GPUs (same product, same total), each managing `managed`.
fn lab(count: usize, managed: &str) -> Lab {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mg.sqlite3");
    let store = Store::open(&path).unwrap();
    let sql = Connection::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let (mut config, mut host) = (
        golden["input"]["deployment"].clone(),
        golden["input"]["host"].clone(),
    );
    let gpus: Vec<String> = (0..count).map(|n| format!("gpu{n}")).collect();
    let mut domains = json!({
        "system": {"memory": "distinct", "managed_limit": "200GiB", "free_reserve": "8GiB",
                   "parked_limit": "64GiB", "host_kv_limit": "16GiB"}
    });
    let mut devices = json!({});
    for (n, gpu) in gpus.iter().enumerate() {
        domains[gpu] = json!({"memory": "device", "device": gpu, "managed_limit": managed,
                              "free_reserve": "1GiB", "parked_limit": "2GiB"});
        devices[gpu] = json!({"domain": gpu, "sharing": "shared", "product_name": "NVIDIA RTX A6000",
                              "physical_gpu_uuid": format!("GPU-{n:08}-0000-0000-0000-000000000000")});
    }
    host["resource_policy"]["domains"] = domains;
    host["resource_policy"]["devices"] = devices;
    let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
    let observed: Vec<_> = std::iter::once("system".to_string())
        .chain(gpus.iter().cloned())
        .map(|d| observation(&d, 1))
        .collect();
    store
        .import_resource_policy(&session, &policy, &observed, 1)
        .unwrap();
    let object = config.as_object_mut().unwrap();
    object.remove("resources");
    object.remove("host");
    config["devices"] = json!([]);
    config["engine_config"] = json!({"memory": {"request": "12GiB", "kv_cache": "4GiB"}});
    config["residency"] = json!("restart_only");
    config["instances"] = json!(1);
    Lab {
        _dir: dir,
        store,
        sql,
        session,
        config,
        host,
        gpus,
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

/// One instance row's placement: (device, device_set, tensor_parallel).
type Placed = (Option<String>, Option<String>, Option<u32>);

impl Lab {
    fn deploy(&self, name: &str, patch: Value) -> String {
        let mut config = self.config.clone();
        config["name"] = json!(name);
        config["routes"] = json!([name]);
        for (field, value) in patch.as_object().unwrap() {
            if value.is_null() {
                config.as_object_mut().unwrap().remove(field);
            } else {
                config[field] = value.clone();
            }
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

    fn start(&self, id: &str, key: &str, scope: StartScope) -> Result<(), String> {
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
            .map(drop)
            .map_err(|e| format!("{e:?}"))
    }

    /// Every planned start: (step, instance, the devices its frozen launch
    /// selects, rank order).
    fn planned(&self, id: &str) -> Vec<(String, u32, Vec<String>)> {
        let rows: Vec<(String, u32, String)> = self
            .sql
            .prepare(
                "SELECT s.id,b.instance_index,json_extract(json_extract(s.step_json,'$.effective_json'),'$.selected_devices')
                   FROM lifecycle_steps s
                   JOIN operations o ON o.id=s.operation_id AND o.kind='initialize'
                   JOIN runtime_bindings b ON b.id=s.binding_id
                  WHERE s.deployment_id=?1 AND s.state='planned' ORDER BY b.instance_index",
            )
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows.into_iter()
            .map(|(step, k, devices)| {
                let devices: Value = serde_json::from_str(&devices).unwrap();
                let ids = devices
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|d| d["id"].as_str().unwrap().to_owned())
                    .collect();
                (step, k, ids)
            })
            .collect()
    }

    fn placed(&self, id: &str) -> Vec<Placed> {
        self.sql
            .prepare("SELECT device,device_set,tensor_parallel FROM deployment_instances WHERE deployment_id=?1 ORDER BY instance_index")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
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
                reserve_absorbs_unmanaged: d.memory
                    == capyctl_config::effective::DomainMemory::Device,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect();
        let observations = std::iter::once("system".to_string())
            .chain(self.gpus.iter().cloned())
            .map(|d| observation(&d, NOW))
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

    /// Bytes charged on `domain`, by every owner.
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

    /// Record the checkpoint's measured weights, which re-resolves a
    /// revision accepted provisionally (ADR 0014 §7).
    fn measure(&self, id: &str, weights: i64) {
        let revision = self.store.current_revision(id).unwrap().unwrap();
        let outcome = self
            .store
            .record_checkpoint_digest(
                &self.session,
                id,
                revision,
                "lab",
                &format!("sha256:{}", "a".repeat(64)),
                weights,
                NOW,
            )
            .unwrap();
        assert!(
            matches!(
                outcome,
                capyctl_store::checkpoint_digests::RecordOutcome::Recorded { .. }
            ),
            "{outcome:?}"
        );
    }

    /// The candidate table recorded on the host's own row: (N, per-GPU peak).
    fn table(&self, id: &str) -> Vec<(u32, i64)> {
        let raw: String = self
            .sql
            .query_row(
                "SELECT effective_json FROM host_effective_revisions WHERE deployment_id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        let e: Value = serde_json::from_str(&raw).unwrap();
        e["topology"]["auto"]["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let peak = ["cold", "ready", "parking", "parked", "wake"]
                    .iter()
                    .map(|p| row["device"][*p].as_i64().unwrap())
                    .max()
                    .unwrap();
                (row["tensor_parallel"].as_u64().unwrap() as u32, peak)
            })
            .collect()
    }

    fn status(&self, id: &str) -> Value {
        let snapshot = self.store.snapshot().unwrap();
        let d = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
        serde_json::to_value(d).unwrap()
    }
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

// T46 (ADR 0035 §§2, 5): two instances of a `tensor_parallel: 2` deployment
// on four identical GPUs: the first takes the lowest pair, the second the
// pair whose smallest headroom is largest; each records its set in rank
// order with N beside rank 0's GPU, is charged on both of its GPUs, and
// status shows its size, GPUs and loopback collectives. A stopped instance
// takes its last set again.
#[test]
fn instances_take_gpu_sets_and_keep_them() {
    let t = lab(4, "24GiB");
    let id = t.deploy(
        "tp2",
        json!({"instances": 2, "topology": {"tensor_parallel": 2}}),
    );
    t.start(&id, "start", StartScope::All).unwrap();
    let planned = t.planned(&id);
    assert_eq!(
        planned
            .iter()
            .map(|(_, k, devices)| (*k, devices.clone()))
            .collect::<Vec<_>>(),
        vec![
            (0, strings(&["gpu0", "gpu1"])),
            (1, strings(&["gpu2", "gpu3"]))
        ]
    );
    assert_eq!(
        t.placed(&id),
        vec![
            (Some("gpu0".into()), Some("gpu0,gpu1".into()), Some(2)),
            (Some("gpu2".into()), Some("gpu2,gpu3".into()), Some(2)),
        ]
    );
    for (n, (step, _, _)) in planned.iter().enumerate() {
        t.ready(step, 100 + 10 * n as u32);
    }
    let per_gpu = t.charged("gpu0");
    assert!(per_gpu > 0);
    for gpu in ["gpu1", "gpu2", "gpu3"] {
        assert_eq!(t.charged(gpu), per_gpu, "each GPU is charged one rank");
    }
    let status = t.status(&id);
    let instance = &status["instances"][0];
    assert_eq!(
        instance["topology"],
        json!({"requested": 2, "tensor_parallel": 2, "devices": ["gpu0", "gpu1"]}),
        "{status}"
    );
    assert_eq!(instance["collectives"], "loopback");
    assert_eq!(status["topology"]["tensor_parallel"], 2, "{status}");

    t.stop(&id, 0, "stop-0");
    assert_eq!((t.charged("gpu0"), t.charged("gpu1")), (0, 0));
    t.start(&id, "start-0", StartScope::Instance(0)).unwrap();
    let planned = t.planned(&id);
    assert_eq!(planned[0].2, strings(&["gpu0", "gpu1"]), "its last set");
}

// T46 (ADR 0035 §5): a deployment on one GPU keeps its status JSON: no
// topology and no collectives.
#[test]
fn one_gpu_status_is_unchanged() {
    let t = lab(2, "24GiB");
    let id = t.deploy("one", json!({}));
    t.start(&id, "start", StartScope::All).unwrap();
    assert_eq!(t.placed(&id), vec![(Some("gpu0".into()), None, Some(1))]);
    let status = t.status(&id);
    assert!(status["instances"][0].get("topology").is_none(), "{status}");
    assert!(status["instances"][0].get("collectives").is_none());
    assert!(status.get("topology").is_none());
}

// T46 (ADR 0035 §1a): `topology: auto` records its candidate table on the
// host's row and, once the weights are measured, places at the smallest N
// the cards' managed limits hold, recording N with the instance.
#[test]
fn auto_places_at_the_size_the_cards_hold() {
    let t = lab(2, "40GiB");
    let id = t.deploy(
        "auto",
        json!({"topology": "auto", "engine_config": {"memory": {"kv_cache": "4GiB"}}}),
    );
    t.measure(&id, 20 * GIB);
    let table = t.table(&id);
    assert_eq!(table.iter().map(|(n, _)| *n).collect::<Vec<_>>(), [1, 2]);
    let limit = 40 * GIB;
    let chosen = table
        .iter()
        .find(|(_, peak)| *peak <= limit)
        .map(|(n, _)| *n);
    let Some(chosen) = chosen else {
        assert!(t.start(&id, "start", StartScope::All).is_err());
        return;
    };
    t.start(&id, "start", StartScope::All).unwrap();
    assert_eq!(t.placed(&id)[0].2, Some(chosen));
    let status = t.status(&id);
    assert_eq!(
        status["instances"][0]["topology"]["requested"], "auto",
        "{status}"
    );
    assert_eq!(status["topology"]["chosen"], chosen, "{status}");
}
