//! W10 (SPEC §10, ADR 0013 §8, owner decision D4): request-driven switching
//! driven by the real worker against scripted engines on one host whose
//! budget holds one READY deployment beside a cold start, not two.
//!
//! CPU and scripted engines only. The vLLM and SGLang deployments here differ
//! in their frozen configuration, not in any engine behaviour; nothing here
//! qualifies a native engine recipe or a host (SPEC §18).
use super::*;
use crate::coordinator_port::CoordinatorLifecycle;
use crate::fault::LifecycleFault;
use crate::port::LifecyclePort;
use crate::switching::SwitchOptions;
use mllm_domain::completion::{EffectObservation, ExecutionIdentities, Milestone};
use serde_json::{json, Value};
use std::sync::atomic::AtomicI64;

/// Every deployment's engine: Initialize through the Fake lifecycle, park and
/// restore answered with the facts a remote host proves (W4).
struct Scripted {
    /// One Fake lifecycle per instance incarnation, as each instance's own
    /// engine.
    gates: Mutex<BTreeMap<String, Arc<Gate>>>,
    clock: Arc<AtomicI64>,
    /// (action, deployment) of every call.
    calls: Mutex<Vec<(RuntimeAction, String)>>,
    /// Refuse every park before any effect (an engine without a verified
    /// release path).
    refuse_park: AtomicBool,
}

impl Scripted {
    fn new(clock: Arc<AtomicI64>) -> Arc<Self> {
        Arc::new(Self {
            gates: Mutex::new(BTreeMap::new()),
            clock,
            calls: Mutex::new(vec![]),
            refuse_park: AtomicBool::new(false),
        })
    }
    fn calls(&self, action: RuntimeAction, deployment: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(a, d)| *a == action && d == deployment)
            .count()
    }
}

#[async_trait::async_trait]
impl EngineAdapter for Scripted {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        let c = &command.context;
        self.calls
            .lock()
            .unwrap()
            .push((command.action, c.token.deployment_id.clone()));
        if command.action == RuntimeAction::Initialize {
            let gate = self
                .gates
                .lock()
                .unwrap()
                .entry(format!("{}:{}", c.token.deployment_id, c.token.generation))
                .or_insert_with(|| {
                    let gate = Gate::new(false);
                    gate.release.add_permits(256);
                    gate
                })
                .clone();
            return gate.execute_persisted(command).await;
        }
        let ExecutionIdentities::Retained(identities) = &c.identities else {
            return Err(RuntimeError::StaleRevision);
        };
        let facts = match command.action {
            RuntimeAction::Park if self.refuse_park.load(Ordering::SeqCst) => {
                return Err(RuntimeError::Unsupported)
            }
            RuntimeAction::Park => vec![Milestone::MemoryReleased],
            RuntimeAction::Restore => vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable,
            ],
            _ => return Err(RuntimeError::Unsupported),
        };
        Ok(EffectObservation {
            token: c.token.clone(),
            binding_id: c.binding_id.clone(),
            incarnation: c.incarnation.clone(),
            identities: identities.clone(),
            observed_at_ms: self.clock.load(Ordering::SeqCst),
            receipt: format!("scripted {:?}", command.action),
            facts,
        })
    }
    async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn cancel_work(
        &self,
        _: &MemberRef,
        _: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
}

struct Fresh {
    observations: Vec<MemoryObservation>,
    clock: Arc<AtomicI64>,
}
impl ServiceObservation for Fresh {
    fn observe(&self, _: String) -> ObservationFuture {
        let at = self.clock.load(Ordering::SeqCst);
        let values = self
            .observations
            .iter()
            .cloned()
            .map(|mut o| {
                o.sampled_at_ms = at;
                o
            })
            .collect();
        Box::pin(async move { Ok(values) })
    }
}

struct Lab {
    dir: tempfile::TempDir,
    owner: SharedCoordinatorState,
    worker: OwnedCoordinator,
    engine: Arc<Scripted>,
    /// vLLM, deep (the fixture's first deployment).
    a: DeploymentFence,
    /// vLLM, deep (the fixture's second deployment).
    c: DeploymentFence,
    /// The host policy as the budget left it, for deployments created later.
    managed_gib: i64,
    window_ms: i64,
    max_parked: Option<u32>,
}

/// A host whose managed budget is `managed_gib` and whose admission window
/// is `window_ms`; deployments cold 10 GiB, READY 8 GiB, parked 2 GiB.
async fn lab(managed_gib: i64, window_ms: i64) -> Lab {
    lab_parking(managed_gib, window_ms, None).await
}

/// As [`lab`], with the host's `max_parked` set when given.
async fn lab_parking(managed_gib: i64, window_ms: i64, max_parked: Option<u32>) -> Lab {
    lab_options(
        managed_gib,
        window_ms,
        max_parked,
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
    )
    .await
}

/// As [`lab_parking`], with the worker's options given.
async fn lab_options(
    managed_gib: i64,
    window_ms: i64,
    max_parked: Option<u32>,
    options: CoordinatorOptions,
) -> Lab {
    lab_bindings(
        managed_gib,
        window_ms,
        max_parked,
        options,
        |scripted, clock| {
            Arc::new(move |_: &InitializeWork| {
                let observed = clock.clone();
                Ok(ExecutionBinding::remote(
                    scripted.clone(),
                    Arc::new(move |context: CleanupExecutionContext| {
                        let at = observed.load(Ordering::SeqCst);
                        Box::pin(async move {
                            Ok(CleanupEvidence {
                                binding_id: context.binding_id,
                                incarnation: context.incarnation,
                                identities: context.identities,
                                observed_at_ms: at,
                                receipt: "scripted host observed the owned group gone".into(),
                            })
                        })
                    }),
                ))
            })
        },
    )
    .await
}

/// As [`lab_options`], with the execution bindings built from the lab's
/// scripted engine and clock.
async fn lab_bindings(
    managed_gib: i64,
    window_ms: i64,
    max_parked: Option<u32>,
    options: CoordinatorOptions,
    bindings: impl FnOnce(Arc<Scripted>, Arc<AtomicI64>) -> Arc<dyn ExecutionBindings>,
) -> Lab {
    let (dir, owner, a, observations) = setup().await;
    let c = fixture::owned_source().await.other.clone();
    {
        let o = owner.lock().unwrap();
        let mut controls = o.store().resource_policy("lab").unwrap().unwrap().controls;
        controls.domains.get_mut("unified").unwrap().managed_limit = managed_gib << 30;
        controls.queue.admission_window_ms = window_ms;
        if let Some(max_parked) = max_parked {
            controls.max_parked = max_parked;
        }
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                1,
                "switch-budget",
                &controls,
                &observations,
                1800,
            )
            .unwrap();
    }
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Scripted::new(clock.clone());
    let now = clock.clone();
    let worker = OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        Arc::new(Fresh {
            observations,
            clock: clock.clone(),
        }),
        Arc::new(move || Ok(now.load(Ordering::SeqCst))),
        options,
        bindings(engine.clone(), clock),
    )
    .unwrap();
    Lab {
        dir,
        owner,
        worker,
        engine,
        a,
        c,
        managed_gib,
        window_ms,
        max_parked,
    }
}

impl Lab {
    fn port(&self, drain_timeout: Duration) -> CoordinatorLifecycle {
        CoordinatorLifecycle::new(self.worker.commands()).with_switch_options(SwitchOptions {
            drain_timeout,
            poll: Duration::from_millis(10),
            ..Default::default()
        })
    }

    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("srv.sqlite3")).unwrap()
    }

    /// (observed state, dispatch open, generation, operator stopped) of one
    /// instance.
    fn instance(&self, deployment: &str, index: u32) -> (String, bool, Option<i64>, bool) {
        self.sql()
            .query_row(
                "SELECT observed_state,dispatch_enabled=1,generation,operator_stopped=1 FROM deployment_instances
                  WHERE deployment_id=?1 AND instance_index=?2",
                rusqlite::params![deployment, index],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    fn state(&self, deployment: &str) -> String {
        self.instance(deployment, 0).0
    }

    fn switch_events(&self) -> Vec<(String, Value)> {
        let sql = self.sql();
        let mut stmt = sql
            .prepare("SELECT kind,payload_json FROM management_events WHERE kind LIKE 'switch_%' ORDER BY sequence")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| {
                let (kind, payload) = r.unwrap();
                (kind, serde_json::from_str(&payload).unwrap())
            })
            .collect()
    }

    fn kinds(&self) -> Vec<String> {
        self.switch_events().into_iter().map(|(k, _)| k).collect()
    }

    /// Operations of `kind` on a deployment accepted by `principal`, with
    /// their states.
    fn operations(&self, deployment: &str, kind: &str) -> Vec<String> {
        let sql = self.sql();
        let mut stmt = sql
            .prepare("SELECT state FROM operations WHERE deployment_id=?1 AND kind=?2 ORDER BY accepted_at,id")
            .unwrap();
        stmt.query_map(rusqlite::params![deployment, kind], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    async fn ready(&self, fence: &DeploymentFence) {
        let start = self.worker.start(fence, 100_000).unwrap();
        assert_eq!(
            start.wait(Duration::from_secs(30)).await.unwrap(),
            InitializeStatus::Completed
        );
    }

    /// Create one more managed deployment on the host from a golden
    /// fixture, edited.
    fn deploy(&self, name: &str, golden: &str, edit: impl FnOnce(&mut Value)) -> String {
        let source: Value = serde_json::from_str(golden).unwrap();
        let mut host = source["input"]["host"].clone();
        host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
        host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
            json!("secret://another-admin");
        // The trusted host document matches the policy the budget published.
        host["resource_policy"]["domains"]["unified"]["managed_limit"] =
            json!(format!("{}GiB", self.managed_gib));
        host["resource_policy"]["queue"]["admission_window"] =
            json!(format!("{}ms", self.window_ms));
        if let Some(max_parked) = self.max_parked {
            host["resource_policy"]["max_parked"] = json!(max_parked);
        }
        let mut deployment = source["input"]["deployment"].clone();
        deployment["name"] = json!(name);
        deployment["routes"] = json!([name]);
        edit(&mut deployment);
        let o = self.owner.lock().unwrap();
        o.store()
            .create_stopped_managed_configuration(
                o.session(),
                "owner",
                name,
                &json!({ "config": deployment }).to_string(),
                &host,
                1700,
            )
            .unwrap()
            .deployment_id
    }
}

const VLLM: &str = include_str!("../../../mllm-config/tests/fixtures/effective-vllm-golden.json");
const SGLANG: &str =
    include_str!("../../../mllm-config/tests/fixtures/effective-sglang-golden.json");

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// T16 T22 T26 (D4, ADR 0013 §8): vLLM A is READY; a request for SGLang B,
// which does not fit beside it, parks A at its declared tier, verifies the
// release, starts B and serves it. A request for A then parks B and wakes A
// in place (A -> B -> A), each release on its own evidence.
#[tokio::test]
async fn a_request_for_b_parks_a_starts_b_and_back_again() {
    let lab = lab(15, 2_000).await;
    lab.ready(&lab.a).await;
    let b = lab.deploy("sglang-b", SGLANG, |_| {});
    let port = lab.port(Duration::from_secs(5));

    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(lab.state(&lab.a.deployment_id), "parked");
    let (_, open, _, operator_stopped) = lab.instance(&lab.a.deployment_id, 0);
    assert!(!open && !operator_stopped, "A stays on-demand eligible");
    assert_eq!(lab.operations(&lab.a.deployment_id, "park"), ["succeeded"]);
    // An idle A has no fairness window to honour; the switch ran end to end.
    assert_eq!(
        lab.kinds(),
        [
            "switch_planned",
            "switch_admission_closed",
            "switch_released",
            "switch_completed"
        ]
    );
    let (_, planned) = &lab.switch_events()[0];
    assert_eq!(planned["target_deployment"], json!(b));
    assert_eq!(
        planned["victims"],
        json!([format!("{}/0", lab.a.deployment_id)])
    );
    // The park was accepted under the switch principal and journaled.
    let journaled: i64 = lab
        .sql()
        .query_row(
            "SELECT COUNT(*) FROM journal_entries j JOIN operations o ON o.id=j.operation_id
              WHERE o.deployment_id=?1 AND o.kind='park' AND j.state='switch_park'",
            [&lab.a.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(journaled, 1);

    // A -> B -> A: B parks, A wakes in place under its own generation.
    let (_, _, a_generation, _) = lab.instance(&lab.a.deployment_id, 0);
    tokio::time::timeout(
        Duration::from_secs(30),
        port.activate_for_request(&lab.a.deployment_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(lab.state(&lab.a.deployment_id), "ready");
    assert_eq!(lab.instance(&lab.a.deployment_id, 0).2, a_generation);
    assert_eq!(lab.state(&b), "parked");
    assert_eq!(
        lab.engine
            .calls(RuntimeAction::Restore, &lab.a.deployment_id),
        1
    );
    assert_eq!(
        lab.engine
            .calls(RuntimeAction::Initialize, &lab.a.deployment_id),
        1
    );
    assert_eq!(lab.engine.calls(RuntimeAction::Initialize, &b), 1);
    assert_eq!(
        lab.kinds()
            .iter()
            .filter(|k| *k == "switch_completed")
            .count(),
        2
    );
    lab.worker.shutdown().await.unwrap();
}

// T16 T26, SPEC §6.5, §10: found live 2026-09-23 (matrix M31) under
// `max_parked: 1`, switching back to a parked deployment reclaimed that very
// target to make room for the victim's park, so every switch was a cold
// restart. The parked target is waking, not reclaimable: the victim parks
// beside it and the target wakes in place under its own generation.
#[tokio::test]
async fn under_max_parked_one_a_switch_wakes_its_parked_target_in_place() {
    let lab = lab_parking(15, 2_000, Some(1)).await;
    lab.ready(&lab.a).await;
    let b = lab.deploy("sglang-b", SGLANG, |_| {});
    let port = lab.port(Duration::from_secs(5));
    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(lab.state(&lab.a.deployment_id), "parked");
    let (_, _, a_generation, _) = lab.instance(&lab.a.deployment_id, 0);

    tokio::time::timeout(
        Duration::from_secs(30),
        port.activate_for_request(&lab.a.deployment_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(lab.state(&lab.a.deployment_id), "ready");
    assert_eq!(
        lab.state(&b),
        "parked",
        "the victim parks beside the waking target"
    );
    assert_eq!(
        lab.instance(&lab.a.deployment_id, 0).2,
        a_generation,
        "woken, not restarted"
    );
    assert_eq!(
        lab.engine
            .calls(RuntimeAction::Restore, &lab.a.deployment_id),
        1
    );
    assert_eq!(
        lab.engine
            .calls(RuntimeAction::Initialize, &lab.a.deployment_id),
        1
    );
    // Nothing reclaimed the target to make room for the victim's park.
    assert!(lab.operations(&lab.a.deployment_id, "stop").is_empty());
    lab.worker.shutdown().await.unwrap();
}

// T15: simultaneous requests for B join one switch and one activation: A is
// parked once and B initialized once.
#[tokio::test]
async fn concurrent_requests_for_b_join_one_switch_and_one_activation() {
    let lab = lab(15, 2_000).await;
    lab.ready(&lab.a).await;
    let b = lab.c.deployment_id.clone();
    let port = Arc::new(lab.port(Duration::from_secs(5)));
    let calls = (0..4).map(|_| {
        let port = port.clone();
        let b = b.clone();
        tokio::spawn(async move { port.activate_for_request(&b).await })
    });
    for outcome in futures::future::join_all(calls).await {
        outcome.unwrap().unwrap();
    }
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(lab.operations(&lab.a.deployment_id, "park"), ["succeeded"]);
    assert_eq!(lab.engine.calls(RuntimeAction::Initialize, &b), 1);
    assert_eq!(
        lab.engine.calls(RuntimeAction::Park, &lab.a.deployment_id),
        1
    );
    assert_eq!(
        lab.kinds()
            .iter()
            .filter(|k| *k == "switch_planned")
            .count(),
        1
    );
    lab.worker.shutdown().await.unwrap();
}

// T19 (SPEC §10 fairness): A is busy the whole time. Its admission stays open
// for the host's window, counted from the switch's start, and then closes:
// sustained A traffic cannot reset or extend the window.
#[tokio::test]
async fn busy_a_keeps_admission_only_for_the_non_resetting_window() {
    let lab = lab(15, 400).await;
    lab.ready(&lab.a).await;
    let (_, _, generation, _) = lab.instance(&lab.a.deployment_id, 0);
    let b = lab.c.deployment_id.clone();
    let port = Arc::new(lab.port(Duration::from_secs(5)));
    let commands = lab.worker.commands();
    let a = lab.a.deployment_id.clone();
    let traffic = tokio::spawn(async move {
        loop {
            commands.note_activity(&a, generation);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let started = std::time::Instant::now();
    let switch = {
        let port = port.clone();
        let b = b.clone();
        tokio::spawn(async move { port.activate_for_request(&b).await })
    };
    until("A's admission closes", || {
        !lab.instance(&lab.a.deployment_id, 0).1
    })
    .await;
    let closed_after = started.elapsed();
    assert!(
        closed_after >= Duration::from_millis(350),
        "A kept admission for its window: {closed_after:?}"
    );
    assert!(
        closed_after < Duration::from_millis(3_000),
        "busy A cannot extend the window: {closed_after:?}"
    );
    tokio::time::timeout(Duration::from_secs(30), switch)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    traffic.abort();
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(lab.state(&lab.a.deployment_id), "parked");
    lab.worker.shutdown().await.unwrap();
}

// T17 (SPEC §10: a drain timeout fails the switch by default): a request
// accepted by A before the switch stays in flight past the drain bound. The
// switch fails, nothing is parked or killed, A serves again and B was never
// started. Once the request completes, the next request's switch succeeds.
#[tokio::test]
async fn a_drain_timeout_fails_the_switch_and_a_keeps_serving() {
    let lab = lab(15, 50).await;
    lab.ready(&lab.a).await;
    let (_, _, generation, _) = lab.instance(&lab.a.deployment_id, 0);
    let b = lab.c.deployment_id.clone();
    let port = lab.port(Duration::from_millis(300));
    let lease = port
        .open_instance_lease(&lab.a.deployment_id, generation.unwrap(), 32)
        .await
        .unwrap()
        .unwrap();

    let failed = tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(&failed, LifecycleFault::Unavailable(m) if m.contains("drain timeout")),
        "{failed:?}"
    );
    let (state, open, _, _) = lab.instance(&lab.a.deployment_id, 0);
    assert_eq!((state.as_str(), open), ("ready", true), "A serves again");
    assert!(
        lab.operations(&lab.a.deployment_id, "park").is_empty(),
        "no premature park"
    );
    assert_eq!(lab.engine.calls(RuntimeAction::Initialize, &b), 0);
    assert_eq!(lab.state(&b), "stopped");
    assert_eq!(
        lab.kinds().last().map(String::as_str),
        Some("switch_failed")
    );
    // A new request reaches A again: its gate is open.
    let again = port
        .open_instance_lease(&lab.a.deployment_id, generation.unwrap(), 32)
        .await
        .unwrap()
        .unwrap();
    port.close_request_lease(again, crate::request_leases::LeaseEnd::Completed)
        .await
        .unwrap();

    port.close_request_lease(lease, crate::request_leases::LeaseEnd::Completed)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(lab.state(&lab.a.deployment_id), "parked");
    lab.worker.shutdown().await.unwrap();
}

// T27 (ADR 0013 §8 rules 4–5): with A running two instances and C one, the
// switch evicts one instance of A, whose route keeps serving, rather than
// C's last READY instance, even though C was used least recently. No
// fairness window applies to an instance that is not its deployment's last.
#[tokio::test]
async fn a_victim_whose_deployment_serves_elsewhere_goes_first() {
    let lab = lab(28, 5_000).await;
    let a = lab.deploy("two-a", VLLM, |d| d["instances"] = json!(2));
    let revision: i64 = lab
        .sql()
        .query_row("SELECT revision FROM deployments WHERE id=?1", [&a], |r| {
            r.get(0)
        })
        .unwrap();
    lab.worker
        .commands()
        .start("operator", &a, revision, "start-both", 100_000)
        .unwrap();
    lab.ready(&lab.c).await;
    until("both instances of A ready", || {
        lab.instance(&a, 0).0 == "ready" && lab.instance(&a, 1).0 == "ready"
    })
    .await;
    // A is the more recently used; C was never used (least recently used).
    for index in [0, 1] {
        let generation = lab.instance(&a, index).2;
        lab.worker.commands().note_activity(&a, generation);
    }
    let b = lab.deploy("sglang-b", SGLANG, |_| {});
    let port = lab.port(Duration::from_secs(5));
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(4_000),
        "no fairness window for a victim that is not its deployment's last"
    );
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(
        lab.state(&lab.c.deployment_id),
        "ready",
        "C's last instance kept"
    );
    let states = [lab.instance(&a, 0).0, lab.instance(&a, 1).0];
    assert_eq!(
        states.iter().filter(|s| *s == "ready").count(),
        1,
        "A keeps serving: {states:?}"
    );
    assert_eq!(states.iter().filter(|s| *s == "parked").count(), 1);
    let (_, planned) = &lab.switch_events()[0];
    assert_eq!(planned["victims"].as_array().unwrap().len(), 1);
    assert!(planned["victims"][0].as_str().unwrap().starts_with(&a));
    lab.worker.shutdown().await.unwrap();
}

// ADR 0013 §8 rule 8, SPEC §6.2: a restart-only victim is stopped with
// absence proof, never parked, and stays eligible for on-demand activation.
#[tokio::test]
async fn a_restart_only_victim_is_stopped_and_stays_on_demand_eligible() {
    let lab = lab(15, 50).await;
    let a = lab.deploy("restart-a", VLLM, |d| {
        d["residency"] = json!("restart_only")
    });
    let revision: i64 = lab
        .sql()
        .query_row("SELECT revision FROM deployments WHERE id=?1", [&a], |r| {
            r.get(0)
        })
        .unwrap();
    lab.worker
        .commands()
        .start("operator", &a, revision, "start-a", 100_000)
        .unwrap();
    until("A ready", || lab.state(&a) == "ready").await;
    let b = lab.c.deployment_id.clone();
    let port = lab.port(Duration::from_secs(5));
    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lab.state(&b), "ready");
    let (state, _, _, operator_stopped) = lab.instance(&a, 0);
    assert_eq!(state, "stopped");
    assert!(!operator_stopped, "an eviction is not an operator's stop");
    assert!(
        lab.operations(&a, "park").is_empty(),
        "restart-only never parks"
    );
    assert_eq!(lab.engine.calls(RuntimeAction::Park, &a), 0);
    assert!(
        !lab.worker
            .commands()
            .read(|store| store.is_admin_stopped(&a))
            .unwrap(),
        "A is still on-demand eligible"
    );
    lab.worker.shutdown().await.unwrap();
}

// T15 T16 (SPEC §6.3, owner decision Q5): a request for an explicitly stopped
// deployment is refused before any switch is planned: the Ready incumbent keeps
// serving and nothing is parked (M48 soak, 2026-09-24: the request parked A on
// the tight host and was refused afterwards).
#[tokio::test]
async fn a_request_for_an_operator_stopped_deployment_evicts_nothing() {
    let lab = lab(15, 50).await;
    lab.ready(&lab.a).await;
    let b = lab.c.deployment_id.clone();
    lab.worker
        .commands()
        .read(|store| store.set_admin_stopped(&b, true))
        .unwrap();
    let port = lab.port(Duration::from_secs(5));
    let refused = tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(refused, LifecycleFault::Stopped(ref m) if m.contains("stopped by an operator")),
        "{refused:?}"
    );
    assert_eq!(lab.state(&lab.a.deployment_id), "ready");
    assert!(lab.instance(&lab.a.deployment_id, 0).1, "A still admits");
    assert_eq!(
        lab.engine.calls(RuntimeAction::Park, &lab.a.deployment_id),
        0
    );
    assert!(lab.switch_events().is_empty(), "no switch was planned");
    assert_eq!(lab.state(&b), "stopped");
    lab.worker.shutdown().await.unwrap();
}

// SPEC §7 (T23): when even releasing every eligible READY instance cannot
// make B fit, nothing is released and the request gets a capacity refusal.
#[tokio::test]
async fn no_eviction_when_nothing_makes_room() {
    let lab = lab(15, 50).await;
    lab.ready(&lab.a).await;
    let big = lab.deploy("too-big", VLLM, |d| {
        d["resources"]["cold"]["allocations"][0]["bytes"] = json!("16GiB");
    });
    let port = lab.port(Duration::from_secs(5));
    let refused = tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&big))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(refused, LifecycleFault::Blocked(_)), "{refused:?}");
    assert_eq!(lab.state(&lab.a.deployment_id), "ready");
    assert!(lab.instance(&lab.a.deployment_id, 0).1, "A still admits");
    assert!(lab.switch_events().is_empty());
    lab.worker.shutdown().await.unwrap();
}

// T20 (ADR 0013 §8 rule 8, SPEC §6.2): a victim whose park is refused before
// any effect serves again; the next plan stops it with absence proof, as the
// idle policy does, and the waiting deployment is served.
#[tokio::test]
async fn a_refused_victim_park_falls_back_to_a_verified_stop() {
    let lab = lab(15, 50).await;
    lab.engine.refuse_park.store(true, Ordering::SeqCst);
    lab.ready(&lab.a).await;
    let b = lab.c.deployment_id.clone();
    let port = lab.port(Duration::from_secs(5));
    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(lab.state(&lab.a.deployment_id), "stopped");
    assert_eq!(lab.operations(&lab.a.deployment_id, "park"), ["failed"]);
    assert_eq!(
        lab.engine.calls(RuntimeAction::Park, &lab.a.deployment_id),
        1
    );
    let kinds = lab.kinds();
    assert_eq!(kinds.first().map(String::as_str), Some("switch_planned"));
    assert!(kinds.contains(&"switch_failed".to_string()));
    assert_eq!(kinds.last().map(String::as_str), Some("switch_completed"));
    lab.worker.shutdown().await.unwrap();
}

// W10 gap (c): status shows a switch in progress on the target's and the
// victim's deployment (target, host, victims, phase) while it drains, and no
// longer once the switch has ended (here it fails on its drain timeout).
// T17 T19 T33
#[tokio::test]
async fn status_shows_the_switch_in_progress_until_it_ends() {
    let lab = lab(15, 50).await;
    lab.ready(&lab.a).await;
    let (_, _, generation, _) = lab.instance(&lab.a.deployment_id, 0);
    let b = lab.c.deployment_id.clone();
    let port = Arc::new(lab.port(Duration::from_millis(600)));
    let lease = port
        .open_instance_lease(&lab.a.deployment_id, generation.unwrap(), 32)
        .await
        .unwrap()
        .unwrap();
    let switch = {
        let (port, b) = (port.clone(), b.clone());
        tokio::spawn(async move { port.activate_for_request(&b).await })
    };
    let snapshot = |deployment: &str| {
        let o = lab.owner.lock().unwrap();
        o.store()
            .snapshot()
            .unwrap()
            .deployments
            .into_iter()
            .find(|d| d.id == deployment)
            .unwrap()
    };
    until("the drain to show in status", || {
        snapshot(&b)
            .switch
            .is_some_and(|s| s.phase == "admission_closed")
    })
    .await;
    let target = serde_json::to_value(snapshot(&b).switch.unwrap()).unwrap();
    assert_eq!(target["role"], "target");
    assert_eq!(target["target"], json!(b));
    assert_eq!(target["host"], "lab");
    assert_eq!(
        target["victims"],
        json!([format!("{}/0", lab.a.deployment_id)])
    );
    assert_eq!(target["explicit"], false);
    let victim = serde_json::to_value(snapshot(&lab.a.deployment_id).switch.unwrap()).unwrap();
    assert_eq!(victim["role"], "victim");
    assert_eq!(victim["switch_id"], target["switch_id"]);
    let failed = tokio::time::timeout(Duration::from_secs(30), switch)
        .await
        .unwrap()
        .unwrap();
    assert!(failed.is_err(), "the drain timeout fails the switch");
    assert!(snapshot(&b).switch.is_none());
    assert!(snapshot(&lab.a.deployment_id).switch.is_none());
    port.close_request_lease(lease, crate::request_leases::LeaseEnd::Completed)
        .await
        .unwrap();
    lab.worker.shutdown().await.unwrap();
}

// Owner decision 2026-09-23 (solo first start) through W10: a request for an
// unmeasured model whose placeholder startup peak exceeds the managed limit
// empties the host by the ordinary switching rules (the READY victim is
// released on its own evidence), then starts alone, reserving the whole
// managed limit until Ready. The plan sees up front that the target needs an
// empty host, so its victims stop rather than park (a parked residual would
// keep the host occupied): the start arms at once, with no failed attempt, no
// retry cooldown and no parked reclaim in between (found live 2026-09-24, M53).
// T26 T27 T15 T20
#[tokio::test]
async fn a_request_empties_the_host_for_a_solo_first_start() {
    let lab = lab(36, 50).await;
    lab.ready(&lab.a).await;
    let big = lab.deploy("big", VLLM, |d| {
        d.as_object_mut().unwrap().remove("resources");
        d["engine_config"]["memory"] = json!({"kv_cache": "4GiB"});
    });
    {
        let o = lab.owner.lock().unwrap();
        o.store()
            .record_checkpoint_digest(
                o.session(),
                &big,
                1,
                "lab",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                20 << 30,
                1700,
            )
            .unwrap();
    }
    let revision: i64 = lab
        .sql()
        .query_row(
            "SELECT revision FROM deployments WHERE id=?1",
            [&big],
            |r| r.get(0),
        )
        .unwrap();
    // An explicit start without --evict is refused beside A.
    assert!(matches!(
        lab.worker
            .commands()
            .start("operator", &big, revision, "start-big", 100_000),
        Err(crate::coordinator::CoordinatorCommandError::Lifecycle(
            mllm_store::lifecycle::LifecycleError::StartupRequiresEmptyHost
        ))
    ));
    let port = lab.port(Duration::from_secs(5));
    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&big))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lab.state(&big), "ready");
    assert_ne!(lab.state(&lab.a.deployment_id), "ready");
    let (_, planned) = &lab.switch_events()[0];
    assert_eq!(
        planned["victims"],
        json!([format!("{}/0", lab.a.deployment_id)])
    );
    // One switch made the room, and the parked victim's residual charge was
    // reclaimed too: nothing else holds a reservation on the host.
    let completed: i64 = lab
        .sql()
        .query_row(
            "SELECT COUNT(*) FROM management_events WHERE kind='switch_completed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(completed, 1);
    let owners: Vec<String> = {
        let o = lab.owner.lock().unwrap();
        o.store()
            .resource_snapshot()
            .unwrap()
            .owners
            .into_keys()
            .collect()
    };
    assert_eq!(owners, vec![big.clone()]);
    assert!(
        lab.operations(&lab.a.deployment_id, "park").is_empty(),
        "a solo first start's victims stop, never park"
    );
    assert_eq!(lab.state(&lab.a.deployment_id), "stopped");
    let retried: Vec<String> = {
        let sql = lab.sql();
        let mut stmt = sql
            .prepare(
                "SELECT j.state FROM journal_entries j JOIN operations o ON o.id=j.operation_id
                  WHERE o.deployment_id=?1 AND j.state IN ('attempt_failed','start_deferred','given_up')",
            )
            .unwrap();
        stmt.query_map([&big], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert!(retried.is_empty(), "the start armed at once: {retried:?}");
    lab.worker.shutdown().await.unwrap();
}

/// Operation state and whether the instance still holds its runtime binding.
fn stop_progress(lab: &Lab, operation: &str, deployment: &str) -> (String, bool) {
    lab.sql()
        .query_row(
            "SELECT (SELECT state FROM operations WHERE id=?1),
                    EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?2 AND state!='released')",
            rusqlite::params![operation, deployment],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn leases(lab: &Lab, deployment: &str) -> i64 {
    lab.sql()
        .query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
            [deployment],
            |r| r.get(0),
        )
        .unwrap()
}

async fn until_succeeded(lab: &Lab, operation: &str, deployment: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while stop_progress(lab, operation, deployment).0 != "succeeded" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Stop never completed");
}

// T17 T10 (SPEC §6.3 "drain, and terminate"; live M65, M66): a Stop accepted
// while a request is in flight closes admission and waits for that request to
// complete before it terminates the engine. The request is not cut.
#[tokio::test]
async fn a_stop_drains_an_in_flight_request_before_it_terminates() {
    let lab = lab_options(
        15,
        50,
        None,
        CoordinatorOptions {
            stop_drain_timeout: Duration::from_secs(20),
            ..Default::default()
        },
    )
    .await;
    lab.ready(&lab.a).await;
    let id = lab.a.deployment_id.clone();
    let (_, _, generation, _) = lab.instance(&id, 0);
    let port = lab.port(Duration::from_millis(300));
    let lease = port
        .open_instance_lease(&id, generation.unwrap(), 32)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while leases(&lab, &id) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the lease never reached the store");
    let stop = lab
        .worker
        .commands()
        .administrative_stop("operator", &id, lab.a.revision, "stop", 100_000)
        .unwrap();
    // Admission is closed at once; the engine keeps serving the request.
    let (_, dispatch, _, _) = lab.instance(&id, 0);
    assert!(!dispatch, "admission closed with the Stop");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (state, held) = stop_progress(&lab, stop.operation_id(), &id);
    assert_ne!(
        state, "succeeded",
        "terminated while a request was in flight"
    );
    assert!(
        held,
        "the runtime was released while a request was in flight"
    );
    port.close_request_lease(lease, crate::request_leases::LeaseEnd::Completed)
        .await
        .unwrap();
    until_succeeded(&lab, stop.operation_id(), &id).await;
    assert_eq!(leases(&lab, &id), 0);
    assert_eq!(lab.state(&id), "stopped");
    lab.worker.shutdown().await.unwrap();
}

// T17 T10 (SPEC §6.3, §10): a request still in flight when the Stop's drain
// bound passes does not hold the Stop for ever: the explicit Stop terminates
// after the bound. The request's lease is settled by the cleanup's evidence
// that the engine is gone, never before it.
#[tokio::test]
async fn a_stop_terminates_after_its_drain_bound_and_the_lease_settles_on_evidence() {
    let lab = lab_options(
        15,
        50,
        None,
        CoordinatorOptions {
            stop_drain_timeout: Duration::from_millis(300),
            ..Default::default()
        },
    )
    .await;
    lab.ready(&lab.a).await;
    let id = lab.a.deployment_id.clone();
    let (_, _, generation, _) = lab.instance(&id, 0);
    let port = lab.port(Duration::from_millis(300));
    let lease = port
        .open_instance_lease(&id, generation.unwrap(), 32)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while leases(&lab, &id) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the lease never reached the store");
    let started = std::time::Instant::now();
    let stop = lab
        .worker
        .commands()
        .administrative_stop("operator", &id, lab.a.revision, "stop", 100_000)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (state, held) = stop_progress(&lab, stop.operation_id(), &id);
    assert_ne!(state, "succeeded", "terminated inside the drain bound");
    assert!(held, "released inside the drain bound");
    assert_eq!(leases(&lab, &id), 1, "the lease is kept while draining");
    until_succeeded(&lab, stop.operation_id(), &id).await;
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "terminated before the drain bound"
    );
    assert_eq!(
        leases(&lab, &id),
        0,
        "the cleanup's gone evidence settles the lease"
    );
    // The router's own close afterwards changes nothing.
    let _ = port
        .close_request_lease(lease, crate::request_leases::LeaseEnd::Completed)
        .await;
    assert_eq!(leases(&lab, &id), 0);
    lab.worker.shutdown().await.unwrap();
}

/// Review finding (ADR 0011 decision 4, ADR 0013 §6): a launch that failed
/// after arm and was settled on its host's evidence closed the whole
/// deployment's admission. Only its own instance closes now, keyed by
/// (deployment, instance index); instance 0 and the deployment stay admitting.
// T16 T30 T32
#[tokio::test]
async fn a_settled_failed_launch_closes_only_its_own_instance() {
    let failing = Gate::new(false);
    *failing.failure.lock().unwrap() = Some("engine exited during load".into());
    failing.release.add_permits(1);
    let lab = lab_bindings(64, 2_000, None, CoordinatorOptions::default(), {
        let failing = failing.clone();
        move |scripted, _clock| {
            Arc::new(move |work: &InitializeWork| {
                let engine: Arc<dyn EngineAdapter> = if work.instance_index() == 1 {
                    failing.clone()
                } else {
                    scripted.clone()
                };
                Ok(ExecutionBinding::remote(
                    engine,
                    Arc::new(|_| {
                        Box::pin(async { Err(CoordinatorError::Service("unused".into())) })
                    }),
                )
                .with_settlement(Arc::new(|context: SettlementContext| {
                    Box::pin(async move {
                        Ok(CleanupEvidence {
                            binding_id: context.binding_id,
                            incarnation: context.incarnation,
                            identities: context.identities,
                            observed_at_ms: 1900,
                            receipt: "scripted host terminated the launch and observed it gone"
                                .into(),
                        })
                    })
                })))
            })
        }
    })
    .await;
    let id = lab.deploy("pair", VLLM, |d| d["instances"] = json!(2));
    let revision: i64 = lab
        .sql()
        .query_row("SELECT revision FROM deployments WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    let first = lab
        .worker
        .commands()
        .start_instance("owner", &id, 0, revision, "start-0", 100_000)
        .unwrap();
    until("instance 0 ready", || lab.instance(&id, 0).0 == "ready").await;
    drop(first);
    lab.worker
        .commands()
        .start_instance("owner", &id, 1, revision, "start-1", 100_000)
        .unwrap();
    let admitting = |index: u32| -> bool {
        lab.sql()
            .query_row(
                "SELECT admission_enabled=1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                rusqlite::params![&id, index],
                |r| r.get(0),
            )
            .unwrap()
    };
    until("instance 1 closes its own admission", || !admitting(1)).await;
    assert!(admitting(0), "the sibling instance keeps being admitted");
    let deployment_admitting: bool = lab
        .sql()
        .query_row(
            "SELECT admission_enabled=1 FROM deployments WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        deployment_admitting,
        "the deployment itself stays admitting"
    );
    assert_eq!(
        *failing.calls.lock().unwrap(),
        vec![RuntimeAction::Initialize]
    );
    assert_eq!(lab.worker.status(), WorkerStatus::Running);
    lab.worker.shutdown().await.unwrap();
}
