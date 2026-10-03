//! ADR 0019 (discrete GPU design §7, owner decision 3): on a multi-GPU host
//! capyctl picks the GPU. A deployment that pins no device is resolved once per
//! GPU; placement puts each instance on the GPU with room, the launch runs
//! the revision as resolved on that GPU, and a stopped instance keeps its
//! last GPU as a preference (ADR 0013 §4).
//!
//! CPU-only with Fake devices: every launch is recorded from scripted
//! evidence; no engine runs. Passing here never qualifies a native engine
//! recipe on a discrete GPU (SPEC §18).
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
/// ADR 0019: the device domain also carries the engine's CUDA context and graphs.
const OVERHEAD: i64 = capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;

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
    gpus("22GiB")
}

/// [`two_gpus`] with `gpu0` managing `gpu0_managed` (a heterogeneous host
/// when it is small).
fn gpus(gpu0_managed: &str) -> Gpus {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gpus.sqlite3");
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
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": gpu0_managed,
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
    let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
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
                reserve_absorbs_unmanaged: d.memory
                    == capyctl_config::effective::DomainMemory::Device,
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
    assert_eq!(
        t.charged("gpu1"),
        12 * GIB + OVERHEAD,
        "instance 0 is charged on gpu1"
    );
    assert_eq!(
        t.charged("gpu0"),
        12 * GIB + OVERHEAD,
        "instance 1 is charged on gpu0"
    );

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
    assert_eq!(
        (t.charged("gpu0"), t.charged("gpu1")),
        (12 * GIB + OVERHEAD, 0)
    );
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
    // 19 GiB (21.5 GiB starting, with the first start's graph allowance,
    // ADR 0014 amendment A8) fits either card only once its one instance is
    // released.
    let c = one("c", "19GiB");
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
                &Default::default(),
            )
            .unwrap()
        {
            capyctl_store::ordinary_lifecycle::switching::SwitchPlan::Evict { victims, .. } => {
                victims
                    .into_iter()
                    .map(|v| v.deployment_id)
                    .collect::<Vec<_>>()
            }
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

/// Explicit per-phase resources pinned to `gpu0`: `device` GiB on the card
/// when Ready, `system` GiB of host RAM, and, parked, 1 GiB of residue with a
/// `copy` GiB weights copy in host RAM beside the 4 GiB engine overhead.
fn host_backed_resources(device: u32, system: u32, copy: u32) -> Value {
    let parked = 4 + copy;
    let phase = |gpu: u32, ram: u32, devices: bool| {
        json!({"allocations": [
                   {"domain": "gpu0", "bytes": format!("{gpu}GiB"), "host_kv_bytes": "0B"},
                   {"domain": "system", "bytes": format!("{ram}GiB"), "host_kv_bytes": "0B"}],
               "devices": if devices { json!([{"id": "gpu0", "sharing": "shared"}]) } else { json!([]) }})
    };
    json!({
        "cold": phase(device, system, true),
        "ready": phase(device, system, true),
        "parking": phase(device, parked.max(system), true),
        "parked": phase(1, parked, false),
        "wake": phase(device, parked.max(system), true),
    })
}

// Discrete GPU design §5 ("When a copy does not fit"): the switch planner
// parks a host_backed victim whose weights copy fits host RAM after the
// switch, and stops one whose copy does not, never overcommitting host RAM.
// T27 T16
#[test]
fn a_host_backed_victim_parks_only_where_its_copy_fits() {
    let t = two_gpus();
    let deploy = |name: &str, device: &str, resources: Value| {
        t.deploy_as(
            name,
            json!({"instances": 1, "residency": "host_backed",
                   "devices": [{"id": device, "sharing": "shared"}],
                   "engine_config": {"memory": {"kv_cache": "4GiB"}},
                   "resources": resources}),
        )
    };
    let ready = |id: &str, pid: u32| {
        t.start(id, &format!("start-{id}"), StartScope::All);
        let planned = t.planned(id);
        t.ready(&planned[0].0, pid);
    };
    let plan = |target: &str| match t
        .store
        .plan_switch(
            &t.session,
            target,
            None,
            false,
            None,
            &std::collections::BTreeSet::new(),
            &|_: &str, _: i64| Some(1),
            &Default::default(),
        )
        .unwrap()
    {
        capyctl_store::ordinary_lifecycle::switching::SwitchPlan::Evict { victims, .. } => victims
            .into_iter()
            .map(|v| (v.deployment_id, v.parks, v.park_does_not_fit))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    // a holds 12 GiB of the 22 GiB card; c needs 12 GiB there too. a's
    // 8 GiB copy (12 GiB parked with the overhead) fits the 24 GiB of host
    // RAM and the 12 GiB parked limit beside c: a parks.
    let a = deploy("a", "gpu0", host_backed_resources(12, 4, 8));
    ready(&a, 100);
    let c = deploy("c", "gpu0", host_backed_resources(12, 4, 8));
    assert_eq!(plan(&c), vec![(a.clone(), true, false)]);
    // b, on the other card, holds 10 GiB of host RAM. a parked (12) beside
    // b (10) and c (4) would need 26 GiB of the 24 GiB host RAM: a stops.
    let b = t.deploy_as(
        "b",
        json!({"instances": 1, "residency": "restart_only",
               "devices": [{"id": "gpu1", "sharing": "shared"}],
               "engine_config": {"memory": {"kv_cache": "4GiB"}},
               "resources": host_backed_resources(12, 10, 0)
                   .as_object()
                   .unwrap()
                   .iter()
                   .map(|(phase, value)| {
                       let mut value = value.clone();
                       value["allocations"][0]["domain"] = json!("gpu1");
                       if phase == "parked" {
                           value["allocations"] = json!([
                               {"domain": "gpu1", "bytes": "0B", "host_kv_bytes": "0B"},
                               {"domain": "system", "bytes": "0B", "host_kv_bytes": "0B"}]);
                       } else {
                           value["devices"] = json!([{"id": "gpu1", "sharing": "shared"}]);
                       }
                       (phase.clone(), value)
                   })
                   .collect::<serde_json::Map<_, _>>()}),
    );
    ready(&b, 200);
    assert_eq!(plan(&c), vec![(a.clone(), false, true)]);
}

// T27 T16 (final review I4, found live on the discrete-GPU laptop host, DG1):
// the ledger alone planned a host_backed park that host RAM, held largely by
// other programs, could not take; the arm refused it and the victim was
// stopped anyway, after a wasted park. With the host's fresh observation the
// planner decides park or stop by min(ledger room, observed free memory minus
// the reserve): the same victim parks when the host has the memory and is
// planned a stop up front when it does not. Nothing is released on the
// observation alone: the victim set is the ledger's.
#[test]
fn a_host_backed_victim_is_planned_a_stop_when_observed_host_ram_cannot_take_its_copy() {
    let t = two_gpus();
    let deploy = |name: &str, resources: Value| {
        t.deploy_as(
            name,
            json!({"instances": 1, "residency": "host_backed",
                   "devices": [{"id": "gpu0", "sharing": "shared"}],
                   "engine_config": {"memory": {"kv_cache": "4GiB"}},
                   "resources": resources}),
        )
    };
    let a = deploy("a", host_backed_resources(12, 4, 8));
    t.start(&a, "start-a", StartScope::All);
    let planned = t.planned(&a);
    t.ready(&planned[0].0, 100);
    let c = deploy("c", host_backed_resources(12, 4, 8));
    let host = t.host["name"].as_str().unwrap().to_owned();
    let plan = |system_available: i64| {
        let mut observed =
            capyctl_store::ordinary_lifecycle::switching::PlanningObservations::new();
        if system_available > 0 {
            let observation = |domain: &str, capacity: i64, available: i64| MemoryObservation {
                domain: domain.into(),
                capacity_bytes: capacity,
                available_bytes: available,
                sampled_at_ms: NOW,
            };
            observed.insert(
                host.clone(),
                (
                    vec![
                        observation("system", 32 * GIB, system_available),
                        observation("gpu0", 23 * GIB, 11 * GIB),
                        observation("gpu1", 31 * GIB, 31 * GIB),
                    ],
                    // a's engine, sampled beside the availability: 12 GiB on
                    // the card, 2 GiB of host RAM.
                    vec![capyctl_domain::resources::ProcessResident {
                        pid: 100,
                        boot_id: "boot".into(),
                        start_ticks: 1000,
                        bytes: 14 * GIB,
                        device_bytes: 12 * GIB,
                        host_bytes: 2 * GIB,
                    }],
                ),
            );
        }
        match t
            .store
            .plan_switch(
                &t.session,
                &c,
                None,
                false,
                None,
                &std::collections::BTreeSet::new(),
                &|_: &str, _: i64| Some(1),
                &observed,
            )
            .unwrap()
        {
            capyctl_store::ordinary_lifecycle::switching::SwitchPlan::Evict { victims, .. } => {
                victims
                    .into_iter()
                    .map(|v| (v.deployment_id, v.parks, v.park_does_not_fit))
                    .collect::<Vec<_>>()
            }
            other => panic!("{other:?}"),
        }
    };
    // The ledger alone: a's parked copy fits the 24 GiB system domain.
    assert_eq!(plan(0), vec![(a.clone(), true, false)]);
    // 30 GiB of host RAM free: with a's 2 GiB returned, a parked (12) and c
    // (4) leave 16, above the 8 GiB reserve: a parks.
    assert_eq!(plan(30 * GIB), vec![(a.clone(), true, false)]);
    // Other programs hold most of it: 20 GiB free leaves 6, below the
    // reserve, so a is planned a stop, the same victim, never another.
    assert_eq!(plan(20 * GIB), vec![(a.clone(), false, true)]);
}

// T27 (final review M9, found live on the discrete-GPU laptop host): a switch
// park the arm refused because host memory could not take the copy
// (`parked_capacity`) left the victim reported `released: stopped` on the
// next round, as if it did not park at all. It is reported `stopped (host
// RAM full)`.
#[test]
fn a_victim_whose_switch_park_was_refused_for_memory_is_reported_host_ram_full() {
    let t = two_gpus();
    let a = t.deploy_as(
        "a",
        json!({"instances": 1, "residency": "host_backed",
               "devices": [{"id": "gpu0", "sharing": "shared"}],
               "engine_config": {"memory": {"kv_cache": "4GiB"}},
               "resources": host_backed_resources(12, 4, 8)}),
    );
    t.start(&a, "start-a", StartScope::All);
    let planned = t.planned(&a);
    t.ready(&planned[0].0, 100);
    let park = t
        .store
        .accept_park_command(
            &t.session,
            "switch",
            &a,
            t.store.current_revision(&a).unwrap().unwrap(),
            "park-a",
            NOW,
            DEADLINE,
        )
        .unwrap();
    let (_, limits, ttl, max_parked) = t.admission();
    let low: Vec<_> = [("system", 32, 9), ("gpu0", 23, 11), ("gpu1", 31, 31)]
        .iter()
        .map(|(domain, capacity, available)| MemoryObservation {
            domain: (*domain).into(),
            capacity_bytes: capacity * GIB,
            available_bytes: available * GIB,
            sampled_at_ms: NOW,
        })
        .collect();
    assert!(matches!(
        t.store
            .arm_residency(
                &t.session,
                &park.step_id,
                AdmissionContext::new(&low, &limits, NOW, ttl, max_parked),
            )
            .unwrap(),
        capyctl_store::ordinary_lifecycle::park::ResidencyArm::Refused("parked_capacity")
    ));
    let c = t.deploy_as(
        "c",
        json!({"instances": 1, "residency": "host_backed",
               "devices": [{"id": "gpu0", "sharing": "shared"}],
               "engine_config": {"memory": {"kv_cache": "4GiB"}},
               "resources": host_backed_resources(12, 4, 8)}),
    );
    let victims = match t
        .store
        .plan_switch(
            &t.session,
            &c,
            None,
            false,
            None,
            &std::collections::BTreeSet::new(),
            &|_: &str, _: i64| Some(1),
            &Default::default(),
        )
        .unwrap()
    {
        capyctl_store::ordinary_lifecycle::switching::SwitchPlan::Evict { victims, .. } => victims
            .into_iter()
            .map(|v| (v.deployment_id, v.parks, v.park_does_not_fit))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(victims, vec![(a, false, true)]);
}

/// The GPUs a deployment's current revision may be placed on, and the device
/// its host row names.
fn options(t: &Gpus, id: &str) -> (Vec<String>, Option<String>) {
    let devices = t
        .sql
        .prepare("SELECT device FROM host_device_effective_revisions WHERE deployment_id=?1 ORDER BY device")
        .unwrap()
        .query_map([id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let host: Option<String> = t
        .sql
        .query_row(
            "SELECT json_extract(effective_json,'$.selected_devices[0].id') FROM host_effective_revisions WHERE deployment_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    (devices, host)
}

// T27 (final review I7, design §7): on a heterogeneous multi-GPU host a GPU
// too small for the model is excluded for that deployment, never the whole
// host: a 12 GiB request is accepted on a host whose gpu0 manages 10 GiB and
// gpu1 30 GiB, gpu1 is its only option and the host's row, and it starts
// there. Before, the small GPU's refusal refused the host.
#[test]
fn a_gpu_too_small_for_the_model_is_excluded_not_the_host() {
    let t = gpus("10GiB");
    let id = t.deploy(json!({"instances": 1}));
    assert_eq!(
        options(&t, &id),
        (vec!["gpu1".to_string()], Some("gpu1".into()))
    );
    t.start(&id, "start", StartScope::All);
    let planned = t.planned(&id);
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].2, "gpu1");
}

// T27 T14 (final review I7, ADR 0014 §7): a revision sized from the weights is
// provisional until the checkpoint is measured, when every GPU is re-resolved.
// A GPU the measured weights no longer fit is dropped as an option; the
// record is not corrupt data and the revision stays usable on the other GPU.
#[test]
fn a_measured_checkpoint_drops_only_the_gpus_it_does_not_fit() {
    let t = gpus("10GiB");
    let id = t.deploy(json!({"instances": 1, "engine_config": {"memory": {"kv_cache": "4GiB"}}}));
    let revision = t.store.current_revision(&id).unwrap().unwrap();
    assert!(
        t.store
            .checkpoint_digest(&id, revision)
            .unwrap()
            .unwrap()
            .provisional,
        "sized once measured"
    );
    let (devices, _) = options(&t, &id);
    assert_eq!(devices, ["gpu0", "gpu1"], "both resolve before measurement");
    let outcome = t
        .store
        .record_checkpoint_digest(
            &t.session,
            &id,
            revision,
            "lab",
            &format!("sha256:{}", "a".repeat(64)),
            10 * GIB,
            NOW,
        )
        .expect("not corrupt data");
    assert!(
        matches!(
            outcome,
            capyctl_store::checkpoint_digests::RecordOutcome::Recorded { .. }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        options(&t, &id),
        (vec!["gpu1".to_string()], Some("gpu1".into()))
    );
}

// T27 (final review I9): the short pin form `devices: [{id: gpu1}]` is
// accepted (the sharing is the host's for that GPU) and places on gpu1.
#[test]
fn the_short_pin_form_is_accepted_and_placed_on_its_gpu() {
    let t = two_gpus();
    let id = t.deploy(json!({"instances": 1, "devices": [{"id": "gpu1"}]}));
    t.start(&id, "start", StartScope::All);
    let planned = t.planned(&id);
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].2, "gpu1");
    let sharing: String = t
        .sql
        .query_row(
            "SELECT json_extract(effective_json,'$.selected_devices[0].sharing') FROM effective_revisions WHERE deployment_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sharing, "shared");
}
