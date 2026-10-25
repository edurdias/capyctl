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
    StepExecutionContext,
};
use capyctl_domain::resources::{MemoryLimit, MemoryObservation};
use capyctl_scheduler::residency::AdmissionContext;
use capyctl_store::dispatch::CoordinatorSession;
use capyctl_store::lifecycle::LifecycleError;
use capyctl_store::managed_configuration::ManagedConfigurationError;
use capyctl_store::ordinary_lifecycle::park::WakeScope;
use capyctl_store::ordinary_lifecycle::placement::StartScope;
use capyctl_store::ordinary_lifecycle::switching::SwitchPlan;
use capyctl_store::Store;
use rusqlite::Connection;
use serde_json::{json, Value};

const NOW: i64 = 10_000;
const DEADLINE: i64 = 200_000;
const GIB: i64 = 1 << 30;
/// ADR 0019 §3: each GPU also carries the engine's CUDA context and graphs.
const CONTEXT: i64 = capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
/// What [`Lab::busy`] charges on its GPU: its 5 GiB request and the context.
const BUSY: i64 = 5 * GIB + CONTEXT;

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

    /// Arm one planned start: from here its cold footprint is charged.
    fn arm(&self, step: &str) -> StepExecutionContext {
        let (observations, limits, ttl, max_parked) = self.admission();
        let (_, context) = self
            .store
            .arm_initialize_with_context(
                &self.session,
                step,
                AdmissionContext::new(&observations, &limits, NOW, ttl, max_parked),
            )
            .unwrap();
        context.unwrap()
    }

    /// Arm, associate and complete one planned start on scripted evidence.
    fn ready(&self, step: &str, pid: u32) {
        let context = self.arm(step);
        self.complete(step, context, pid);
    }

    /// Associate and complete an armed start on scripted evidence.
    fn complete(&self, step: &str, context: StepExecutionContext, pid: u32) {
        let ttl = self.admission().2;
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
            "{outcome:?}: {}",
            self.status(id)["checkpoint_digest"]
        );
    }

    /// Record measured weights the revision no longer resolves with; the
    /// diagnostic status shows for the checkpoint (ADR 0014 §7).
    fn unusable(&self, id: &str, weights: i64) -> String {
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
            !matches!(
                outcome,
                capyctl_store::checkpoint_digests::RecordOutcome::Recorded { .. }
            ),
            "{outcome:?}"
        );
        let status = self.status(id);
        status["checkpoint_digest"]["diagnostic"]
            .as_str()
            .unwrap_or_else(|| panic!("{status}"))
            .to_owned()
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

    /// A `topology: auto` deployment with a 4 GiB KV cache whose checkpoint
    /// measured `weights`.
    fn auto(&self, name: &str, weights: i64) -> String {
        let id = self.deploy(
            name,
            json!({"topology": "auto", "engine_config": {"memory": {"kv_cache": "4GiB"}}}),
        );
        self.measure(&id, weights);
        id
    }

    /// Another owner on `gpu`: a one-GPU deployment pinned there, Ready and
    /// charged [`BUSY`] on that card.
    fn busy(&self, name: &str, gpu: &str, pid: u32) -> String {
        let id = self.deploy(
            name,
            json!({"devices": [{"id": gpu, "sharing": "shared"}],
                   "engine_config": {"memory": {"request": "5GiB", "kv_cache": "4GiB"}}}),
        );
        self.start(&id, &format!("start-{name}"), StartScope::All)
            .unwrap();
        let planned = self.planned(&id);
        self.ready(&planned[0].0, pid);
        id
    }

    /// The refusal of a start of `scope`, with nothing planned for it.
    fn refused(&self, id: &str, key: &str, scope: StartScope) -> LifecycleError {
        let error = self
            .store
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
            .unwrap_err();
        assert!(self.planned(id).is_empty(), "nothing is planned");
        error
    }

    /// The operator changes every GPU's managed limit; the standalone's
    /// policy follows its document (ADR 0025).
    fn set_gpu_limits(&mut self, managed: &str) {
        for gpu in &self.gpus {
            self.host["resource_policy"]["domains"][gpu]["managed_limit"] = json!(managed);
        }
        let policy = capyctl_config::effective::normalize_host_policy(&self.host).unwrap();
        let observed: Vec<_> = std::iter::once("system".to_string())
            .chain(self.gpus.iter().cloned())
            .map(|d| observation(&d, NOW))
            .collect();
        self.store
            .import_resource_policy(&self.session, &policy, &observed, NOW)
            .unwrap();
    }

    /// Record the startup peaks a completed start measured, by domain.
    fn peak(&self, step: &str, peaks: &[(&str, i64)]) {
        let peaks = peaks
            .iter()
            .map(|(domain, bytes)| ((*domain).to_owned(), *bytes))
            .collect();
        assert!(self
            .store
            .record_startup_peak(&self.session, step, &peaks, NOW)
            .unwrap());
    }

    /// The payloads of the management events of `kind`.
    fn events(&self, kind: &str) -> Vec<Value> {
        self.sql
            .prepare("SELECT payload_json FROM management_events WHERE kind=?1 ORDER BY sequence")
            .unwrap()
            .query_map([kind], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|raw| serde_json::from_str(&raw.unwrap()).unwrap())
            .collect()
    }

    /// The evidence of each journal entry in `state`.
    fn journal(&self, state: &str) -> Vec<String> {
        self.sql
            .prepare("SELECT evidence FROM journal_entries WHERE state=?1 ORDER BY rowid")
            .unwrap()
            .query_map([state], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
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

// The per-GPU charges below are pinned. On the lab's 41 GiB cards managing
// 40 GiB, a vLLM request derived from the measured weights is the rank's
// share of the weights x 1.10 plus the 4 GiB KV cache, and at least 0.75 of
// the card (30.75 GiB); each GPU adds the 1.25 GiB context allowance and, on
// two or more GPUs, the 1 GiB communication allowance (ADR 0035 §3). Without
// a recorded layout a rank's share at N is 0.9 of the weights / N plus the
// 0.1 every rank keeps whole (ADR 0028 §5). The startup placeholder of a
// derived vLLM request is the request, so every phase charges the same:
//
// | weights | N = 1                  | N = 2                          |
// |---------|------------------------|--------------------------------|
// | 30 GiB  | 33 + 4 + 1.25 = 38.25  | 30.75 + 1.25 + 1 = 33.0        |
// | 40 GiB  | 44 + 4 + 1.25 = 49.25  | 30.75 + 1.25 + 1 = 33.0        |
// | 50 GiB  | 55 + 4 + 1.25 = 60.25  | 30.25 + 4 + 1.25 + 1 = 36.5    |
// | 100 GiB |                        | 60.5 + 4 + 1.25 + 1 = 66.75    |
//
// (GiB, before the integer rounding of the x 1.10.)
const W30_N1: i64 = 41_070_624_746;
const FLOOR_N2: i64 = 35_433_480_129;
const W40_N1: i64 = 52_881_784_766;
const W50_N1: i64 = 64_692_944_896;
const W50_N2: i64 = 39_191_576_510;
const W100_N2: i64 = 71_672_266_730;

// T46 (ADR 0035 §1a): `topology: auto` records the candidate table on the
// host's row and, once the weights are measured, places at the smallest N
// whose per-GPU charge the cards' managed limits hold: 30 GiB of weights fit
// one 40 GiB card (38.25 GiB), so N = 1 although two cards are idle and N = 2
// would charge less; 40 GiB of weights (49.25 GiB on one card) take N = 2.
#[test]
fn auto_places_at_the_size_the_cards_hold() {
    for (weights, table, chosen, devices) in [
        (30, [(1, W30_N1), (2, FLOOR_N2)], 1, vec!["gpu0"]),
        (40, [(1, W40_N1), (2, FLOOR_N2)], 2, vec!["gpu0", "gpu1"]),
    ] {
        let t = lab(2, "40GiB");
        let id = t.auto("auto", weights * GIB);
        assert_eq!(t.table(&id), table, "{weights} GiB");
        t.start(&id, "start", StartScope::All).unwrap();
        assert_eq!(t.placed(&id)[0].2, Some(chosen), "{weights} GiB");
        assert_eq!(t.planned(&id)[0].2, strings(&devices), "{weights} GiB");
        let status = t.status(&id);
        assert_eq!(
            status["instances"][0]["topology"],
            json!({"requested": "auto", "tensor_parallel": chosen, "devices": devices}),
            "{status}"
        );
        assert_eq!(status["topology"]["chosen"], chosen, "{status}");
    }
}

// T46 (ADR 0035 §§1a, 2): `auto` chooses N from the cards' managed limits,
// never their free memory, so the same deployment on the same cards gets the
// same N whatever else runs. 30 GiB of weights charge 38.25 GiB at N = 1 and
// 33.0 GiB at N = 2. With gpu0 busy the instance still runs at N = 1, on
// gpu1. With both cards holding another owner's 6.25 GiB, N = 1 fits neither
// card's free 33.75 GiB while N = 2 would: the start is not placed at N = 2;
// the switch planner evicts one owner (the one used least recently) for
// N = 1, and once it is gone the instance starts at N = 1 on that card.
#[test]
fn auto_chooses_from_the_cards_limits_never_their_free_memory() {
    // An idle card beside gpu1's owner: the same N as on idle cards.
    let t = lab(2, "40GiB");
    t.busy("b0", "gpu0", 100);
    assert_eq!(t.charged("gpu0"), BUSY);
    let id = t.auto("auto", 30 * GIB);
    t.start(&id, "start", StartScope::All).unwrap();
    assert_eq!(t.placed(&id), vec![(Some("gpu1".into()), None, Some(1))]);

    // Both cards busy: room for N = 2 now, but N = 1 is what the limits say.
    let t = lab(2, "40GiB");
    let b0 = t.busy("b0", "gpu0", 100);
    let b1 = t.busy("b1", "gpu1", 110);
    let id = t.auto("auto", 30 * GIB);
    const { assert!(40 * GIB - BUSY >= FLOOR_N2 && 40 * GIB - BUSY < W30_N1) };
    let used = {
        let (b0, b1) = (b0.clone(), b1.clone());
        move |deployment: &str, _: i64| {
            Some(if deployment == b0 {
                1
            } else if deployment == b1 {
                5
            } else {
                0
            })
        }
    };
    match t
        .store
        .plan_switch(
            &t.session,
            &id,
            None,
            false,
            None,
            &std::collections::BTreeSet::new(),
            &used,
            &Default::default(),
        )
        .unwrap()
    {
        SwitchPlan::Evict { victims, .. } => assert_eq!(
            victims
                .into_iter()
                .map(|v| v.deployment_id)
                .collect::<Vec<_>>(),
            vec![b0.clone()],
            "one owner, the least recently used, is evicted for N = 1"
        ),
        other => panic!("evicts for N = 1, never fits now at N = 2: {other:?}"),
    }
    assert!(matches!(
        t.refused(&id, "start", StartScope::All),
        LifecycleError::CapacityBlocked(_)
    ));
    assert_eq!(t.placed(&id), vec![(None, None, None)], "never N = 2");
    t.stop(&b0, 0, "stop-b0");
    t.start(&id, "start-again", StartScope::All).unwrap();
    assert_eq!(t.placed(&id), vec![(Some("gpu0".into()), None, Some(1))]);
}

// T46 (ADR 0035 §§1a, 3, 13): a start under `auto` whose N differs from the
// instance's previous start journals `topology_auto_changed` with both sizes
// and writes the management event of that name; a start at the same N does
// not. Measured startup peaks are kept per N: the peak measured at N = 1
// (41 GiB, above the card's 40 GiB limit) moves the next start to N = 2 and
// never sizes it; the peak measured at N = 2 (35 GiB, the largest of its
// GPUs) sizes every GPU of the next N = 2 start, on a set it was never
// measured on. Status shows `auto` and, once placed, the size and GPUs.
#[test]
fn a_changed_size_is_journaled_and_peaks_are_kept_per_size() {
    const CHANGED: &str = "topology_auto_changed";
    let t = lab(3, "40GiB");
    let id = t.auto("auto", 30 * GIB);
    let status = t.status(&id);
    assert_eq!(
        status["instances"][0]["topology"],
        json!({"requested": "auto"}),
        "not placed: {status}"
    );
    assert!(status["instances"][0].get("collectives").is_none());

    t.start(&id, "start-1", StartScope::All).unwrap();
    let planned = t.planned(&id);
    assert_eq!(planned[0].2, strings(&["gpu0"]));
    let status = t.status(&id);
    assert_eq!(
        status["instances"][0]["topology"],
        json!({"requested": "auto", "tensor_parallel": 1, "devices": ["gpu0"]}),
        "{status}"
    );
    assert!(status["instances"][0].get("collectives").is_none());
    assert!(
        t.events(CHANGED).is_empty(),
        "a first start changes nothing"
    );
    t.ready(&planned[0].0, 100);
    t.peak(&planned[0].0, &[("gpu0", 41 * GIB)]);
    t.stop(&id, 0, "stop-1");

    // The N = 1 peak no longer fits a card: N = 2, journaled once.
    t.start(&id, "start-2", StartScope::All).unwrap();
    let planned = t.planned(&id);
    assert_eq!(planned[0].2, strings(&["gpu0", "gpu1"]));
    assert_eq!(
        t.placed(&id),
        vec![(Some("gpu0".into()), Some("gpu0,gpu1".into()), Some(2))]
    );
    let events = t.events(CHANGED);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["deployment_id"], json!(id));
    assert_eq!(events[0]["instance_index"], 0);
    assert_eq!(events[0]["host_id"], "lab");
    assert_eq!(events[0]["previous_tensor_parallel"], 1);
    assert_eq!(events[0]["tensor_parallel"], 2);
    assert_eq!(events[0]["devices"], json!(["gpu0", "gpu1"]));
    let journal = t.journal(CHANGED);
    assert_eq!(journal.len(), 1);
    assert!(
        journal[0].ends_with(
            "instance 0 under topology: auto chose tensor_parallel 2 on host lab (gpu0, gpu1), \
             where its previous start chose 1"
        ),
        "{}",
        journal[0]
    );
    let context = t.arm(&planned[0].0);
    assert_eq!(
        (t.charged("gpu0"), t.charged("gpu1")),
        (FLOOR_N2, FLOOR_N2),
        "the peak measured at N = 1 never sizes a start at N = 2"
    );
    let status = t.status(&id);
    assert_eq!(
        status["instances"][0]["topology"],
        json!({"requested": "auto", "tensor_parallel": 2, "devices": ["gpu0", "gpu1"]}),
        "{status}"
    );
    assert_eq!(status["instances"][0]["collectives"], "loopback");
    t.complete(&planned[0].0, context, 200);
    t.peak(&planned[0].0, &[("gpu0", 35 * GIB), ("gpu1", 34 * GIB)]);
    t.stop(&id, 0, "stop-2");

    // gpu0 is busy now; the N = 2 peak sizes both GPUs of another set.
    t.busy("b", "gpu0", 300);
    t.start(&id, "start-3", StartScope::All).unwrap();
    let planned = t.planned(&id);
    assert_eq!(planned[0].2, strings(&["gpu1", "gpu2"]));
    assert_eq!(t.events(CHANGED).len(), 1, "the same N journals nothing");
    assert_eq!(t.journal(CHANGED).len(), 1);
    t.arm(&planned[0].0);
    assert_eq!(
        (t.charged("gpu1"), t.charged("gpu2")),
        (35 * GIB, 35 * GIB),
        "the per-GPU peak measured at N = 2 applies to every GPU of the set"
    );
}

// T46 (ADR 0035 §§1a, 13): when no size of `auto` fits a host's cards now
// (their managed limits were lowered after the revision was accepted), the
// start is refused at placement and the refusal names what would fit:
// `insufficient_device_memory` with the per-GPU charge at the largest N the
// host offers and how many such cards would hold it (30 GiB of weights on
// 20 GiB cards), or `insufficient_gpus` with the number of identical GPUs
// that would, estimated from the candidate table (50 GiB of weights: 60.25
// GiB per GPU at N = 1 and 36.5 GiB at N = 2, so about 5 GPUs managing
// 24 GiB each). Nothing is placed or planned.
#[test]
fn auto_refusals_name_what_would_fit() {
    let gib = |bytes: i64| format!("{bytes} bytes ({:.2} GiB)", bytes as f64 / GIB as f64);
    let mut t = lab(2, "40GiB");
    let id = t.auto("memory", 30 * GIB);
    t.set_gpu_limits("20GiB");
    match t.refused(&id, "start", StartScope::All) {
        LifecycleError::PlacementRefused(reason) => assert_eq!(
            reason,
            format!(
                "insufficient_device_memory: no allowed host can run the deployment as declared \
                 (host lab: insufficient_device_memory: no tensor_parallel up to this host's 2 \
                 GPU(s) fits; at tensor_parallel: 2 each GPU is charged {}, above the {} the \
                 largest GPU here manages; 2 identical GPUs each managing at least that would \
                 hold it)",
                gib(FLOOR_N2),
                gib(20 * GIB)
            )
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(t.placed(&id), vec![(None, None, None)]);

    let mut t = lab(2, "40GiB");
    let id = t.auto("gpus", 50 * GIB);
    assert_eq!(t.table(&id), [(1, W50_N1), (2, W50_N2)]);
    t.set_gpu_limits("24GiB");
    match t.refused(&id, "start", StartScope::All) {
        LifecycleError::PlacementRefused(reason) => assert_eq!(
            reason,
            format!(
                "insufficient_gpus: no allowed host can run the deployment as declared (host \
                 lab: insufficient_gpus: no tensor_parallel up to this host's 2 GPU(s) fits; at \
                 tensor_parallel: 2 each GPU is charged {}, above the {} each GPU here manages; \
                 about 5 identical GPUs like these would hold it (estimated from the candidate \
                 table))",
                gib(W50_N2),
                gib(24 * GIB)
            )
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(t.placed(&id), vec![(None, None, None)]);
}

// T46 (ADR 0035 §§1, 13, live row MG14): an explicit `tensor_parallel` is
// refused when the host has too few identical GPUs (`insufficient_gpus`,
// naming the count needed and the host's count; before the checkpoint is
// measured no size that would fit is guessed), and a size whose per-GPU
// charge the cards cannot hold names what would fit once the weights are
// measured: one GPU for 50 GiB of weights names `tensor_parallel: 2` and its
// per-GPU charge (36.5 GiB); two GPUs for 100 GiB of weights name the charge
// at N = 2 and that two cards managing at least that would hold it.
#[test]
fn explicit_size_refusals_name_what_would_fit() {
    let t = lab(2, "40GiB");
    let mut config = t.config.clone();
    config["name"] = json!("tp3");
    config["routes"] = json!(["tp3"]);
    config["topology"] = json!({"tensor_parallel": 3});
    let detail = match t
        .store
        .create_stopped_managed_configuration(
            &t.session,
            "owner",
            "tp3",
            &json!({ "config": config }).to_string(),
            &t.host,
            NOW,
        )
        .map(drop)
        .unwrap_err()
    {
        ManagedConfigurationError::Rejected(error) => error.detail,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        detail,
        "insufficient_gpus: tensor_parallel: 3 needs 3 identical discrete GPUs; host `lab` has \
         2; what would fit is known once the checkpoint is measured: no tensor-parallel size is \
         chosen before"
    );

    let derived = json!({"engine_config": {"memory": {"kv_cache": "4GiB"}}});
    let id = t.deploy("one", derived.clone());
    let diagnostic = t.unusable(&id, 50 * GIB);
    assert!(
        diagnostic.starts_with("insufficient_device_memory: "),
        "{diagnostic}"
    );
    assert!(
        diagnostic.ends_with(&format!(
            "what would fit: `topology: {{tensor_parallel: 2}}`, on 2 identical GPUs of this \
             host, each charged {W50_N2} bytes ({:.2} GiB)",
            W50_N2 as f64 / GIB as f64
        )),
        "{diagnostic}"
    );

    let mut patch = derived;
    patch["topology"] = json!({"tensor_parallel": 2});
    let id = t.deploy("two", patch);
    let diagnostic = t.unusable(&id, 100 * GIB);
    assert!(
        diagnostic.starts_with("insufficient_device_memory: "),
        "{diagnostic}"
    );
    // The whole hint is kept in the checkpoint's diagnostic, its card count
    // included (it was cut at 512 characters before).
    assert!(
        diagnostic.ends_with(&format!(
            "what would fit: no tensor_parallel up to this host's 2 GPU(s) fits; at \
             tensor_parallel: 2 each GPU is charged {W100_N2} bytes ({:.2} GiB), above the {} \
             bytes (40.00 GiB) the largest GPU here manages; 2 identical GPUs each managing at \
             least that would hold it",
            W100_N2 as f64 / GIB as f64,
            40 * GIB
        )),
        "{diagnostic}"
    );
}

// T46 (ADR 0035 §§1a, 2, 9): a parked instance keeps the N and GPU set it
// was started at. Parking at N > 1 is gated off in the shipped support data,
// so the park is recorded directly: instance 0 of an `auto` deployment runs
// at N = 2 on gpu0 and gpu1 and is marked parked. A peak measured at N = 2
// since (45 GiB, recorded for every size) leaves no size of `auto` fitting
// the 40 GiB cards, so instance 1 is refused `insufficient_device_memory` at
// placement; instance 0's wake is still planned, through its recorded set,
// and the restore it is accepted as wakes that launch: same binding, same
// generation, the same two GPUs, no placement.
#[test]
fn a_parked_instance_keeps_its_set_and_wakes_through_it() {
    let t = lab(4, "40GiB");
    let id = t.deploy(
        "auto",
        json!({"instances": 2, "topology": "auto",
               "engine_config": {"memory": {"kv_cache": "4GiB"}}}),
    );
    t.measure(&id, 40 * GIB);
    t.start(&id, "start", StartScope::All).unwrap();
    let planned = t.planned(&id);
    assert_eq!(planned[0].2, strings(&["gpu0", "gpu1"]));
    assert_eq!(planned[1].2, strings(&["gpu2", "gpu3"]));
    t.ready(&planned[0].0, 100);
    t.ready(&planned[1].0, 200);
    t.peak(&planned[0].0, &[("gpu0", 45 * GIB)]);
    for n in [3, 4] {
        t.sql
            .execute(
                "INSERT INTO startup_measurements(deployment_id,revision,host_id,installation,tensor_parallel,domain,peak_bytes,step_id,measured_at_ms)
                 SELECT deployment_id,revision,host_id,installation,?2,domain,peak_bytes,step_id,measured_at_ms
                   FROM startup_measurements WHERE deployment_id=?1 AND tensor_parallel=2",
                rusqlite::params![id, n],
            )
            .unwrap();
    }
    let placed = t.placed(&id)[0].clone();
    let generation: i64 = t
        .sql
        .query_row(
            "SELECT generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    t.sql
        .execute(
            "UPDATE deployment_instances SET observed_state='parked',dispatch_enabled=0 WHERE deployment_id=?1 AND instance_index=0",
            [&id],
        )
        .unwrap();
    t.stop(&id, 1, "stop-1");
    match t.refused(&id, "start-1", StartScope::Instance(1)) {
        LifecycleError::PlacementRefused(reason) => assert!(
            reason.starts_with("insufficient_device_memory: "),
            "{reason}"
        ),
        other => panic!("{other:?}"),
    }
    let plan = t
        .store
        .plan_switch(
            &t.session,
            &id,
            Some(0),
            true,
            None,
            &std::collections::BTreeSet::new(),
            &|_: &str, _: i64| Some(1),
            &Default::default(),
        )
        .unwrap();
    assert_eq!(plan, SwitchPlan::FitsNow, "the wake is planned on its set");
    let revision = t.store.current_revision(&id).unwrap().unwrap();
    let receipt = t
        .store
        .accept_restore_command(
            &t.session,
            "owner",
            &id,
            WakeScope::Instance(0),
            revision,
            "wake",
            NOW,
            DEADLINE,
        )
        .unwrap()
        .expect("the parked instance is woken");
    let (binding, devices): (String, String) = t
        .sql
        .query_row(
            "SELECT s.binding_id,json_extract(json_extract(i.step_json,'$.effective_json'),'$.selected_devices')
               FROM lifecycle_steps s
               JOIN lifecycle_steps i ON i.id=json_extract(s.step_json,'$.source_step_id')
              WHERE s.id=?1",
            [&receipt.step_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let source: String = t
        .sql
        .query_row(
            "SELECT binding_id FROM lifecycle_steps WHERE id=?1",
            [&planned[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(binding, source, "the wake restores the parked launch");
    let devices: Vec<String> = serde_json::from_str::<Value>(&devices)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(devices, strings(&["gpu0", "gpu1"]));
    assert_eq!(t.placed(&id)[0], placed, "the wake places nothing");
    let after: i64 = t
        .sql
        .query_row(
            "SELECT generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, generation);
}
