//! W5: park, wake, preinitialize and idle policy driven by the real worker
//! against scripted engines that keep the W4 execution contract. A remote
//! binding answers a Restore with all four restore facts after its own fresh
//! probe; an embedded vLLM-shaped one answers each persisted step separately.
//! CPU only: nothing here qualifies an engine or a host.
use super::*;
use capyctl_domain::completion::{EffectObservation, ExecutionIdentities, Milestone};
use capyctl_store::ordinary_lifecycle::park::{IdlePolicy, WakeScope};
use std::sync::atomic::AtomicI64;

/// How the scripted engine answers one residency action.
#[derive(Clone, Debug)]
enum Answer {
    Proven,
    Refused,
    Uncertain,
}

/// A remote (compound restore) or embedded (stepwise restore) engine.
struct Residency {
    gate: Arc<Gate>,
    compound: bool,
    clock: Arc<AtomicI64>,
    park: Mutex<Answer>,
    restore: Mutex<Answer>,
    /// (action, binding, generation, step id) of every call after Initialize.
    calls: Mutex<Vec<(RuntimeAction, String, i64, String)>>,
}

impl Residency {
    fn new(compound: bool, clock: Arc<AtomicI64>) -> Arc<Self> {
        let gate = Gate::new(false);
        gate.release.add_permits(64);
        Arc::new(Self {
            gate,
            compound,
            clock,
            park: Mutex::new(Answer::Proven),
            restore: Mutex::new(Answer::Proven),
            calls: Mutex::new(vec![]),
        })
    }
    fn actions(&self) -> Vec<RuntimeAction> {
        self.calls.lock().unwrap().iter().map(|c| c.0).collect()
    }
}

#[async_trait::async_trait]
impl EngineAdapter for Residency {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        let c = &command.context;
        if command.action == RuntimeAction::Initialize {
            return self.gate.execute_persisted(command).await;
        }
        self.calls.lock().unwrap().push((
            command.action,
            c.binding_id.clone(),
            c.token.generation,
            c.token.step_id.clone(),
        ));
        let ExecutionIdentities::Retained(identities) = &c.identities else {
            return Err(RuntimeError::StaleRevision);
        };
        let answer = match command.action {
            RuntimeAction::Park => self.park.lock().unwrap().clone(),
            RuntimeAction::Restore => self.restore.lock().unwrap().clone(),
            RuntimeAction::ReloadWeights
            | RuntimeAction::InvalidateCache
            | RuntimeAction::Probe
                if !self.compound =>
            {
                Answer::Proven
            }
            _ => return Err(RuntimeError::Unsupported),
        };
        let facts = match (command.action, self.compound) {
            (RuntimeAction::Park, _) => vec![Milestone::MemoryReleased],
            (RuntimeAction::Restore, true) => vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable,
            ],
            (RuntimeAction::Restore, false) => vec![Milestone::AllocationsRestored],
            (RuntimeAction::ReloadWeights, _) => vec![Milestone::WeightsUsable],
            (RuntimeAction::InvalidateCache, _) => vec![Milestone::CacheValid],
            (RuntimeAction::Probe, _) => vec![Milestone::ModelUsable],
            _ => return Err(RuntimeError::Unsupported),
        };
        match answer {
            Answer::Refused => Err(RuntimeError::Unsupported),
            Answer::Uncertain => Err(RuntimeError::Uncertain("scripted: outcome lost".into())),
            Answer::Proven => Ok(EffectObservation {
                token: c.token.clone(),
                binding_id: c.binding_id.clone(),
                incarnation: c.incarnation.clone(),
                identities: identities.clone(),
                observed_at_ms: self.clock.load(Ordering::SeqCst),
                receipt: format!("scripted {:?}", command.action),
                facts,
            }),
        }
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

/// Observations sampled at the test clock, so they stay fresh as it moves.
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

/// A worker whose every binding is driven through `engine`, as a remote host
/// binding is: no local process tools, cleanup proven by the "host".
fn residency_worker(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    engine: Arc<Residency>,
    clock: Arc<AtomicI64>,
    idle: IdlePolicy,
) -> OwnedCoordinator {
    let now = clock.clone();
    OwnedCoordinator::spawn_with_execution_bindings(
        owner,
        Arc::new(Fresh {
            observations,
            clock: clock.clone(),
        }),
        Arc::new(move || Ok(now.load(Ordering::SeqCst))),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            idle,
            ..Default::default()
        },
        Arc::new(move |_: &InitializeWork| {
            let observed = clock.clone();
            Ok(ExecutionBinding::remote(
                engine.clone(),
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
        }),
    )
    .unwrap()
}

/// (observed state, dispatch open, generation, host) of instance 0.
fn instance(dir: &tempfile::TempDir, deployment: &str) -> (String, bool, i64, Option<String>) {
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.query_row(
        "SELECT observed_state,dispatch_enabled=1,generation,host_id FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
        [deployment],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

fn operation(owner: &SharedCoordinatorState, id: &str) -> (String, Option<String>) {
    let o = owner.lock().unwrap();
    let row = o.store().get_operation(id).unwrap().unwrap();
    (
        match row.state {
            capyctl_store::deployments::OpState::Pending => "pending",
            capyctl_store::deployments::OpState::Running => "running",
            capyctl_store::deployments::OpState::Succeeded => "succeeded",
            capyctl_store::deployments::OpState::Failed => "failed",
        }
        .to_string(),
        row.error_code,
    )
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

fn phase(owner: &SharedCoordinatorState, deployment: &str) -> Option<ResourcePhase> {
    let o = owner.lock().unwrap();
    o.store()
        .resource_snapshot()
        .unwrap()
        .owners
        .get(deployment)
        .map(|f| f.phase)
}

async fn ready(worker: &OwnedCoordinator, fence: &DeploymentFence) {
    let start = worker.start(fence, 100_000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(30)).await.unwrap(),
        InitializeStatus::Completed
    );
}

// T16 T15 (ADR 0013 §4 sticky parked placement): park, then two simultaneous
// on-demand requests join one restore of the same launch on the same host,
// under the same generation, and dispatch reopens only on the restore's
// usable-model evidence.
#[tokio::test]
async fn a_remote_park_and_on_demand_wake_restore_the_same_launch_in_place() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Residency::new(true, clock.clone());
    let w = residency_worker(
        owner.clone(),
        observations,
        engine.clone(),
        clock,
        IdlePolicy::default(),
    );
    ready(&w, &fence).await;
    let (_, open, generation, host) = instance(&dir, &fence.deployment_id);
    assert!(open);
    let park = w
        .commands()
        .park(
            "operator",
            &fence.deployment_id,
            fence.revision,
            "park",
            100_000,
        )
        .unwrap();
    until("parked", || {
        instance(&dir, &fence.deployment_id).0 == "parked"
    })
    .await;
    assert_eq!(operation(&owner, &park.operation_id).0, "succeeded");
    assert_eq!(
        phase(&owner, &fence.deployment_id),
        Some(ResourcePhase::Parked)
    );
    assert!(
        !instance(&dir, &fence.deployment_id).1,
        "a parked instance never dispatches"
    );

    let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    use crate::port::LifecyclePort;
    let (first, second) = tokio::join!(
        port.auto_activate(&fence.deployment_id),
        port.auto_activate(&fence.deployment_id)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.operation_id, second.operation_id, "T15: one restore");
    assert_eq!(
        port.wait_terminal(&first).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    let (state, open, now_generation, now_host) = instance(&dir, &fence.deployment_id);
    assert_eq!((state.as_str(), open), ("ready", true));
    assert_eq!(now_generation, generation, "the same incarnation woke");
    assert_eq!(now_host, host, "a parked instance wakes on its own host");
    assert_eq!(
        phase(&owner, &fence.deployment_id),
        Some(ResourcePhase::Ready)
    );
    let calls = engine.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|c| c.0).collect::<Vec<_>>(),
        [RuntimeAction::Park, RuntimeAction::Restore]
    );
    assert!(calls.iter().all(|c| c.1 == calls[0].1 && c.2 == generation));
    w.shutdown().await.unwrap();
}

// T20 (W4 hand-off): a wake refused before any effect leaves the instance
// parked with its parked reservation; nothing is retried by itself.
#[tokio::test]
async fn a_refused_wake_leaves_the_instance_parked() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Residency::new(true, clock.clone());
    *engine.restore.lock().unwrap() = Answer::Refused;
    let w = residency_worker(
        owner.clone(),
        observations,
        engine.clone(),
        clock,
        IdlePolicy::default(),
    );
    ready(&w, &fence).await;
    w.commands()
        .park(
            "operator",
            &fence.deployment_id,
            fence.revision,
            "park",
            100_000,
        )
        .unwrap();
    until("parked", || {
        instance(&dir, &fence.deployment_id).0 == "parked"
    })
    .await;
    let wake = w
        .commands()
        .wake(
            "router",
            &fence.deployment_id,
            WakeScope::OnDemand,
            fence.revision,
            "wake",
            100_000,
        )
        .unwrap()
        .unwrap();
    until("refused", || {
        operation(&owner, &wake.operation_id).0 == "failed"
    })
    .await;
    assert_eq!(
        operation(&owner, &wake.operation_id).1.as_deref(),
        Some("restore_refused")
    );
    assert_eq!(instance(&dir, &fence.deployment_id).0, "parked");
    assert_eq!(
        phase(&owner, &fence.deployment_id),
        Some(ResourcePhase::Parked)
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        engine.actions(),
        [RuntimeAction::Park, RuntimeAction::Restore]
    );
    w.shutdown().await.unwrap();
}

// T20: a park refused before any effect settles at once; the launch serves
// again and its reservation returns to Ready.
#[tokio::test]
async fn a_refused_park_returns_the_launch_to_service() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Residency::new(true, clock.clone());
    *engine.park.lock().unwrap() = Answer::Refused;
    let w = residency_worker(
        owner.clone(),
        observations,
        engine.clone(),
        clock,
        IdlePolicy::default(),
    );
    ready(&w, &fence).await;
    let park = w
        .commands()
        .park(
            "operator",
            &fence.deployment_id,
            fence.revision,
            "park",
            100_000,
        )
        .unwrap();
    until("refused", || {
        operation(&owner, &park.operation_id).0 == "failed"
    })
    .await;
    assert_eq!(
        operation(&owner, &park.operation_id).1.as_deref(),
        Some("park_refused")
    );
    let (state, open, _, _) = instance(&dir, &fence.deployment_id);
    assert_eq!((state.as_str(), open), ("ready", true));
    assert_eq!(
        phase(&owner, &fence.deployment_id),
        Some(ResourcePhase::Ready)
    );
    w.shutdown().await.unwrap();
}

// T20, AGENTS.md: an uncertain park keeps the peak reservation and the claim,
// is never sent again, and only an operator stop's gone evidence releases it.
#[tokio::test]
async fn an_uncertain_park_keeps_accounting_until_a_stop_proves_it_gone() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Residency::new(true, clock.clone());
    *engine.park.lock().unwrap() = Answer::Uncertain;
    let w = residency_worker(
        owner.clone(),
        observations,
        engine.clone(),
        clock,
        IdlePolicy::default(),
    );
    ready(&w, &fence).await;
    let park = w
        .commands()
        .park(
            "operator",
            &fence.deployment_id,
            fence.revision,
            "park",
            100_000,
        )
        .unwrap();
    let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    use crate::port::LifecyclePort;
    let handle = crate::operations::OperationHandle {
        operation_id: capyctl_domain::OperationId(park.operation_id.clone()),
        deployment_id: fence.deployment_id.clone(),
    };
    assert!(matches!(
        port.wait_terminal(&handle).await,
        Err(crate::fault::LifecycleFault::Uncertain(_))
    ));
    assert_eq!(
        phase(&owner, &fence.deployment_id),
        Some(ResourcePhase::Parking)
    );
    assert!(!instance(&dir, &fence.deployment_id).1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(engine.actions(), [RuntimeAction::Park], "never repeated");
    let stop = w
        .commands()
        .administrative_stop(
            "operator",
            &fence.deployment_id,
            fence.revision,
            "stop",
            100_000,
        )
        .unwrap();
    until("released", || phase(&owner, &fence.deployment_id).is_none()).await;
    assert_eq!(instance(&dir, &fence.deployment_id).0, "stopped");
    assert_eq!(operation(&owner, stop.operation_id()).0, "succeeded");
    assert_eq!(
        operation(&owner, &park.operation_id),
        ("failed".into(), Some("resolved_by_owned_cleanup".into()))
    );
    w.shutdown().await.unwrap();
}

// SPEC §9.1 (embedded vLLM): a restore runs weight wake, reload, KV wake and
// a fresh model probe, one persisted step each, before dispatch reopens.
#[tokio::test]
async fn an_embedded_restore_runs_each_step_and_probes_before_serving() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Residency::new(false, clock.clone());
    let w = residency_worker(
        owner.clone(),
        observations,
        engine.clone(),
        clock,
        IdlePolicy::default(),
    );
    ready(&w, &fence).await;
    w.commands()
        .park(
            "operator",
            &fence.deployment_id,
            fence.revision,
            "park",
            100_000,
        )
        .unwrap();
    until("parked", || {
        instance(&dir, &fence.deployment_id).0 == "parked"
    })
    .await;
    w.commands()
        .wake(
            "operator",
            &fence.deployment_id,
            WakeScope::All,
            fence.revision,
            "wake",
            100_000,
        )
        .unwrap()
        .unwrap();
    until("ready", || {
        instance(&dir, &fence.deployment_id).0 == "ready"
    })
    .await;
    assert!(instance(&dir, &fence.deployment_id).1);
    assert_eq!(
        engine.actions(),
        [
            RuntimeAction::Park,
            RuntimeAction::Restore,
            RuntimeAction::ReloadWeights,
            RuntimeAction::InvalidateCache,
            RuntimeAction::Probe
        ]
    );
    // Each persisted step is its own call, never a repeat of another.
    let steps: std::collections::BTreeSet<String> = engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.3.clone())
        .collect();
    assert_eq!(steps.len(), 5);
    w.shutdown().await.unwrap();
}

// T26 (SPEC §6.5): preinitialize runs start, verify (Ready) and park, and
// succeeds only when the instance is parked.
#[tokio::test]
async fn preinitialize_starts_verifies_and_parks() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Residency::new(true, clock.clone());
    let w = residency_worker(
        owner.clone(),
        observations,
        engine.clone(),
        clock,
        IdlePolicy::default(),
    );
    let receipt = w
        .commands()
        .preinitialize(
            "operator",
            &fence.deployment_id,
            fence.revision,
            "pre",
            100_000,
        )
        .unwrap();
    until("preinitialized", || {
        operation(&owner, &receipt.operation_id).0 == "succeeded"
    })
    .await;
    assert_eq!(instance(&dir, &fence.deployment_id).0, "parked");
    assert_eq!(
        *engine.gate.calls.lock().unwrap(),
        [RuntimeAction::Initialize]
    );
    assert_eq!(engine.actions(), [RuntimeAction::Park]);
    w.shutdown().await.unwrap();
}

// T33 (SPEC §6.5): the controller owns idle policy. Ready-idle parks,
// parked-idle stops, and both leave on-demand activation enabled.
#[tokio::test]
async fn idle_timers_park_then_stop_and_keep_on_demand_activation() {
    let (dir, owner, fence, observations) = setup().await;
    let clock = Arc::new(AtomicI64::new(1900));
    let engine = Residency::new(true, clock.clone());
    let w = residency_worker(
        owner.clone(),
        observations,
        engine.clone(),
        clock.clone(),
        IdlePolicy {
            ready_idle_ms: Some(10_000),
            parked_idle_ms: Some(20_000),
        },
    );
    ready(&w, &fence).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(
        instance(&dir, &fence.deployment_id).0,
        "ready",
        "not idle yet"
    );
    clock.store(13_000, Ordering::SeqCst);
    until("idle park", || {
        instance(&dir, &fence.deployment_id).0 == "parked"
    })
    .await;
    clock.store(40_000, Ordering::SeqCst);
    until("idle stop", || {
        phase(&owner, &fence.deployment_id).is_none()
    })
    .await;
    assert_eq!(instance(&dir, &fence.deployment_id).0, "stopped");
    let admin: bool = {
        let o = owner.lock().unwrap();
        o.store().is_admin_stopped(&fence.deployment_id).unwrap()
    };
    assert!(!admin, "an idle stop leaves on-demand activation enabled");
    w.shutdown().await.unwrap();
}
