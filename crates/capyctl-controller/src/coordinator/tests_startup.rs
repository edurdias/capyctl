//! Owner decision 2026-09-23: the startup memory budget and the per-host
//! activation gate (ADR 0015 implementation note).
//!
//! These drive the real scheduler over scripted engines and a scripted
//! availability source. Nothing here runs or qualifies a native engine, and
//! the peaks are the script's, not measurements of any model.
use super::*;

/// The engine's CUDA context and graphs, charged beside the request on every
/// host shape (re-review parity rule).
const OVERHEAD: i64 = capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
/// ADR 0014 amendment A8: the first start's graph allowance in a placeholder.
const GRAPHS: i64 = capyctl_config::effective::STARTUP_GRAPH_ALLOWANCE_BYTES;
use capyctl_domain::resources::PhaseFootprint;
use std::sync::atomic::AtomicI64;

const GIB: i64 = 1 << 30;

fn startup_worker(
    owner: SharedCoordinatorState,
    observations: Arc<dyn ServiceObservation>,
    gates: BTreeMap<String, Arc<Gate>>,
    options: CoordinatorOptions,
) -> OwnedCoordinator {
    OwnedCoordinator::spawn(
        owner,
        observations,
        Arc::new(|| Ok(1900)),
        options,
        Arc::new(move |work| match gates.get(&work.fence().deployment_id) {
            Some(gate) => Ok(test_driver(gate.clone())),
            None => Err(CoordinatorError::Service(
                "no scripted engine for this deployment".into(),
            )),
        }),
    )
    .unwrap()
}

/// Set the host's managed limit on its one memory domain.
fn managed_limit(owner: &SharedCoordinatorState, observations: &[MemoryObservation], bytes: i64) {
    let o = owner.lock().unwrap();
    let mut controls = o.store().resource_policy("lab").unwrap().unwrap().controls;
    controls.domains.get_mut("unified").unwrap().managed_limit = bytes;
    o.store()
        .update_resource_policy(
            o.session(),
            "owner",
            "lab",
            1,
            "startup-budget",
            &controls,
            observations,
            1800,
        )
        .unwrap();
}

fn footprint(owner: &SharedCoordinatorState, deployment: &str) -> Option<PhaseFootprint> {
    owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners
        .get(deployment)
        .cloned()
}

fn step_state(sql: &rusqlite::Connection, deployment: &str) -> String {
    sql.query_row(
        "SELECT s.state FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
          WHERE s.deployment_id=?1 AND o.kind='initialize' ORDER BY s.rowid DESC LIMIT 1",
        [deployment],
        |r| r.get(0),
    )
    .unwrap()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// T26 T27: the fixture's deployments reserve a 10 GiB startup peak (their
/// declared cold phase) and 8 GiB once Ready. With a 19 GiB limit both peaks
/// do not fit together, but one peak beside the other's steady footprint
/// does: the second start waits, planned and never sent, until the first
/// reaches Ready, and then starts. Nothing fails, and each reservation drops
/// to its steady footprint at Ready.
// T24 T26 T27 T29
#[tokio::test]
async fn two_starts_whose_startup_peaks_do_not_fit_together_serialize() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    managed_limit(&owner, &observations, 19 * GIB);
    let (a, b) = (Gate::new(false), Gate::new(false));
    b.release.add_permits(1);
    let w = startup_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    let first = w.start(&fence, 60_000).unwrap();
    a.entered().await;
    assert_eq!(
        footprint(&owner, &fence.deployment_id).unwrap().phase,
        ResourcePhase::Cold
    );
    assert_eq!(
        footprint(&owner, &fence.deployment_id).unwrap().allocations[0].bytes,
        10 * GIB
    );
    let second = w.start(&other, 60_000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    // Several passes go by: the second start stays planned and unsent.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        b.calls.lock().unwrap().is_empty(),
        "the second start was sent"
    );
    assert_eq!(step_state(&sql, &other.deployment_id), "planned");
    assert!(footprint(&owner, &other.deployment_id).is_none());
    assert_eq!(w.status(), WorkerStatus::Running);
    // Status shows the reservation the queued start will hold, and why.
    let snapshot = owner.lock().unwrap().store().snapshot().unwrap();
    let queued = snapshot
        .deployments
        .iter()
        .find(|d| d.id == other.deployment_id)
        .unwrap();
    let startup = queued.instances[0].startup.as_ref().unwrap();
    assert_eq!(startup.bytes, 10 * GIB);
    assert_eq!(
        serde_json::to_value(startup).unwrap()["provenance"],
        "resources"
    );
    // The first reaches Ready and drops to its steady footprint; the second
    // then starts and reaches Ready beside it.
    a.release.add_permits(1);
    assert_eq!(
        first.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(
        second.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(*b.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    for deployment in [&fence.deployment_id, &other.deployment_id] {
        let held = footprint(&owner, deployment).unwrap();
        assert_eq!(held.phase, ResourcePhase::Ready);
        assert_eq!(held.allocations[0].bytes, 8 * GIB);
    }
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

/// T24 T26: placement judges a start's peak against the steady footprints of
/// launches still starting. A start command for a second deployment while the
/// first loads is accepted (queued), not refused for capacity, and runs once
/// the first reaches Ready.
// T24 T26 T29
#[tokio::test]
async fn a_start_command_is_placed_and_queued_behind_a_loading_peak() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    managed_limit(&owner, &observations, 19 * GIB);
    let (a, b) = (Gate::new(false), Gate::new(false));
    b.release.add_permits(1);
    let w = startup_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    w.commands()
        .start(
            "owner",
            &fence.deployment_id,
            fence.revision,
            "start-a",
            60_000,
        )
        .unwrap();
    a.entered().await;
    w.commands()
        .start(
            "owner",
            &other.deployment_id,
            other.revision,
            "start-b",
            60_000,
        )
        .expect("the second start is accepted while the first loads");
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(b.calls.lock().unwrap().is_empty());
    assert_eq!(step_state(&sql, &other.deployment_id), "planned");
    a.release.add_permits(1);
    until("both to reach Ready", || {
        [&fence.deployment_id, &other.deployment_id]
            .iter()
            .all(|d| footprint(&owner, d).is_some_and(|f| f.phase == ResourcePhase::Ready))
    })
    .await;
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

/// T26 T27: with room for both startup peaks, two starts on one host load at
/// the same time; the gate holds nothing.
// T26 T27
#[tokio::test]
async fn two_starts_whose_startup_peaks_fit_together_run_concurrently() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    managed_limit(&owner, &observations, 21 * GIB);
    let (a, b) = (Gate::new(false), Gate::new(false));
    let w = startup_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    let first = w.start(&fence, 60_000).unwrap();
    let second = w.start(&other, 60_000).unwrap();
    a.entered().await;
    b.entered().await;
    assert!(a.active.load(Ordering::SeqCst) && b.active.load(Ordering::SeqCst));
    for deployment in [&fence.deployment_id, &other.deployment_id] {
        assert_eq!(
            footprint(&owner, deployment).unwrap().phase,
            ResourcePhase::Cold
        );
    }
    a.release.add_permits(1);
    b.release.add_permits(1);
    for start in [first, second] {
        assert_eq!(
            start.wait(Duration::from_secs(10)).await.unwrap(),
            InitializeStatus::Completed
        );
    }
    w.shutdown().await.unwrap();
}

/// Found live 2026-09-23 (matrix M08): SGLang profiles device-wide free memory
/// while it loads, so another launch allocating on the same host at that time
/// makes it refuse to start. With room for both peaks, a start beside an
/// SGLang start still waits, planned and unsent, until the first is Ready.
// T26 T27
#[tokio::test]
async fn a_start_beside_a_loading_sglang_launch_waits_even_when_peaks_fit() {
    let (dir, owner, _, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    // An SGLang deployment on the same host, from the SGLang golden revision;
    // nothing here runs an engine, the scripted drivers ignore the family.
    let fence = {
        let o = owner.lock().unwrap();
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-sglang-golden.json"
        ))
        .unwrap();
        let vllm: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        // The fixture host's own document, with an SGLang profile beside its vLLM one.
        let mut host = vllm["input"]["host"].clone();
        host["runtime_profiles"]["sgl"] =
            source["input"]["host"]["runtime_profiles"]["local"].clone();
        let mut deployment = source["input"]["deployment"].clone();
        deployment["name"] = serde_json::json!("sglang-loading");
        deployment["routes"] = serde_json::json!(["sglang-loading"]);
        deployment["runtime_profile"] = serde_json::json!("sgl");
        let receipt = o
            .store()
            .create_stopped_managed_configuration(
                o.session(),
                "owner",
                "sglang-loading",
                &serde_json::json!({ "config": deployment }).to_string(),
                &host,
                1700,
            )
            .unwrap();
        DeploymentFence {
            deployment_id: receipt.deployment_id,
            revision: receipt.revision,
            generation: receipt.generation,
        }
    };
    // Room for both startup peaks at once: only the SGLang rule holds the second.
    managed_limit(&owner, &observations, 21 * GIB);
    let (a, b) = (Gate::new(false), Gate::new(false));
    b.release.add_permits(1);
    let w = startup_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    let first = w.start(&fence, 60_000).unwrap();
    a.entered().await;
    let second = w.start(&other, 60_000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        b.calls.lock().unwrap().is_empty(),
        "the start beside a loading SGLang launch was sent"
    );
    assert_eq!(step_state(&sql, &other.deployment_id), "planned");
    a.release.add_permits(1);
    for start in [first, second] {
        assert_eq!(
            start.wait(Duration::from_secs(10)).await.unwrap(),
            InitializeStatus::Completed
        );
    }
    assert_eq!(*b.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    w.shutdown().await.unwrap();
}

/// A host whose published availability the test scripts. Each observation is
/// sampled a millisecond after the last, never past the service clock.
struct ScriptedAvailability {
    domains: Vec<MemoryObservation>,
    available: Arc<AtomicI64>,
    sampled: AtomicI64,
}
impl ServiceObservation for ScriptedAvailability {
    fn observe(&self, _: String) -> ObservationFuture {
        let at = self.sampled.fetch_add(1, Ordering::SeqCst).min(1899);
        let available = self.available.load(Ordering::SeqCst);
        let values = self
            .domains
            .iter()
            .map(|o| MemoryObservation {
                available_bytes: available,
                sampled_at_ms: at,
                ..o.clone()
            })
            .collect();
        Box::pin(async move { Ok(values) })
    }
}

/// Waits until `host` has published `samples` more readings, so the value
/// the test scripted was read however slowly the runner schedules the
/// startup sampler.
async fn until_sampled(host: &ScriptedAvailability, samples: i64) {
    let from = host.sampled.load(Ordering::SeqCst);
    until(
        "the startup sampler to read the scripted availability",
        || host.sampled.load(Ordering::SeqCst) >= from + samples,
    )
    .await;
}

/// T29: a deployment with a derived memory request and no declared startup
/// peak reserves the placeholder (its request, while its weights are unknown)
/// on its first run. The run's peak drop in published availability is recorded
/// for the revision on its host; the next start reserves that measured peak as
/// its cold phase, status says it was measured, and at Ready the reservation
/// drops to the steady request.
// T29 T26
#[tokio::test]
async fn a_measured_startup_peak_is_recorded_and_reused() {
    let (dir, owner, _, observations) = setup().await;
    let fence = {
        let o = owner.lock().unwrap();
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = source["input"]["host"].clone();
        host["runtime_profiles"]["local"]["build_fingerprint"] =
            serde_json::json!("qualification-fake-v1");
        host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
            serde_json::json!("secret://another-admin");
        let mut deployment = source["input"]["deployment"].clone();
        deployment.as_object_mut().unwrap().remove("resources");
        deployment["name"] = serde_json::json!("measured");
        deployment["routes"] = serde_json::json!(["measured"]);
        deployment["engine_config"]["memory"] =
            serde_json::json!({"request": "8GiB", "kv_cache": "4GiB"});
        let receipt = o
            .store()
            .create_stopped_managed_configuration(
                o.session(),
                "owner",
                "measured",
                &serde_json::json!({ "config": deployment }).to_string(),
                &host,
                1700,
            )
            .unwrap();
        DeploymentFence {
            deployment_id: receipt.deployment_id,
            revision: receipt.revision,
            generation: receipt.generation,
        }
    };
    let baseline = observations[0].available_bytes;
    let available = Arc::new(AtomicI64::new(baseline));
    // One scripted engine per launch: the first run, then the second.
    let (gate, again) = (Gate::new(false), Gate::new(false));
    let launches = Arc::new(Mutex::new(vec![again.clone(), gate.clone()]));
    let scripted = Arc::new(ScriptedAvailability {
        domains: observations.clone(),
        available: available.clone(),
        sampled: AtomicI64::new(1000),
    });
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        scripted.clone(),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            startup_sample_interval: Some(Duration::from_millis(10)),
            ..Default::default()
        },
        Arc::new(move |_| match launches.lock().unwrap().pop() {
            Some(gate) => Ok(test_driver(gate)),
            None => Err(CoordinatorError::Service("no scripted engine left".into())),
        }),
    )
    .unwrap();
    let first = w.start(&fence, 60_000).unwrap();
    gate.entered().await;
    // The placeholder: weights unknown, so the startup peak is the request
    // and the graph allowance.
    assert_eq!(
        footprint(&owner, &fence.deployment_id).unwrap().allocations[0].bytes,
        8 * GIB + GRAPHS + OVERHEAD
    );
    // The engine's load drops availability by 12 GiB, then it settles.
    available.store(baseline - 12 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    available.store(baseline - 8 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    gate.release.add_permits(1);
    assert_eq!(
        first.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let peak = |sql: &rusqlite::Connection| -> Option<i64> {
        sql.query_row(
            "SELECT peak_bytes FROM startup_measurements WHERE deployment_id=?1 AND revision=?2",
            rusqlite::params![fence.deployment_id, fence.revision],
            |r| r.get(0),
        )
        .ok()
    };
    until("the measured peak", || peak(&sql).is_some()).await;
    assert_eq!(peak(&sql), Some(12 * GIB));
    let held = footprint(&owner, &fence.deployment_id).unwrap();
    assert_eq!(held.phase, ResourcePhase::Ready);
    assert_eq!(held.allocations[0].bytes, 8 * GIB + OVERHEAD);

    // Stop, then start again through placement: the measured peak is reused.
    available.store(baseline, Ordering::SeqCst);
    let stop = w.stop("owner", &fence, "stop-measured", 60_000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(10)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    w.commands()
        .start(
            "owner",
            &fence.deployment_id,
            fence.revision,
            "start-measured-again",
            60_000,
        )
        .unwrap();
    again.entered().await;
    let held = footprint(&owner, &fence.deployment_id).unwrap();
    assert_eq!(held.phase, ResourcePhase::Cold);
    assert_eq!(held.allocations[0].bytes, 12 * GIB);
    let snapshot = owner.lock().unwrap().store().snapshot().unwrap();
    let status = snapshot
        .deployments
        .iter()
        .find(|d| d.id == fence.deployment_id)
        .unwrap();
    let instance = serde_json::to_value(status.instances[0].startup.as_ref().unwrap()).unwrap();
    assert_eq!(instance["bytes"], 12 * GIB);
    assert_eq!(instance["provenance"], "measured");
    let deployment = serde_json::to_value(status.startup.as_ref().unwrap()).unwrap();
    assert_eq!(deployment["provenance"], "default");
    assert_eq!(deployment["bytes"], 8 * GIB + GRAPHS + OVERHEAD);
    assert_eq!(deployment["measured"][0]["peak_bytes"], 12 * GIB);
    again.release.add_permits(1);
    until("the second start to reach Ready", || {
        footprint(&owner, &fence.deployment_id)
            .is_some_and(|held| held.phase == ResourcePhase::Ready)
    })
    .await;
    assert_eq!(
        footprint(&owner, &fence.deployment_id).unwrap().allocations[0].bytes,
        8 * GIB + OVERHEAD
    );
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

/// ADR 0014 amendment A11: availability sampled while the engine reported a
/// kernel build (its compilers' memory, on a unified host) is not part of the
/// startup peak. Samples before and after the build are, so the recorded peak
/// is the engine's own, and the next start is not refused for capacity.
// T29
#[tokio::test]
async fn a_peak_sampled_during_a_kernel_build_is_not_recorded() {
    let (dir, owner, _, observations) = setup().await;
    let fence = measured_deployment(&owner, "building");
    let baseline = observations[0].available_bytes;
    let available = Arc::new(AtomicI64::new(baseline));
    let gate = Gate::new(false);
    let scripted = Arc::new(ScriptedAvailability {
        domains: observations.clone(),
        available: available.clone(),
        sampled: AtomicI64::new(1000),
    });
    let w = startup_worker(
        owner.clone(),
        scripted.clone(),
        BTreeMap::from([(fence.deployment_id.clone(), gate.clone())]),
        CoordinatorOptions {
            startup_sample_interval: Some(Duration::from_millis(10)),
            ..Default::default()
        },
    );
    let first = w.start(&fence, 60_000).unwrap();
    gate.entered().await;
    // Loading the weights drops availability by 6 GiB.
    available.store(baseline - 6 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    // The compilers take 40 GiB more while the build runs. The sample taken
    // just before the build began may already read it, so the build is
    // reported from that sample on (the watcher's poll brackets a build).
    let from = scripted.sampled.load(Ordering::SeqCst) - 1;
    available.store(baseline - 46 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    // The build ends; graphs and the KV cache bring the engine to 12 GiB.
    available.store(baseline - 12 * GIB, Ordering::SeqCst);
    let ended = scripted.sampled.load(Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    available.store(baseline - 8 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    *gate.builds.lock().unwrap() = vec![capyctl_domain::completion::KernelBuild {
        from_ms: from,
        until_ms: ended,
    }];
    gate.release.add_permits(1);
    assert_eq!(
        first.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let peak = || -> Option<i64> {
        sql.query_row(
            "SELECT peak_bytes FROM startup_measurements WHERE deployment_id=?1 AND revision=?2",
            rusqlite::params![fence.deployment_id, fence.revision],
            |r| r.get(0),
        )
        .ok()
    };
    until("the measured peak", || peak().is_some()).await;
    assert_eq!(peak(), Some(12 * GIB));
    w.shutdown().await.unwrap();
}

/// ADR 0014 amendment A11: a start whose every sample fell inside a kernel
/// build measured nothing of its own, so it records no peak and the next
/// start keeps the placeholder.
// T29
#[tokio::test]
async fn a_start_that_built_kernels_throughout_records_no_peak() {
    let (dir, owner, _, observations) = setup().await;
    let fence = measured_deployment(&owner, "always-building");
    let baseline = observations[0].available_bytes;
    let available = Arc::new(AtomicI64::new(baseline));
    let gate = Gate::new(false);
    let scripted = Arc::new(ScriptedAvailability {
        domains: observations.clone(),
        available: available.clone(),
        sampled: AtomicI64::new(1000),
    });
    let w = startup_worker(
        owner.clone(),
        scripted.clone(),
        BTreeMap::from([(fence.deployment_id.clone(), gate.clone())]),
        CoordinatorOptions {
            startup_sample_interval: Some(Duration::from_millis(10)),
            ..Default::default()
        },
    );
    let first = w.start(&fence, 60_000).unwrap();
    gate.entered().await;
    available.store(baseline - 46 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    *gate.builds.lock().unwrap() = vec![capyctl_domain::completion::KernelBuild {
        from_ms: 0,
        until_ms: i64::MAX,
    }];
    gate.release.add_permits(1);
    assert_eq!(
        first.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    // The step is complete; give a recording every chance to land.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let measured: i64 = sql
        .query_row(
            "SELECT COUNT(*) FROM startup_measurements WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(measured, 0);
    w.shutdown().await.unwrap();
}

/// A stopped deployment whose memory request is derived and whose startup
/// peak is the measurable placeholder (no declared `resources:` block).
fn measured_deployment(owner: &SharedCoordinatorState, name: &str) -> DeploymentFence {
    let o = owner.lock().unwrap();
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut host = source["input"]["host"].clone();
    host["runtime_profiles"]["local"]["build_fingerprint"] =
        serde_json::json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        serde_json::json!("secret://another-admin");
    let mut deployment = source["input"]["deployment"].clone();
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["name"] = serde_json::json!(name);
    deployment["routes"] = serde_json::json!([name]);
    deployment["engine_config"]["memory"] =
        serde_json::json!({"request": "8GiB", "kv_cache": "4GiB"});
    let receipt = o
        .store()
        .create_stopped_managed_configuration(
            o.session(),
            "owner",
            name,
            &serde_json::json!({ "config": deployment }).to_string(),
            &host,
            1700,
        )
        .unwrap();
    DeploymentFence {
        deployment_id: receipt.deployment_id,
        revision: receipt.revision,
        generation: receipt.generation,
    }
}

/// T29: a peak measured while another task ran on the same host cannot be
/// attributed to one launch, so none is recorded.
// T29
#[tokio::test]
async fn a_contended_startup_records_no_peak() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let baseline = observations[0].available_bytes;
    let available = Arc::new(AtomicI64::new(baseline));
    let (a, b) = (Gate::new(false), Gate::new(false));
    let w = startup_worker(
        owner.clone(),
        Arc::new(ScriptedAvailability {
            domains: observations.clone(),
            available: available.clone(),
            sampled: AtomicI64::new(1000),
        }),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions {
            startup_sample_interval: Some(Duration::from_millis(10)),
            ..Default::default()
        },
    );
    let first = w.start(&fence, 60_000).unwrap();
    let second = w.start(&other, 60_000).unwrap();
    a.entered().await;
    b.entered().await;
    available.store(baseline - 12 * GIB, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.release.add_permits(1);
    b.release.add_permits(1);
    for start in [first, second] {
        assert_eq!(
            start.wait(Duration::from_secs(10)).await.unwrap(),
            InitializeStatus::Completed
        );
    }
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let recorded: i64 = sql
        .query_row("SELECT COUNT(*) FROM startup_measurements", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(recorded, 0);
    w.shutdown().await.unwrap();
}

/// Owner decision 2026-09-23 (solo first start): an unmeasured model whose
/// placeholder startup estimate exceeds the host's managed limit, while its
/// steady request fits, starts only when no other engine holds a charge on
/// the host, and reserves the whole managed limit until Ready. Beside another
/// engine an explicit start is refused `startup_requires_empty_host`. The solo
/// run is uncontended by construction, so its peak is recorded, and the next
/// start reserves that measured peak like any other start: beside another
/// engine it is an ordinary capacity decision.
// T26 T27 T29
#[tokio::test]
async fn an_unmeasured_model_above_the_managed_limit_starts_alone_and_is_measured() {
    use crate::coordinator::CoordinatorCommandError;
    use capyctl_store::lifecycle::LifecycleError;
    let (dir, owner, fence, observations) = setup().await;
    // Weights 20 GiB, KV 4 GiB: request 20 + 4 + 8 (margin) = 32 GiB; the
    // placeholder peak is 20 × 2.25 + 8 = 53 GiB (ADR 0014 amendment A8),
    // above the 36 GiB limit.
    managed_limit(&owner, &observations, 36 * GIB);
    let big = {
        let o = owner.lock().unwrap();
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = source["input"]["host"].clone();
        host["runtime_profiles"]["local"]["build_fingerprint"] =
            serde_json::json!("qualification-fake-v1");
        host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
            serde_json::json!("secret://another-admin");
        host["resource_policy"]["domains"]["unified"]["managed_limit"] = serde_json::json!("36GiB");
        let mut deployment = source["input"]["deployment"].clone();
        deployment.as_object_mut().unwrap().remove("resources");
        deployment["name"] = serde_json::json!("big");
        deployment["routes"] = serde_json::json!(["big"]);
        deployment["engine_config"]["memory"] = serde_json::json!({"kv_cache": "4GiB"});
        let receipt = o
            .store()
            .create_stopped_managed_configuration(
                o.session(),
                "owner",
                "big",
                &serde_json::json!({ "config": deployment }).to_string(),
                &host,
                1700,
            )
            .unwrap();
        o.store()
            .record_checkpoint_digest(
                o.session(),
                &receipt.deployment_id,
                receipt.revision,
                "lab",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                20 * GIB,
                1700,
            )
            .unwrap();
        receipt
    };
    let baseline = observations[0].available_bytes;
    let available = Arc::new(AtomicI64::new(baseline));
    let a = Gate::new(false);
    a.release.add_permits(8);
    let (first, second) = (Gate::new(false), Gate::new(false));
    let launches = Arc::new(Mutex::new(vec![second.clone(), first.clone()]));
    let (small, gate_a) = (fence.deployment_id.clone(), a.clone());
    let scripted = Arc::new(ScriptedAvailability {
        domains: observations.clone(),
        available: available.clone(),
        sampled: AtomicI64::new(1000),
    });
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        scripted.clone(),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            startup_sample_interval: Some(Duration::from_millis(10)),
            ..Default::default()
        },
        Arc::new(move |work| {
            if work.fence().deployment_id == small {
                return Ok(test_driver(gate_a.clone()));
            }
            match launches.lock().unwrap().pop() {
                Some(gate) => Ok(test_driver(gate)),
                None => Err(CoordinatorError::Service("no scripted engine left".into())),
            }
        }),
    )
    .unwrap();
    let id = big.deployment_id.clone();
    let start_big = |key: &str| w.commands().start("owner", &id, big.revision, key, 60_000);
    let status = |owner: &SharedCoordinatorState| {
        let snapshot = owner.lock().unwrap().store().snapshot().unwrap();
        snapshot
            .deployments
            .into_iter()
            .find(|d| d.id == id)
            .unwrap()
    };

    // Another engine holds a charge: refused, typed, nothing reserved.
    let other = w.start(&fence, 60_000).unwrap();
    assert_eq!(
        other.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert!(matches!(
        start_big("start-beside"),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::StartupRequiresEmptyHost
        ))
    ));
    assert!(footprint(&owner, &id).is_none());
    let before = serde_json::to_value(status(&owner).startup.unwrap()).unwrap();
    assert_eq!(before["provenance"], "default");
    assert_eq!(before["bytes"], 53 * GIB + OVERHEAD);

    // Alone on the host it starts and holds the whole managed limit.
    let stop = w.stop("owner", &fence, "stop-small", 60_000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(10)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    start_big("start-alone").unwrap();
    first.entered().await;
    let held = footprint(&owner, &id).unwrap();
    assert_eq!(held.phase, ResourcePhase::Cold);
    assert_eq!(held.allocations[0].bytes, 36 * GIB);
    let reserved =
        serde_json::to_value(status(&owner).instances[0].startup.clone().unwrap()).unwrap();
    assert_eq!(reserved["provenance"], "whole_host");
    assert_eq!(reserved["bytes"], 36 * GIB);
    // Its load drops availability by 34 GiB, then it settles at Ready.
    available.store(baseline - 34 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    available.store(baseline - 32 * GIB, Ordering::SeqCst);
    until_sampled(&scripted, 5).await;
    first.release.add_permits(1);
    until("the solo start to reach Ready", || {
        footprint(&owner, &id).is_some_and(|held| held.phase == ResourcePhase::Ready)
    })
    .await;
    assert_eq!(
        footprint(&owner, &id).unwrap().allocations[0].bytes,
        32 * GIB + OVERHEAD
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let peak = |sql: &rusqlite::Connection| -> Option<i64> {
        sql.query_row(
            "SELECT peak_bytes FROM startup_measurements WHERE deployment_id=?1",
            [&id],
            |r| r.get(0),
        )
        .ok()
    };
    until("the measured peak", || peak(&sql).is_some()).await;
    assert_eq!(peak(&sql), Some(34 * GIB));

    // The next start reserves the measured peak, not the whole host.
    available.store(baseline, Ordering::SeqCst);
    w.commands()
        .stop("owner", &id, big.revision, "stop-big", 60_000)
        .unwrap();
    until("the solo run to stop", || footprint(&owner, &id).is_none()).await;
    start_big("start-measured").unwrap();
    second.entered().await;
    let held = footprint(&owner, &id).unwrap();
    assert_eq!(held.phase, ResourcePhase::Cold);
    assert_eq!(held.allocations[0].bytes, 34 * GIB);
    let reserved =
        serde_json::to_value(status(&owner).instances[0].startup.clone().unwrap()).unwrap();
    assert_eq!(reserved["provenance"], "measured");
    second.release.add_permits(1);
    until("the measured start to reach Ready", || {
        footprint(&owner, &id).is_some_and(|held| held.phase == ResourcePhase::Ready)
    })
    .await;
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

/// Owner decision 2026-09-23 (solo first start), found live 2026-09-23: a
/// revision accepted while its checkpoint digest was pending was frozen with
/// the request as its startup placeholder, so the solo first start never
/// triggered. The host sizes the weights (a stat walk) right after deploy, the
/// estimate is recomputed from them, and a start beside another engine is then
/// refused `startup_requires_empty_host`, all while the digest stays pending.
// T26 T27 T29
#[tokio::test]
async fn weights_sized_while_the_digest_is_pending_trigger_the_solo_first_start() {
    use crate::coordinator::CoordinatorCommandError;
    use capyctl_store::lifecycle::LifecycleError;
    let (_dir, owner, fence, observations) = setup().await;
    managed_limit(&owner, &observations, 36 * GIB);
    let big = {
        let o = owner.lock().unwrap();
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = source["input"]["host"].clone();
        host["runtime_profiles"]["local"]["build_fingerprint"] =
            serde_json::json!("qualification-fake-v1");
        host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
            serde_json::json!("secret://another-admin");
        host["resource_policy"]["domains"]["unified"]["managed_limit"] = serde_json::json!("36GiB");
        let mut deployment = source["input"]["deployment"].clone();
        deployment.as_object_mut().unwrap().remove("resources");
        deployment["name"] = serde_json::json!("big");
        deployment["routes"] = serde_json::json!(["big"]);
        // A declared request: accepted and startable while the digest is
        // pending, its placeholder startup frozen at the request (30 GiB).
        deployment["engine_config"]["memory"] =
            serde_json::json!({"request": "30GiB", "kv_cache": "4GiB"});
        o.store()
            .create_stopped_managed_configuration(
                o.session(),
                "owner",
                "big",
                &serde_json::json!({ "config": deployment }).to_string(),
                &host,
                1700,
            )
            .unwrap()
    };
    let id = big.deployment_id.clone();
    let startup = |owner: &SharedCoordinatorState| {
        let snapshot = owner.lock().unwrap().store().snapshot().unwrap();
        let d = snapshot
            .deployments
            .into_iter()
            .find(|d| d.id == id)
            .unwrap();
        serde_json::to_value(d.startup.unwrap()).unwrap()
    };
    assert_eq!(startup(&owner)["bytes"], 30 * GIB + GRAPHS + OVERHEAD);
    // The host sized 24 GiB of weights; the digest itself is still pending.
    {
        let o = owner.lock().unwrap();
        assert!(o
            .store()
            .record_checkpoint_weights(o.session(), &id, big.revision, "lab", 24 * GIB, 1700)
            .unwrap());
        let digest = o
            .store()
            .checkpoint_digest(&id, big.revision)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(digest.state).unwrap(),
            serde_json::json!("pending")
        );
    }
    // 24 × 2.25 = 54 GiB (plus the margin): above the 36 GiB limit.
    let estimate = startup(&owner);
    assert_eq!(estimate["provenance"], "default");
    assert!(estimate["bytes"].as_i64().unwrap() > 36 * GIB, "{estimate}");

    let a = Gate::new(false);
    a.release.add_permits(8);
    let small = fence.deployment_id.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(ScriptedAvailability {
            domains: observations.clone(),
            available: Arc::new(AtomicI64::new(observations[0].available_bytes)),
            sampled: AtomicI64::new(1000),
        }),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(move |work| {
            assert_eq!(
                work.fence().deployment_id,
                small,
                "only the small engine launches"
            );
            Ok(test_driver(a.clone()))
        }),
    )
    .unwrap();
    let other = w.start(&fence, 60_000).unwrap();
    assert_eq!(
        other.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert!(matches!(
        w.commands()
            .start("owner", &id, big.revision, "start-beside", 60_000),
        Err(CoordinatorCommandError::Lifecycle(
            LifecycleError::StartupRequiresEmptyHost
        ))
    ));
    assert!(footprint(&owner, &id).is_none());
    w.shutdown().await.unwrap();
}
