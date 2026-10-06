//! ADR 0019 (discrete GPU design §§7-8): an enrolled host with discrete GPUs
//! behaves exactly like a discrete standalone host. Its device-memory domains
//! import under host-scoped ledger keys, placement picks the GPU with room
//! on it, each instance is charged on its own GPU's scoped domain, and the
//! launch the server sends names the chosen GPU by its host-local id, which
//! the host pins by UUID or by index.
//!
//! CPU-only with Fake devices: every launch is recorded from scripted
//! evidence; no engine runs. Passing here never qualifies a native engine
//! recipe on a discrete GPU (SPEC §18).
use capyctl_config::effective::CudaNamespace;
use capyctl_config::remote_resources::{
    ledger_key, local_deployment_document, scope_host_document,
};
use capyctl_domain::completion::{
    CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity, StepExecutionContext,
};
use capyctl_domain::resources::{MemoryLimit, MemoryObservation, ProcessResident};
use capyctl_scheduler::residency::AdmissionContext;
use capyctl_store::dispatch::CoordinatorSession;
use capyctl_store::managed_configuration::HostTarget;
use capyctl_store::ordinary_lifecycle::placement::StartScope;
use capyctl_store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const NOW: i64 = 10_000;
const DEADLINE: i64 = 200_000;
const GIB: i64 = 1 << 30;
/// ADR 0019: the device domain also carries the engine's CUDA context and graphs.
const OVERHEAD: i64 = capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
const HOST: (&str, &str) = ("host-a", "laptop");
const UUID1: &str = "GPU-11111111-1111-1111-1111-111111111111";

struct Remote {
    domains: Vec<&'static str>,
    _dir: tempfile::TempDir,
    store: Store,
    sql: Connection,
    session: CoordinatorSession,
    config: Value,
    host: Value,
    target: HostTarget,
}

/// The host document an enrolled discrete host publishes: host RAM in
/// `system` and two GPUs of 22 and 30 GiB managed, each its own device
/// domain. Only gpu1 publishes its UUID, so gpu0 is pinned by index.
fn discrete_host() -> (Value, Value) {
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let (config, mut host) = (
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
        "gpu0": {"domain": "gpu0", "sharing": "shared"},
        "gpu1": {"domain": "gpu1", "sharing": "shared", "physical_gpu_uuid": UUID1}
    });
    (config, host)
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

fn remote() -> Remote {
    let (mut config, host) = discrete_host();
    let object = config.as_object_mut().unwrap();
    object.remove("resources");
    config["devices"] = json!([]);
    config["engine_config"] = json!({"memory": {"request": "12GiB", "kv_cache": "4GiB"}});
    remote_on(config, host, vec!["system", "gpu0", "gpu1"])
}

/// One enrolled host publishing `host`, whose journal fences per instance,
/// and `config` deployed as two restart-only instances placed there.
fn remote_on(mut config: Value, host: Value, domains: Vec<&'static str>) -> Remote {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote.sqlite3");
    let store = Store::open(&path).unwrap();
    let sql = Connection::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let (id, name) = HOST;
    sql.execute(
        "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES(?1,?2,'key',0)",
        params![id, name],
    )
    .unwrap();
    // ADR 0013 §4: the host's journal fences per instance, so two instances
    // of one deployment may share it (one per GPU here).
    sql.execute(
        "INSERT OR REPLACE INTO host_launch_claims VALUES(?1,'per_instance',1)",
        [id],
    )
    .unwrap();
    // The publication path (`host_publication::publish`): the host's own
    // document, normalized, with one observation per local domain.
    let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
    let observed: Vec<_> = domains.iter().map(|d| observation(d, 1)).collect();
    store
        .import_remote_resource_policy(&session, id, &policy, &observed, 1)
        .expect("the remote policy imports");
    let trusted = scope_host_document(id, &host).unwrap();
    let current = store.resource_policy(id).unwrap().unwrap();
    let target = HostTarget {
        host_id: id.into(),
        host_name: name.into(),
        trusted_host: capyctl_config::effective::compose_current_resource_controls(
            &trusted,
            &current.context,
            &current.controls,
        )
        .unwrap(),
        scoped: true,
    };
    config.as_object_mut().unwrap().remove("host");
    config["residency"] = json!("restart_only");
    config["instances"] = json!(2);
    config["placement"] = json!({"hosts": [name]});
    Remote {
        domains,
        _dir: dir,
        store,
        sql,
        session,
        config,
        host,
        target,
    }
}

impl Remote {
    fn scoped(&self, kind: &str, local: &str) -> String {
        ledger_key(HOST.0, kind, local)
    }

    fn deploy(&self) -> String {
        self.store
            .create_managed_configuration_on_hosts(
                &self.session,
                "owner",
                "deploy",
                &json!({ "config": self.config }).to_string(),
                std::slice::from_ref(&self.target),
                &[],
                NOW,
            )
            .unwrap()
            .deployment_id
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

    fn ready(&self, step: &str, pid: u32) {
        let observations: Vec<_> = self
            .domains
            .iter()
            .map(|d| observation(&self.scoped("domain", d), NOW))
            .collect();
        let context = self.arm(step, &observations, &[]).unwrap();
        self.complete(step, context, pid);
    }

    /// Arm a planned start against `observations`, crediting `residents`.
    fn arm(
        &self,
        step: &str,
        observations: &[MemoryObservation],
        residents: &[ProcessResident],
    ) -> Result<StepExecutionContext, capyctl_store::lifecycle::LifecycleError> {
        let policy = self.store.resource_policy(HOST.0).unwrap().unwrap();
        let limits: Vec<_> = policy
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
        self.store
            .arm_initialize_with_residents(
                &self.session,
                step,
                AdmissionContext::new(
                    observations,
                    &limits,
                    NOW,
                    policy.controls.observation_ttl_ms,
                    policy.controls.max_parked as usize,
                ),
                residents,
            )
            .map(|(_, context)| context.expect("a new arm"))
    }

    /// Associate and complete an armed start on scripted evidence.
    fn complete(&self, step: &str, context: StepExecutionContext, pid: u32) {
        let policy = self.store.resource_policy(HOST.0).unwrap().unwrap();
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
                policy.controls.observation_ttl_ms,
            )
            .unwrap();
    }

    fn charged(&self, local_domain: &str) -> i64 {
        let domain = self.scoped("domain", local_domain);
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

    /// The namespace the host pins the launch of `device` (a scoped id) to:
    /// the server sends the GPU-scoped document with its ids made local
    /// again, and the host resolves it against its own policy.
    fn pinned(&self, id: &str, device: &str) -> Option<CudaNamespace> {
        let revision = self.store.current_revision(id).unwrap().unwrap();
        let source = self
            .store
            .launch_configuration_source(id, revision, HOST.0, Some(device))
            .unwrap()
            .expect("the chosen GPU's document");
        let local = local_deployment_document(HOST.0, &source).unwrap();
        let host = capyctl_config::remote_resources::local_host_document(&self.host).unwrap();
        let effective = capyctl_config::effective::resolve_effective(&local, &host).unwrap();
        effective.cuda_namespace().unwrap()
    }
}

// Carried from the discrete standalone path (Tasks 7 and 9): a remote
// multi-GPU host picks the GPU with room for each instance, charges each on
// its own GPU's host-scoped domain, and the launch pins the chosen GPU on the
// host, by its published UUID or else by its PCI-ordered index.
// T27 T26 T16 T03
#[test]
fn a_remote_multi_gpu_host_places_and_pins_like_a_discrete_standalone() {
    let t = remote();
    let id = t.deploy();
    t.store
        .accept_scoped_start_command(
            &t.session,
            "owner",
            &id,
            StartScope::All,
            t.store.current_revision(&id).unwrap().unwrap(),
            "start",
            NOW,
            DEADLINE,
            None,
        )
        .unwrap();
    let planned = t.planned(&id);
    let (gpu0, gpu1) = (t.scoped("device", "gpu0"), t.scoped("device", "gpu1"));
    assert_eq!(
        planned
            .iter()
            .map(|(_, k, device)| (*k, device.clone()))
            .collect::<Vec<_>>(),
        vec![(0, gpu1.clone()), (1, gpu0.clone())],
        "the first instance takes the card with more room"
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
    assert_eq!(t.charged("system"), 2 * (4 * GIB), "engine host overhead");

    // The host pins each launch to its own GPU: gpu1 by the UUID it
    // published, gpu0 by index in PCI bus order.
    assert_eq!(
        t.pinned(&id, &gpu1),
        Some(CudaNamespace::Uuid(UUID1.into()))
    );
    assert_eq!(t.pinned(&id, &gpu0), Some(CudaNamespace::PciIndex(0)));
}

/// Today's two-host setup: the golden unified host, one pool for weights and
/// host pages. Two 8 GiB Ready (10 GiB cold) instances of one deployment.
fn remote_unified() -> Remote {
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    config["devices"] = json!([{"sharing": "shared"}]);
    for phase in ["cold", "ready", "parking", "wake"] {
        config["resources"][phase]["devices"] = json!([{"sharing": "shared"}]);
    }
    remote_on(config, golden["input"]["host"].clone(), vec!["unified"])
}

// T26 (ADR 0007, carried from the device-domain resident split): a remote
// unified host keeps exactly today's resident credit. With instance 0 Ready
// and holding 8 GiB, the host reports 30 GiB of 64 GiB free: charging
// instance 0 again leaves 30 - 8 - 10 = 12 GiB, below the 16 GiB reserve, so
// instance 1 waits without a sample; its processes' resident sum credits
// instance 0 on the unified domain, from an older host (sum only) and from a
// host that also reports the split figures alike.
// T26 T05
#[test]
fn a_remote_unified_host_is_credited_the_resident_sum_as_before() {
    for split in [false, true] {
        let t = remote_unified();
        let id = t.deploy();
        t.store
            .accept_scoped_start_command(
                &t.session,
                "owner",
                &id,
                StartScope::All,
                t.store.current_revision(&id).unwrap().unwrap(),
                "start",
                NOW,
                DEADLINE,
                None,
            )
            .unwrap();
        let planned = t.planned(&id);
        assert_eq!(planned.len(), 2);
        t.ready(&planned[0].0, 100);
        let observations = vec![MemoryObservation {
            domain: t.scoped("domain", "unified"),
            capacity_bytes: 64 * GIB,
            available_bytes: 30 * GIB,
            sampled_at_ms: NOW,
        }];
        let resident = |pid: u32, gib: i64| ProcessResident {
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid) * 10,
            bytes: gib * GIB,
            device_bytes: if split { (gib - 1) * GIB } else { 0 },
            host_bytes: if split { GIB } else { 0 },
        };
        assert!(
            t.arm(&planned[1].0, &observations, &[]).is_err(),
            "without a sample instance 0 is charged twice"
        );
        let context = t
            .arm(
                &planned[1].0,
                &observations,
                &[resident(100, 5), resident(101, 3)],
            )
            .expect("instance 0's resident sum is credited on the unified domain");
        t.complete(&planned[1].0, context, 200);
        assert_eq!(t.charged("unified"), 16 * GIB, "split={split}");
    }
}

// T26 (ADR 0007, ADR 0019; carried must-do): a remote discrete host's
// residents credit each domain from its own figure, as on a discrete
// standalone host. Two instances pinned to gpu1: with instance 0 Ready
// (12 GiB on the card), the card reports 20 GiB free, so charging instance 0
// again leaves no room for instance 1. The split figures credit instance 0 on
// gpu1 (and its host pages on `system`), which admits instance 1; residents
// from an older host carry only the sum, which credits no device domain.
// T26 T05
#[test]
fn a_remote_discrete_host_is_credited_per_domain() {
    for split in [false, true] {
        let (mut config, host) = discrete_host();
        config.as_object_mut().unwrap().remove("resources");
        config["devices"] = json!([{"id": "gpu1", "sharing": "shared"}]);
        config["engine_config"] = json!({"memory": {"request": "12GiB", "kv_cache": "4GiB"}});
        let t = remote_on(config, host, vec!["system", "gpu0", "gpu1"]);
        let id = t.deploy();
        t.store
            .accept_scoped_start_command(
                &t.session,
                "owner",
                &id,
                StartScope::All,
                t.store.current_revision(&id).unwrap().unwrap(),
                "start",
                NOW,
                DEADLINE,
                None,
            )
            .unwrap();
        let planned = t.planned(&id);
        assert_eq!(planned.len(), 2);
        assert!(planned
            .iter()
            .all(|(_, _, d)| *d == t.scoped("device", "gpu1")));
        t.ready(&planned[0].0, 100);
        let observations: Vec<_> = ["system", "gpu0", "gpu1"]
            .iter()
            .map(|d| MemoryObservation {
                domain: t.scoped("domain", d),
                capacity_bytes: if *d == "gpu1" { 32 * GIB } else { 1 << 40 },
                available_bytes: if *d == "gpu1" { 20 * GIB } else { 1 << 40 },
                sampled_at_ms: NOW,
            })
            .collect();
        let resident = |pid: u32, device: i64, host: i64| ProcessResident {
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid) * 10,
            bytes: (device + host) * GIB,
            device_bytes: if split { device * GIB } else { 0 },
            host_bytes: if split { host * GIB } else { 0 },
        };
        let residents = [resident(100, 11, 2), resident(101, 1, 1)];
        let armed = t.arm(&planned[1].0, &observations, &residents);
        if split {
            let context = armed.expect("instance 0's GPU bytes are credited on gpu1");
            t.complete(&planned[1].0, context, 200);
            assert_eq!(t.charged("gpu1"), 2 * (12 * GIB + OVERHEAD));
        } else {
            assert!(
                armed.is_err(),
                "a sum without the split credits no device domain (fail closed)"
            );
        }
    }
}

// T27, ADR 0028 §5, SPEC §3: a host's group rendezvous range may overlap its
// endpoint range. An ordinary start on that host leases past a port an
// unsettled group plan holds there as its rendezvous port, instead of taking it
// or failing; the torch store binds that port on every interface.
#[test]
fn an_ordinary_start_leases_past_a_held_rendezvous_port() {
    let t = remote();
    let id = t.deploy();
    t.sql
        .execute(
            "INSERT INTO group_plans VALUES(?1,9,1,'{}',?2,8100,'active')",
            params![id, HOST.0],
        )
        .unwrap();
    t.store
        .accept_scoped_start_command(
            &t.session,
            "owner",
            &id,
            StartScope::All,
            t.store.current_revision(&id).unwrap().unwrap(),
            "start",
            NOW,
            DEADLINE,
            None,
        )
        .unwrap();
    assert_eq!(t.planned(&id).len(), 2);
    let ports: Vec<u16> = t
        .sql
        .prepare("SELECT port FROM endpoint_leases WHERE host_id=?1 ORDER BY port")
        .unwrap()
        .query_map([HOST.0], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ports, vec![8101, 8102]);
}
