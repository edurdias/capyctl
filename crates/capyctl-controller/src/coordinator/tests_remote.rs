//! G1 (U5 live): remote launches that fail or go uncertain after arm.
//!
//! A remote driver has no local process tools, so before this the worker paused
//! every failed remote launch uncertain and nothing could resolve it: Stop was
//! refused because no association existed, and a controller restart stranded it.
//! These drive the real worker against a scripted host that answers the
//! settlement Terminate, so nothing here qualifies a native engine or a host.
use super::*;
use capyctl_domain::completion::ProcessIdentity;
use std::sync::atomic::AtomicUsize;

/// The host side of a settlement: whether it can be reached, and what it saw.
struct ScriptedHost {
    /// Whether the settlement Terminate is answered.
    reachable: AtomicBool,
    /// Whether an operator Stop's cleanup Terminate is answered.
    stoppable: AtomicBool,
    calls: AtomicUsize,
    contexts: Mutex<Vec<SettlementContext>>,
    cleanups: Mutex<Vec<CleanupExecutionContext>>,
}
impl ScriptedHost {
    fn new(reachable: bool) -> Arc<Self> {
        Arc::new(Self {
            reachable: AtomicBool::new(reachable),
            stoppable: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            contexts: Mutex::new(vec![]),
            cleanups: Mutex::new(vec![]),
        })
    }
    /// The binding a remote coordinator resolves: its engine, its cleanup
    /// Terminate and its failed-launch settlement Terminate.
    fn binding(self: &Arc<Self>, gate: Arc<dyn EngineAdapter>) -> ExecutionBinding {
        let (cleanup, settle) = (self.clone(), self.clone());
        ExecutionBinding::remote(
            gate,
            Arc::new(move |context: CleanupExecutionContext| {
                let host = cleanup.clone();
                Box::pin(async move {
                    host.cleanups.lock().unwrap().push(context.clone());
                    if !host.stoppable.load(Ordering::SeqCst) {
                        return Err(CoordinatorError::Service("host unreachable".into()));
                    }
                    // Observed when the Terminate was issued (1900 under
                    // the fixed test clock).
                    let observed_at_ms = context.issued_at_ms;
                    Ok(CleanupEvidence {
                        binding_id: context.binding_id,
                        incarnation: context.incarnation,
                        identities: context.identities,
                        observed_at_ms,
                        receipt: "scripted host observed the owned group gone".into(),
                    })
                })
            }),
        )
        .with_settlement(Arc::new(move |context: SettlementContext| {
            let host = settle.clone();
            Box::pin(async move {
                host.calls.fetch_add(1, Ordering::SeqCst);
                host.contexts.lock().unwrap().push(context.clone());
                if !host.reachable.load(Ordering::SeqCst) {
                    return Err(CoordinatorError::Service("host unreachable".into()));
                }
                Ok(CleanupEvidence {
                    binding_id: context.binding_id,
                    incarnation: context.incarnation,
                    identities: context.identities,
                    observed_at_ms: 1900,
                    receipt: "scripted host terminated the launch and observed it gone".into(),
                })
            })
        }))
    }
}

/// A remote Initialize whose host journaled the API process (the controller's
/// ownership observer recorded it) and whose outcome was then lost.
fn lost_remote_launch(owner: &SharedCoordinatorState) -> Arc<Gate> {
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.api_only.store(true, Ordering::SeqCst);
    *gate.failure.lock().unwrap() = Some("remote initialize remains unresolved".into());
    gate.release.add_permits(1);
    gate
}

fn remote_worker(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    host: Arc<ScriptedHost>,
    gate: Arc<dyn EngineAdapter>,
) -> OwnedCoordinator {
    OwnedCoordinator::spawn_with_execution_bindings(
        owner,
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
        Arc::new(move |_: &InitializeWork| Ok(host.binding(gate.clone()))),
    )
    .unwrap()
}

/// The retained binding's state; a released binding is no longer retained.
fn binding_state(owner: &SharedCoordinatorState, fence: &DeploymentFence) -> String {
    let o = owner.lock().unwrap();
    o.store()
        .runtime_binding(&fence.deployment_id)
        .unwrap()
        .map(|b| b.state)
        .unwrap_or_else(|| "released".into())
}

fn owns(owner: &SharedCoordinatorState, fence: &DeploymentFence) -> bool {
    let o = owner.lock().unwrap();
    o.store()
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(&fence.deployment_id)
}

fn recorded(owner: &SharedCoordinatorState, fence: &DeploymentFence) -> Vec<ProcessIdentity> {
    let o = owner.lock().unwrap();
    let binding = o
        .store()
        .runtime_binding(&fence.deployment_id)
        .unwrap()
        .unwrap();
    o.store().runtime_binding_identities(&binding.id).unwrap()
}

/// G1: a failed remote launch is settled by the host's authenticated evidence
/// that the owned launch is terminated and gone, exactly as a native launch is
/// settled by local proof: released in one transaction, admission closed.
// T30 T32 T20
#[tokio::test]
async fn a_failed_remote_launch_is_settled_on_authenticated_host_evidence() {
    let (_dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let gate = lost_remote_launch(&owner);
    let w = remote_worker(owner.clone(), observations, host.clone(), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Closed
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    let context = host.contexts.lock().unwrap()[0].clone();
    assert_eq!(context.step_id, start.step_id());
    assert_eq!(context.identities.len(), 1, "the recorded API process");
    assert_eq!(context.identities[0].role, "api");
    assert_eq!(binding_state(&owner, &fence), "released");
    assert!(!owns(&owner, &fence));
    running(&w).await;
    w.shutdown().await.unwrap();
}

/// G1: an unreachable host proves nothing, so the launch stays uncertain with
/// its reservation retained, and the worker keeps asking. When the host
/// answers, the launch is settled without any operator action.
// T32 T20
#[tokio::test]
async fn an_unreachable_host_keeps_the_launch_uncertain_until_it_answers() {
    let (_dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(false);
    let gate = lost_remote_launch(&owner);
    let w = remote_worker(owner.clone(), observations, host.clone(), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert_eq!(
        start.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Uncertain
    );
    assert_eq!(binding_state(&owner, &fence), "uncertain");
    assert!(owns(&owner, &fence), "nothing is released without evidence");
    host.reachable.store(true, Ordering::SeqCst);
    running(&w).await;
    assert!(host.calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(binding_state(&owner, &fence), "released");
    assert!(!owns(&owner, &fence));
    w.shutdown().await.unwrap();
}

/// G1 (U5 live): the operator's Stop of an uncertain unassociated remote launch
/// was refused as `Lifecycle state does not permit this action`. It is accepted,
/// drives the host Terminate for exactly the recorded identities, and completes
/// only on the host's gone evidence.
// T10 T32 T34
#[tokio::test]
async fn an_operator_stop_settles_an_uncertain_unassociated_remote_launch() {
    let (_dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(false);
    let gate = lost_remote_launch(&owner);
    let w = remote_worker(owner.clone(), observations, host.clone(), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    let api = recorded(&owner, &fence);
    assert_eq!(api.len(), 1);
    // The paused settlement keeps failing; only the operator's Stop reaches it.
    let stop = w.stop("owner", &fence, "operator-stop", 10000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        capyctl_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Completed
    );
    let cleanups = host.cleanups.lock().unwrap().clone();
    // The Stop was armed with exactly the recorded identity, nothing more.
    assert_eq!(cleanups.len(), 1);
    assert_eq!(cleanups[0].identities, api);
    assert_eq!(binding_state(&owner, &fence), "released");
    assert!(!owns(&owner, &fence));
    drop(start);
    running(&w).await;
    w.shutdown().await.unwrap();
}

fn make_remote(dir: &tempfile::TempDir, owner: &SharedCoordinatorState, fence: &DeploymentFence) {
    let binding = {
        let o = owner.lock().unwrap();
        o.store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .id
    };
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "INSERT OR IGNORE INTO enrolled_hosts(host_id,host_name,key_digest) VALUES('lab','lab','digest')",
        [],
    )
    .unwrap();
    sql.execute(
        "INSERT INTO remote_binding_ingress VALUES(?1,'lab','http://100.64.0.1:9443')",
        [binding],
    )
    .unwrap();
}

/// G1 durability (T33): a controller restart retired the session that owned an
/// uncertain remote launch. The restarted remote worker adopts it, keeps its
/// reservation until the host answers, and settles it on the host's evidence.
// T33 T32 T34
#[tokio::test]
async fn a_restarted_controller_adopts_and_settles_an_uncertain_remote_launch() {
    let (dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(false);
    let gate = lost_remote_launch(&owner);
    let w = remote_worker(owner.clone(), observations.clone(), host.clone(), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    make_remote(&dir, &owner, &fence);
    drop(start);
    w.shutdown().await.unwrap();
    drop(owner);

    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    assert!(owns(&owner, &fence), "restart releases nothing");
    let restarted = ScriptedHost::new(false);
    let w = remote_worker(
        owner.clone(),
        observations,
        restarted.clone(),
        Gate::new(false),
    );
    // The adopted launch is still uncertain, and the worker pauses on it.
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert!(restarted.calls.load(Ordering::SeqCst) >= 1);
    assert!(owns(&owner, &fence));
    let context = restarted.contexts.lock().unwrap()[0].clone();
    assert_eq!(context.identities.len(), 1);
    restarted.reachable.store(true, Ordering::SeqCst);
    running(&w).await;
    assert_eq!(binding_state(&owner, &fence), "released");
    assert!(!owns(&owner, &fence));
    w.shutdown().await.unwrap();
}

/// Review finding (ADR 0015 invariant 5, SPEC §13.2): restart adoption paused
/// only the first adopted launch still uncertain. Every other one was never
/// asked again, so once the first settled, starts were re-admitted while those
/// launches stayed unproven. Every uncertain adopted launch now pauses and is
/// retried on its own until its host's evidence settles it.
// T33 T32 T20
#[tokio::test]
async fn a_restart_pauses_and_retries_every_adopted_uncertain_launch() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let host = ScriptedHost::new(false);
    let gate = lost_remote_launch(&owner);
    // Hold both launches until each has armed, so the first one's pause does
    // not refuse the second start.
    gate.release.acquire().await.unwrap().forget();
    let w = remote_worker(
        owner.clone(),
        observations.clone(),
        host.clone(),
        gate.clone(),
    );
    let first = w.start(&fence, 10000).unwrap();
    let second = w.start(&other, 10000).unwrap();
    gate.entered().await;
    gate.entered().await;
    gate.release.add_permits(2);
    for start in [&first, &second] {
        assert_eq!(
            start.wait(Duration::from_secs(10)).await.unwrap(),
            InitializeStatus::Uncertain
        );
    }
    make_remote(&dir, &owner, &fence);
    make_remote(&dir, &owner, &other);
    drop((first, second));
    w.shutdown().await.unwrap();
    // The gate's association holds the owned state too.
    drop(gate);
    drop(owner);

    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    let restarted = ScriptedHost::new(false);
    let w = remote_worker(
        owner.clone(),
        observations,
        restarted.clone(),
        Gate::new(false),
    );
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    let asked = |deployment: &str| {
        restarted
            .contexts
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.fence.deployment_id == deployment)
            .count()
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while asked(&fence.deployment_id) < 3 || asked(&other.deployment_id) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("every adopted uncertain launch is asked again, not only the first");
    assert!(owns(&owner, &fence) && owns(&owner, &other));
    restarted.reachable.store(true, Ordering::SeqCst);
    running(&w).await;
    assert_eq!(binding_state(&owner, &fence), "released");
    assert_eq!(binding_state(&owner, &other), "released");
    w.shutdown().await.unwrap();
}

/// Review finding (ADR 0015 invariant 6): a paused launch's settlement task
/// did not watch the stop signal, so a host that never answered held the
/// worker's shutdown for the settlement's whole protocol bound (30 s by
/// default). Shutdown now stops it at once and leaves the launch uncertain,
/// its reservation charged, for a restarted worker to adopt.
// T33 T32
#[tokio::test]
async fn shutdown_does_not_wait_for_a_settlement_the_host_never_answers() {
    let (_dir, owner, fence, observations) = setup().await;
    let gate = lost_remote_launch(&owner);
    let calls = Arc::new(AtomicUsize::new(0));
    let asked = calls.clone();
    let w = OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
        Arc::new(move |_: &InitializeWork| {
            let asked = asked.clone();
            Ok(ExecutionBinding::remote(
                gate.clone(),
                Arc::new(|_| Box::pin(async { Err(CoordinatorError::Service("unused".into())) })),
            )
            .with_settlement(Arc::new(move |_| {
                // The first settlement fails at once; every retry hangs.
                let first = asked.fetch_add(1, Ordering::SeqCst) == 0;
                Box::pin(async move {
                    if !first {
                        std::future::pending::<()>().await;
                    }
                    Err(CoordinatorError::Service("host unreachable".into()))
                })
            })))
        }),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    tokio::time::timeout(Duration::from_secs(10), async {
        while calls.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the paused launch is asked again");
    drop(start);
    let began = std::time::Instant::now();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), w.shutdown())
            .await
            .expect("shutdown waited on an unanswered settlement")
            .unwrap(),
        WorkerStatus::Uncertain { .. }
    ));
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(owns(&owner, &fence), "nothing is released without evidence");
}

/// G2 durability (D8): a controller restart with a Ready remote engine adopts
/// it without restarting it. Its dispatch stays closed until readiness is
/// re-proven, and the adopted runtime can still be stopped by this session.
// T33 T10
#[tokio::test]
async fn a_restarted_controller_adopts_a_ready_remote_launch_and_can_stop_it() {
    let (dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(
        owner.clone(),
        observations.clone(),
        host.clone(),
        gate.clone(),
    );
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    drop(start);
    w.shutdown().await.unwrap();
    // The gate held the old session's ownership for its association writes.
    drop(gate);
    drop(owner);

    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    let w = remote_worker(owner.clone(), observations, host.clone(), Gate::new(false));
    let adopted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listed = {
                let o = owner.lock().unwrap();
                o.store().remote_ready_launches(o.session()).unwrap()
            };
            if !listed.is_empty() {
                break listed;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Ready remote launch was never adopted");
    assert!(
        !adopted[0].dispatch_enabled,
        "no dispatch before a fresh probe"
    );
    assert_eq!(
        host.calls.load(Ordering::SeqCst),
        0,
        "a Ready launch is never settled"
    );
    assert_eq!(w.status(), WorkerStatus::Running);
    let stop = w.stop("owner", &fence, "stop-adopted", 10000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        capyctl_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Completed
    );
    assert_eq!(host.cleanups.lock().unwrap()[0].identities.len(), 2);
    assert!(!owns(&owner, &fence));
    w.shutdown().await.unwrap();
}

/// The host side of readiness supervision, scripted: which session is current
/// per host, and what a fresh probe of the retained engine answers.
struct ScriptedReadiness {
    current: Mutex<Option<String>>,
    changes: tokio::sync::watch::Sender<u64>,
    group: Mutex<Vec<ProcessIdentity>>,
    usable: AtomicBool,
    probes: Mutex<Vec<capyctl_protocol::execution::MemberCommand>>,
    binding: (String, String),
    /// The load the host reports for the launch, sampled when it is read.
    load: Mutex<Option<crate::load_table::LoadView>>,
}
impl crate::remote_readiness::ReadinessHosts for ScriptedReadiness {
    fn load(&self, _deployment_id: &str, _generation: i64) -> Option<crate::load_table::LoadView> {
        self.load.lock().unwrap().clone().map(|mut view| {
            view.sampled_at_ms = capyctl_protocol::now_unix_ms();
            view
        })
    }
    fn current_session(&self, _host: &str) -> Option<String> {
        self.current.lock().unwrap().clone()
    }
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
    fn probe(
        &self,
        command: capyctl_protocol::execution::MemberCommand,
    ) -> crate::remote_readiness::ProbeFuture {
        self.probes.lock().unwrap().push(command.clone());
        let session = self.current.lock().unwrap().clone();
        let result = capyctl_protocol::pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            owned_handle: match &command.action {
                capyctl_protocol::execution::MemberAction::Probe { owned_handle, .. } => {
                    owned_handle.clone()
                }
                _ => String::new(),
            },
            processes: self
                .group
                .lock()
                .unwrap()
                .iter()
                .map(|p| capyctl_protocol::pb::OwnedProcessObservation {
                    role: p.role.clone(),
                    pid: p.pid,
                    boot_id: p.boot_id.clone(),
                    start_ticks: p.start_ticks,
                    presence: "alive".into(),
                })
                .collect(),
            observed_at_unix_ms: capyctl_protocol::now_unix_ms(),
            claim_retained: true,
            model_usable: self.usable.load(Ordering::SeqCst),
            binding_id: self.binding.0.clone(),
            incarnation: self.binding.1.clone(),
            residency: None,
            checkpoint: None,
            refused: String::new(),
            launch_failure: String::new(),
            source: None,
            kernel_builds: Vec::new(),
            escalated: false,
            probe_tokens: Vec::new(),
        };
        Box::pin(async move { session.map(|s| (s, result)).ok_or(()) })
    }
}

fn ready_launch(
    owner: &SharedCoordinatorState,
) -> capyctl_store::ordinary_lifecycle::recovery::RemoteReadyLaunch {
    let o = owner.lock().unwrap();
    let mut listed = o.store().remote_ready_launches(o.session()).unwrap();
    assert_eq!(listed.len(), 1);
    listed.remove(0)
}

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition never held");
}

/// G2 (U5 live): after a host agent restart the controller kept dispatching to
/// a closed gate and the router answered 500. Dispatch now closes as soon as the
/// session that proved readiness is gone, and reopens only when the host's new
/// session answers a fresh probe naming exactly the associated group. A failed
/// or mismatched probe keeps it closed, charged and owned.
// T33 T38 T32
#[tokio::test]
async fn host_session_loss_closes_dispatch_until_a_fresh_probe_passes() {
    let (dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(owner.clone(), observations, host, gate);
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let launch = ready_launch(&owner);
    assert!(launch.dispatch_enabled);
    let ledger = crate::remote_execution::ReadinessLedger::default();
    ledger
        .lock()
        .unwrap()
        .insert(launch.binding_id.clone(), "session-1".into());
    let hosts = Arc::new(ScriptedReadiness {
        current: Mutex::new(Some("session-1".into())),
        changes: tokio::sync::watch::channel(0).0,
        group: Mutex::new(launch.identities.clone()),
        usable: AtomicBool::new(true),
        probes: Mutex::new(vec![]),
        binding: (launch.binding_id.clone(), launch.incarnation.clone()),
        load: Mutex::new(None),
    });
    let supervisor = crate::remote_readiness::RemoteReadiness::new(
        owner.clone(),
        hosts.clone(),
        "controller".into(),
        ledger.clone(),
    );
    let dispatching = || ready_launch(&owner).dispatch_enabled;

    // The proving session is current: nothing changes, nothing is probed.
    supervisor.pass();
    assert!(dispatching());
    assert!(hosts.probes.lock().unwrap().is_empty());

    // The host session is lost: dispatch closes at once, ownership stays.
    *hosts.current.lock().unwrap() = None;
    supervisor.pass();
    assert!(!dispatching());
    assert!(owns(&owner, &fence));
    assert!(hosts.probes.lock().unwrap().is_empty(), "no host to probe");

    // A new session whose probe finds a different group proves nothing.
    *hosts.current.lock().unwrap() = Some("session-2".into());
    hosts.group.lock().unwrap()[0].start_ticks += 1;
    supervisor.pass();
    until(|| !hosts.probes.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!dispatching());
    let probe = hosts.probes.lock().unwrap()[0].clone();
    assert_eq!(
        probe.action,
        capyctl_protocol::execution::MemberAction::Probe {
            owned_handle: launch.step_id.clone(),
            max_tokens: None,
        }
    );
    assert_eq!(probe.identity.expected_state, "ready");
    probe.verify_digest().unwrap();

    // A host that answers but cannot prove the model usable proves nothing.
    hosts.group.lock().unwrap()[0].start_ticks -= 1;
    hosts.usable.store(false, Ordering::SeqCst);
    *hosts.current.lock().unwrap() = Some("session-3".into());
    supervisor.pass();
    until(|| hosts.probes.lock().unwrap().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!dispatching());

    // The next session's fresh probe of the exact group reopens dispatch.
    hosts.usable.store(true, Ordering::SeqCst);
    *hosts.current.lock().unwrap() = Some("session-4".into());
    supervisor.pass();
    until(dispatching).await;
    assert_eq!(
        ledger
            .lock()
            .unwrap()
            .get(&launch.binding_id)
            .map(String::as_str),
        Some("session-4")
    );
    let evidence = {
        let o = owner.lock().unwrap();
        o.store().journal_evidence(&launch.operation_id).unwrap()
    };
    assert!(
        evidence.iter().any(|e| e.contains("dispatch reopened")),
        "{evidence:?}"
    );
    assert!(
        evidence.iter().any(|e| e.contains("dispatch is closed")),
        "{evidence:?}"
    );
    drop(start);
    w.shutdown().await.unwrap();
}

/// W10 gap (a), SPEC §§10, 13.2: a switch closed a victim's gate to drain it,
/// and the host session that proved the victim was lost during that drain
/// window. The switch then failed. Reopening the gate would dispatch into a
/// host whose readiness is unproven, so it stays closed until a fresh probe
/// passes. Conversely a probe that passes while the switch still holds the
/// gate closed does not reopen it; the switch's own failure does.
// T17 T33 T38
#[tokio::test]
async fn a_failed_switch_never_reopens_a_gate_a_host_loss_closed() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(owner.clone(), observations, ScriptedHost::new(true), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let launch = ready_launch(&owner);
    let ledger = crate::remote_execution::ReadinessLedger::default();
    ledger
        .lock()
        .unwrap()
        .insert(launch.binding_id.clone(), "session-1".into());
    let hosts = Arc::new(ScriptedReadiness {
        current: Mutex::new(Some("session-1".into())),
        changes: tokio::sync::watch::channel(0).0,
        group: Mutex::new(launch.identities.clone()),
        usable: AtomicBool::new(true),
        probes: Mutex::new(vec![]),
        binding: (launch.binding_id.clone(), launch.incarnation.clone()),
        load: Mutex::new(None),
    });
    let supervisor = crate::remote_readiness::RemoteReadiness::new(
        owner.clone(),
        hosts.clone(),
        "controller".into(),
        ledger.clone(),
    );
    let dispatching = || ready_launch(&owner).dispatch_enabled;
    let commands = w.commands();
    let (deployment, generation) = (fence.deployment_id.clone(), launch.fence.generation);

    // The switch closes the victim's gate to drain it.
    assert!(commands
        .close_for_switch(&deployment, 0, generation)
        .unwrap());
    // The proving session is lost during the drain window: supervision
    // records its own reason although the gate is already closed.
    *hosts.current.lock().unwrap() = None;
    supervisor.pass();
    assert!(ready_launch(&owner).host_closure_recorded);
    // The switch fails (drain timeout): it must not reopen the gate.
    assert!(!commands
        .reopen_after_switch(&deployment, 0, generation)
        .unwrap());
    assert!(
        !dispatching(),
        "a failed switch reopened a host-loss closure"
    );
    // Only the host's fresh probe reopens it.
    *hosts.current.lock().unwrap() = Some("session-2".into());
    supervisor.pass();
    until(dispatching).await;

    // The other order: a switch holds the gate closed, the host flaps and its
    // new session proves the engine again. The probe clears only its own
    // reason; the gate stays closed until the switch ends.
    assert!(commands
        .close_for_switch(&deployment, 0, generation)
        .unwrap());
    *hosts.current.lock().unwrap() = Some("session-3".into());
    supervisor.pass();
    until(|| {
        ledger
            .lock()
            .unwrap()
            .get(&launch.binding_id)
            .map(String::as_str)
            == Some("session-3")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!dispatching(), "a probe reopened a gate the switch holds");
    assert!(!ready_launch(&owner).host_closure_recorded);
    assert!(commands
        .reopen_after_switch(&deployment, 0, generation)
        .unwrap());
    assert!(dispatching());
    drop(start);
    w.shutdown().await.unwrap();
}

/// G2 (U5 live): the router asked for a Ready remote deployment's endpoint and
/// forwarded into the host's closed gate, answering HTTP 500 `backend completion
/// unverified`. While dispatch is closed for an unproven host session the
/// lifecycle authority refuses the endpoint, so the router answers 503 instead.
// T38 T18
#[tokio::test]
async fn a_closed_remote_dispatch_refuses_the_router_its_endpoint() {
    use crate::port::LifecyclePort;
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(owner.clone(), observations, ScriptedHost::new(true), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let lifecycle = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    let open = lifecycle
        .runtime_endpoint(&fence.deployment_id)
        .unwrap()
        .expect("a Ready deployment has an endpoint");
    assert_eq!(open.endpoint, "http://100.64.0.1:9443");
    let launch = ready_launch(&owner);
    {
        let o = owner.lock().unwrap();
        assert!(o
            .store()
            .suspend_remote_dispatch(o.session(), &launch.step_id)
            .unwrap());
    }
    assert!(matches!(
        lifecycle.runtime_endpoint(&fence.deployment_id),
        Err(crate::fault::LifecycleFault::Unavailable(_))
    ));
    drop(start);
    w.shutdown().await.unwrap();
}

/// W12 (SPEC §10): a controller crash with requests in flight left their leases
/// on the adopted launch. A passing probe no longer reopens dispatch while they
/// remain; a load sample with unknown or non-zero engine work settles nothing;
/// only a quiescent sample from the host session, taken after the probe, closes
/// them as abandoned, and only then does dispatch reopen.
// T33 T38 T32
#[tokio::test]
async fn crashed_request_leases_close_only_on_quiescence_after_a_fresh_probe() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(owner.clone(), observations, ScriptedHost::new(true), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let launch = ready_launch(&owner);
    // The requests a crashed session had in flight on this exact fence.
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('crashed',?1,?2,?3,'01J00000000000000000000000','uncertain')",
        rusqlite::params![launch.fence.deployment_id, launch.fence.revision, launch.fence.generation],
    )
    .unwrap();
    let leases = || -> i64 {
        sql.query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
            [&launch.fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let hosts = Arc::new(ScriptedReadiness {
        current: Mutex::new(None),
        changes: tokio::sync::watch::channel(0).0,
        group: Mutex::new(launch.identities.clone()),
        usable: AtomicBool::new(true),
        probes: Mutex::new(vec![]),
        binding: (launch.binding_id.clone(), launch.incarnation.clone()),
        load: Mutex::new(None),
    });
    let sample = |running: u32, engine: bool| crate::load_table::LoadView {
        deployment_id: launch.fence.deployment_id.clone(),
        generation: launch.fence.generation,
        host_id: "lab".into(),
        owned_handle: launch.step_id.clone(),
        sampled_at_ms: 0,
        age_ms: 0,
        fresh: true,
        ingress_in_flight: 0,
        engine: engine.then_some(crate::load_table::EngineGauges {
            running,
            waiting: 0,
            kv_usage_ppm: 0,
        }),
    };
    let ledger = crate::remote_execution::ReadinessLedger::default();
    let supervisor = crate::remote_readiness::RemoteReadiness::new(
        owner.clone(),
        hosts.clone(),
        "controller".into(),
        ledger,
    );
    let dispatching = || ready_launch(&owner).dispatch_enabled;
    supervisor.pass();
    assert!(!dispatching());

    // The host reconnects; its fresh probe passes, but the engine is still busy
    // and a failed scrape is unknown load, not zero.
    *hosts.load.lock().unwrap() = Some(sample(1, true));
    *hosts.current.lock().unwrap() = Some("session-2".into());
    supervisor.pass();
    until(|| !hosts.probes.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dispatching());
    assert_eq!(leases(), 1);
    *hosts.load.lock().unwrap() = Some(sample(0, false));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dispatching());
    assert_eq!(leases(), 1);
    assert!(owns(&owner, &fence));

    // The engine drains: the leases close as abandoned and dispatch reopens.
    *hosts.load.lock().unwrap() = Some(sample(0, true));
    until(dispatching).await;
    assert_eq!(leases(), 0);
    let evidence = {
        let o = owner.lock().unwrap();
        o.store().journal_evidence(&launch.operation_id).unwrap()
    };
    assert!(
        evidence.iter().any(|e| e.contains("closed as abandoned")),
        "{evidence:?}"
    );
    assert!(owns(&owner, &fence));
    drop(start);
    w.shutdown().await.unwrap();
}

/// W12 (SPEC §13.2): a Stop accepted and armed when the controller went down
/// was stranded with the dead session: the deployment stayed charged and every
/// later Stop was refused. The restarted remote worker adopts the Stop, re-sends
/// its Terminate to exactly the recorded group, and releases only on the host's
/// gone evidence.
// T33 T32 T10
#[tokio::test]
async fn a_restarted_controller_resumes_a_stop_its_crashed_session_accepted() {
    let (dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(
        owner.clone(),
        observations.clone(),
        host.clone(),
        gate.clone(),
    );
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    // The host does not answer the cleanup Terminate before the controller dies.
    host.stoppable.store(false, Ordering::SeqCst);
    let stop = w.stop("owner", &fence, "stop-before-crash", 10000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert_eq!(host.cleanups.lock().unwrap().len(), 1);
    drop(stop);
    drop(start);
    w.shutdown().await.unwrap();
    drop(gate);
    drop(owner);

    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    assert!(owns(&owner, &fence), "restart releases nothing");
    let restarted = ScriptedHost::new(true);
    let w = remote_worker(
        owner.clone(),
        observations,
        restarted.clone(),
        Gate::new(false),
    );
    until(|| !owns(&owner, &fence)).await;
    assert_eq!(binding_state(&owner, &fence), "released");
    let resent = restarted.cleanups.lock().unwrap().clone();
    assert_eq!(resent.len(), 1, "one fresh Terminate after adoption");
    assert_eq!(resent[0].identities.len(), 2);
    let evidence = {
        let o = owner.lock().unwrap();
        o.store().journal_evidence_of(&fence.deployment_id).unwrap()
    };
    assert!(
        evidence
            .iter()
            .any(|e| e.contains("adopted its accepted Stop")
                && e.contains("may already have been sent")),
        "{evidence:?}"
    );
    assert_eq!(w.status(), WorkerStatus::Running);
    w.shutdown().await.unwrap();
}

/// A remote engine whose host refused the launch on its own policy before any
/// effect, as `remote_execution` reports it: nothing recorded, nothing claimed.
struct RefusingHost;
#[async_trait::async_trait]
impl EngineAdapter for RefusingHost {
    async fn execute_persisted(
        &self,
        _: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        Err(RuntimeError::Refused("checkpoint_mismatch".into()))
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

/// SPEC §13 (WE3 limit 1): a host that refused the launch on its own policy
/// (here a checkpoint that no longer measures to the recorded digest) started
/// and claimed nothing. The failed launch settles at once on the host's
/// authenticated evidence, with one settlement and no uncertain pause, and
/// the closed reason is what the operation's journal reports.
// T14 T20 T34
#[tokio::test]
async fn a_host_policy_refusal_settles_the_launch_at_once_with_its_reason() {
    let (_dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let scripted = host.clone();
    let w = OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
        Arc::new(move |_: &InitializeWork| Ok(scripted.binding(Arc::new(RefusingHost)))),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Closed
    );
    assert_eq!(
        host.calls.load(Ordering::SeqCst),
        1,
        "settled once, never paused"
    );
    let context = host.contexts.lock().unwrap()[0].clone();
    assert!(
        context.identities.is_empty(),
        "the refusing host started nothing"
    );
    assert_eq!(binding_state(&owner, &fence), "released");
    assert!(!owns(&owner, &fence));
    {
        let o = owner.lock().unwrap();
        let evidence = o.store().journal_evidence(start.operation_id()).unwrap();
        assert!(
            evidence
                .iter()
                .any(|entry| entry.contains("launch failed")
                    && entry.contains("checkpoint_mismatch")),
            "{evidence:?}"
        );
        let operation = o
            .store()
            .latest_operation(&fence.deployment_id)
            .unwrap()
            .unwrap();
        assert_eq!(operation.error_code.as_deref(), Some("launch_failed"));
    }
    running(&w).await;
    w.shutdown().await.unwrap();
}

/// Observations that also report which remote hosts are reachable now.
struct Presence {
    observations: Vec<MemoryObservation>,
    online: Mutex<std::collections::BTreeSet<String>>,
}
impl ServiceObservation for Presence {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.observations.clone();
        Box::pin(async move { Ok(values) })
    }
    fn online_hosts(&self) -> Option<std::collections::BTreeSet<String>> {
        Some(self.online.lock().unwrap().clone())
    }
}

/// Owner decision 4 (2026-09-22): `drain host` on an offline host. Its Stop is
/// accepted and durable, but its cleanup Terminate was sent into the protocol
/// timeout and the whole worker halted uncertain until a controller restart.
/// The cleanup now waits unarmed while the host is offline (nothing is sent,
/// nothing released, the worker keeps running) and completes on the host's
/// gone evidence once it reconnects.
// T10 T32 T33
#[tokio::test]
async fn a_stop_for_an_offline_host_waits_unarmed_until_it_reconnects() {
    let (dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let presence = Arc::new(Presence {
        observations,
        online: Mutex::new(["lab".to_string()].into_iter().collect()),
    });
    let (scripted, launched) = (host.clone(), gate.clone());
    let w = OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        presence.clone(),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        Arc::new(move |_: &InitializeWork| Ok(scripted.binding(launched.clone()))),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    presence.online.lock().unwrap().clear();
    let stop = w.stop("owner", &fence, "drain:offline", 10000).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        host.cleanups.lock().unwrap().is_empty(),
        "nothing is sent to an offline host"
    );
    assert_eq!(
        w.status(),
        WorkerStatus::Running,
        "the worker keeps running"
    );
    assert!(owns(&owner, &fence), "nothing is released without evidence");
    assert_ne!(binding_state(&owner, &fence), "released");
    presence.online.lock().unwrap().insert("lab".into());
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        capyctl_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Completed
    );
    let cleanups = host.cleanups.lock().unwrap().clone();
    assert_eq!(
        cleanups.len(),
        1,
        "one Terminate, after the host reconnected"
    );
    assert_eq!(binding_state(&owner, &fence), "released");
    assert!(!owns(&owner, &fence));
    drop(start);
    w.shutdown().await.unwrap();
}

/// Owner decision 4 (limit): a drain Stop's deadline is bounded by the
/// deployment's request deadline, and an elapsed one can no longer be armed.
/// When the host reconnects only after it passed, the Stop is not sent and the
/// worker does not halt on it: the engine stays owned and charged, the
/// operation stays open, and every other deployment keeps being served.
// T10 T32 T33
#[tokio::test]
async fn an_offline_stop_whose_deadline_passed_neither_sends_nor_halts() {
    let (dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let presence = Arc::new(Presence {
        observations,
        online: Mutex::new(["lab".to_string()].into_iter().collect()),
    });
    let clock = Arc::new(std::sync::atomic::AtomicI64::new(1900));
    let (scripted, launched, now) = (host.clone(), gate.clone(), clock.clone());
    let w = OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        presence.clone(),
        Arc::new(move || Ok(now.load(Ordering::SeqCst))),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        Arc::new(move |_: &InitializeWork| Ok(scripted.binding(launched.clone()))),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    presence.online.lock().unwrap().clear();
    let stop = w.stop("owner", &fence, "drain:expired", 10000).unwrap();
    clock.store(20_000, Ordering::SeqCst);
    presence.online.lock().unwrap().insert("lab".into());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        host.cleanups.lock().unwrap().is_empty(),
        "an elapsed Stop is never sent"
    );
    assert_eq!(
        w.status(),
        WorkerStatus::Running,
        "the worker keeps running"
    );
    assert!(owns(&owner, &fence), "nothing is released without evidence");
    assert_ne!(binding_state(&owner, &fence), "released");
    drop((start, stop));
    w.shutdown().await.unwrap();
}

/// Owner decision 2026-09-22 (offline drain re-issue): a drain's Stop whose
/// deadline passed while its host was offline was never armed, so no effect
/// was ever sent. When the host reconnects, the server closes it as `expired`
/// and issues a fresh ordinary Stop, drain origin, with a new deadline, for the
/// same instance. The engine stays owned and charged until the host's gone
/// evidence; the drain marker stays pending until the fresh Stop settles, then
/// clears. Every transition is journaled.
// T10 T32 T33
#[tokio::test]
async fn an_expired_drain_stop_is_reissued_when_its_host_reconnects() {
    let (dir, owner, fence, observations) = setup().await;
    let host = ScriptedHost::new(true);
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let presence = Arc::new(Presence {
        observations,
        online: Mutex::new(["lab".to_string()].into_iter().collect()),
    });
    let clock = Arc::new(std::sync::atomic::AtomicI64::new(1900));
    let (scripted, launched, now) = (host.clone(), gate.clone(), clock.clone());
    let w = OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        presence.clone(),
        Arc::new(move || Ok(now.load(Ordering::SeqCst))),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        Arc::new(move |_: &InitializeWork| Ok(scripted.binding(launched.clone()))),
    )
    .unwrap();
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    presence.online.lock().unwrap().clear();
    let stop = w.stop("owner", &fence, "drain:reissue", 10000).unwrap();
    let expired = stop.operation_id().to_owned();
    owner
        .lock()
        .unwrap()
        .store()
        .record_host_drain("lab", std::slice::from_ref(&expired), 1900)
        .unwrap();
    // The host stays offline past the Stop's deadline: nothing is sent.
    clock.store(20_000, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(host.cleanups.lock().unwrap().is_empty());
    assert!(owns(&owner, &fence), "nothing is released without evidence");
    assert!(owner
        .lock()
        .unwrap()
        .store()
        .host_drain_pending("lab")
        .unwrap());

    presence.online.lock().unwrap().insert("lab".into());
    until(|| !owns(&owner, &fence)).await;
    assert_eq!(binding_state(&owner, &fence), "released");
    let cleanups = host.cleanups.lock().unwrap().clone();
    assert_eq!(cleanups.len(), 1, "one Terminate, from the reissued Stop");
    assert_eq!(
        stop.wait(Duration::from_secs(10)).await.unwrap(),
        capyctl_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Expired
    );

    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let (state, code): (String, Option<String>) = sql
        .query_row(
            "SELECT state,error_code FROM operations WHERE id=?1",
            [&expired],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("failed", Some("expired"))
    );
    let reissued: String = sql
        .query_row(
            "SELECT operation_id FROM host_drains WHERE host_id='lab' AND operation_id!=?1",
            [&expired],
            |r| r.get(0),
        )
        .unwrap();
    let (state, code): (String, Option<String>) = sql
        .query_row(
            "SELECT state,error_code FROM operations WHERE id=?1 AND kind='ordinary_cleanup'",
            [&reissued],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), code), ("succeeded", None));
    let deadline: i64 = sql
        .query_row(
            "SELECT deadline_ms FROM lifecycle_runs WHERE operation_id=?1",
            [&reissued],
            |r| r.get(0),
        )
        .unwrap();
    assert!(deadline > 20_000, "the reissued Stop has a new deadline");
    let kinds: Vec<String> = sql
        .prepare(
            "SELECT kind FROM management_events WHERE operation_id IN (?1,?2) ORDER BY sequence",
        )
        .unwrap()
        .query_map([&expired, &reissued], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for kind in [
        "ordinary_cleanup_expired_unarmed",
        "ordinary_cleanup_accepted",
        "ordinary_cleanup_armed",
        "ordinary_cleanup_completed",
    ] {
        assert!(kinds.iter().any(|k| k == kind), "{kind}: {kinds:?}");
    }
    {
        let o = owner.lock().unwrap();
        let old = o.store().journal_evidence(&expired).unwrap();
        assert!(old.iter().any(|e| e.contains("expired")), "{old:?}");
        let new = o.store().journal_evidence(&reissued).unwrap();
        assert!(new.iter().any(|e| e.contains("reissued")), "{new:?}");
        assert!(
            !o.store().host_drain_pending("lab").unwrap(),
            "the marker clears"
        );
    }
    // A retried drain replays the original Stop's receipt; it issues nothing.
    let replay = w.stop("owner", &fence, "drain:reissue", 10000).unwrap();
    assert_eq!(replay.operation_id(), expired);
    assert_eq!(
        replay.wait(Duration::from_secs(10)).await.unwrap(),
        capyctl_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Expired
    );
    assert_eq!(host.cleanups.lock().unwrap().len(), 1);
    assert_eq!(w.status(), WorkerStatus::Running);
    drop((start, stop, replay));
    w.shutdown().await.unwrap();
}

/// ADR 0013 §10 (unit I3): the router's instance view of a Ready remote
/// instance carries its generation, serving host, launch, gate, the host's
/// session liveness and the load that host reported for exactly this launch.
/// A lease and its endpoint are resolved for exactly that generation; once the
/// gate closes both are refused, so the router fails over before sending.
// T18 T33 T38
#[tokio::test]
async fn the_router_sees_each_instance_with_its_host_load_and_generation_fence() {
    use crate::port::LifecyclePort;
    use capyctl_protocol::reports::{EngineLoad, LoadReport, LoadSample};
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(owner.clone(), observations, ScriptedHost::new(true), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let launch = ready_launch(&owner);
    let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let load = Arc::new(crate::load_table::LoadTable::new());
    let signals = crate::coordinator_port::RoutingSignals {
        load: load.clone(),
        host_live: {
            let live = live.clone();
            Arc::new(move |host: &str| host == "lab" && live.load(Ordering::SeqCst))
        },
        host_unresponsive: Arc::new(|_: &str| false),
    };
    let lifecycle =
        crate::coordinator_port::CoordinatorLifecycle::new(w.commands()).with_routing(signals);
    let now = capyctl_protocol::now_unix_ms();
    let sample = |owned_handle: &str| LoadSample {
        deployment_id: fence.deployment_id.clone(),
        generation: fence.generation,
        owned_handle: owned_handle.into(),
        sampled_at_ms: now,
        ingress_in_flight: 1,
        engine: Some(EngineLoad {
            running: 3,
            waiting: 1,
            kv_usage_ppm: 100_000,
        }),
        latency: None,
    };
    load.accept(
        "lab",
        LoadReport {
            host_id: "lab".into(),
            samples: vec![sample(&launch.step_id)],
        },
        now,
    )
    .unwrap();
    let instances = lifecycle
        .serving_instances(&fence.deployment_id)
        .unwrap()
        .expect("the coordinator has an instance view");
    assert_eq!(instances.len(), 1);
    let only = &instances[0];
    assert_eq!(only.generation, fence.generation);
    assert_eq!(only.remote_host.as_deref(), Some("lab"));
    assert_eq!(
        only.launch_command_id.as_deref(),
        Some(launch.step_id.as_str())
    );
    assert!(only.dispatch_open && only.host_live);
    let view = only.load.as_ref().expect("a fresh sample for this launch");
    assert_eq!(view.engine_queue(), Some(4));
    assert_eq!(view.owned_handle, launch.step_id);
    // The endpoint and the lease both follow the generation.
    let endpoint = lifecycle
        .instance_endpoint(&fence.deployment_id, fence.generation)
        .unwrap()
        .expect("the instance's endpoint");
    assert_eq!(endpoint.endpoint, "http://100.64.0.1:9443");
    assert!(lifecycle
        .instance_endpoint(&fence.deployment_id, fence.generation + 7)
        .unwrap()
        .is_none());
    let lease = lifecycle
        .open_instance_lease(&fence.deployment_id, fence.generation, 8)
        .await
        .unwrap()
        .expect("a durable lease");
    assert!(matches!(
        lifecycle
            .open_instance_lease(&fence.deployment_id, fence.generation + 7, 8)
            .await,
        Err(crate::request_leases::LeaseRefused::Closed)
    ));
    // SPEC §13.2: the host's session is gone; the view says so at once.
    live.store(false, Ordering::SeqCst);
    let instances = lifecycle
        .serving_instances(&fence.deployment_id)
        .unwrap()
        .unwrap();
    assert!(!instances[0].host_live);
    // The gate closes (the supervisor noticed): no lease, no endpoint.
    {
        let o = owner.lock().unwrap();
        assert!(o
            .store()
            .suspend_remote_dispatch(o.session(), &launch.step_id)
            .unwrap());
    }
    let instances = lifecycle
        .serving_instances(&fence.deployment_id)
        .unwrap()
        .unwrap();
    assert!(!instances[0].dispatch_open);
    assert!(matches!(
        lifecycle
            .open_instance_lease(&fence.deployment_id, fence.generation, 8)
            .await,
        Err(crate::request_leases::LeaseRefused::Closed)
    ));
    assert!(matches!(
        lifecycle.instance_endpoint(&fence.deployment_id, fence.generation),
        Err(crate::fault::LifecycleFault::Unavailable(_))
    ));
    // T32: the lease already granted stays charged until closed on evidence.
    {
        let o = owner.lock().unwrap();
        assert_eq!(
            o.store()
                .pending_dispatches(&fence.deployment_id)
                .unwrap()
                .len(),
            1
        );
    }
    lifecycle
        .close_request_lease(lease, crate::request_leases::LeaseEnd::NotAccepted)
        .await
        .unwrap();
    drop(start);
    w.shutdown().await.unwrap();
}

/// Owner decision 2026-09-23: a host whose heartbeats went silent keeps its
/// session, so when it is heard again its session id is unchanged. Suspension
/// forgets that session's readiness proof, so the same session must answer a
/// fresh probe before dispatch reopens (same as a reconnect). Nothing is
/// released while it is silent.
// T33 T38 T29
#[tokio::test]
async fn an_unresponsive_host_reproves_readiness_on_the_same_session() {
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let w = remote_worker(owner.clone(), observations, ScriptedHost::new(true), gate);
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let launch = ready_launch(&owner);
    assert!(launch.dispatch_enabled);
    let ledger = crate::remote_execution::ReadinessLedger::default();
    ledger
        .lock()
        .unwrap()
        .insert(launch.binding_id.clone(), "session-1".into());
    let hosts = Arc::new(ScriptedReadiness {
        current: Mutex::new(Some("session-1".into())),
        changes: tokio::sync::watch::channel(0).0,
        group: Mutex::new(launch.identities.clone()),
        usable: AtomicBool::new(true),
        probes: Mutex::new(vec![]),
        binding: (launch.binding_id.clone(), launch.incarnation.clone()),
        load: Mutex::new(None),
    });
    let supervisor = crate::remote_readiness::RemoteReadiness::new(
        owner.clone(),
        hosts.clone(),
        "controller".into(),
        ledger.clone(),
    );
    let dispatching = || ready_launch(&owner).dispatch_enabled;

    // Silent past the suspend bound: dispatch closes and the proof is gone,
    // while the session no longer counts as current.
    *hosts.current.lock().unwrap() = None;
    supervisor.suspend_unresponsive_host(&launch.host_id);
    assert!(!dispatching());
    assert!(ledger.lock().unwrap().get(&launch.binding_id).is_none());
    supervisor.pass();
    assert!(!dispatching());
    assert!(owns(&owner, &fence), "nothing is released while silent");
    assert!(hosts.probes.lock().unwrap().is_empty());

    // Heard again on the same session: a fresh probe re-proves it first.
    *hosts.current.lock().unwrap() = Some("session-1".into());
    supervisor.pass();
    until(|| !hosts.probes.lock().unwrap().is_empty()).await;
    until(dispatching).await;
    assert_eq!(
        ledger
            .lock()
            .unwrap()
            .get(&launch.binding_id)
            .map(String::as_str),
        Some("session-1")
    );
    assert!(owns(&owner, &fence));
    drop(start);
    w.shutdown().await.unwrap();
}

/// The scripted remote launch, whose quiescence question is answered by the
/// real remote engine once the test has built it for the Ready launch.
struct RemoteQuiescence {
    gate: Arc<Gate>,
    remote: Mutex<Option<Arc<dyn EngineAdapter>>>,
}
#[async_trait::async_trait]
impl EngineAdapter for RemoteQuiescence {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        self.gate.execute_persisted(command).await
    }
    async fn inspect(&self, m: &MemberRef) -> Result<EngineState, AdapterError> {
        self.gate.inspect(m).await
    }
    async fn render_plan(&self, p: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        self.gate.render_plan(p).await
    }
    async fn check_readiness(&self, m: &MemberRef) -> Result<Readiness, AdapterError> {
        self.gate.check_readiness(m).await
    }
    async fn prepare_park(&self, m: &MemberRef) -> Result<Quiescence, AdapterError> {
        self.gate.prepare_park(m).await
    }
    async fn park(&self, m: &MemberRef, l: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        self.gate.park(m, l).await
    }
    async fn restore(&self, m: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        self.gate.restore(m).await
    }
    async fn reload_weights(&self, m: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        self.gate.reload_weights(m).await
    }
    async fn observe_work(&self, m: &MemberRef) -> Result<WorkObservation, AdapterError> {
        self.gate.observe_work(m).await
    }
    async fn cancel_work(
        &self,
        m: &MemberRef,
        r: &RequestRef,
        f: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        self.gate.cancel_work(m, r, f).await
    }
    async fn engine_quiescent(&self, member: &MemberRef, after_ms: i64) -> bool {
        let remote = self.remote.lock().unwrap().clone();
        match remote {
            Some(remote) => remote.engine_quiescent(member, after_ms).await,
            None => false,
        }
    }
}

/// SPEC §10 (amended 2026-10-01): a hung-up request on a remote launch closes
/// only on the host's load report. A sample is clamped to the time it reached
/// the controller, so it is always older than a question asked now; the
/// question is therefore asked about the hang-up itself. An idle sample taken
/// before the hang-up closes nothing; one taken after it closes the lease.
// T17 T18 T38
#[tokio::test]
async fn a_remote_cancelling_lease_settles_only_on_an_idle_sample_after_the_hang_up() {
    use crate::port::LifecyclePort;
    use capyctl_protocol::reports::{EngineLoad, LoadReport, LoadSample};
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let engine = Arc::new(RemoteQuiescence {
        gate,
        remote: Mutex::new(None),
    });
    let w = remote_worker(
        owner.clone(),
        observations,
        ScriptedHost::new(true),
        engine.clone(),
    );
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let launch = ready_launch(&owner);
    let authority = Arc::new(crate::enrollment::EnrollmentAuthority::new(
        owner.clone(),
        capyctl_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    let sessions = crate::agent_sessions::AgentSessions::new(authority);
    let load = sessions.load_table();
    *engine.remote.lock().unwrap() = Some(crate::remote_execution::engine(
        sessions,
        owner.clone(),
        crate::remote_execution::RemoteLaunchBinding {
            controller_id: "controller".into(),
            host_id: "lab".into(),
            member_id: "head".into(),
            profile_fingerprint: "fingerprint".into(),
            launch_command_id: launch.step_id.clone(),
            plan: capyctl_protocol::execution::SingleLaunchPlan {
                deployment_config: "{}".into(),
                profile_name: "local".into(),
                checkpoint_fingerprint: "checkpoint".into(),
                host_policy_fingerprint: "a".repeat(64),
                binding_id: launch.binding_id.clone(),
                incarnation: launch.incarnation.clone(),
                grant_id: "grant".into(),
                service_port: 30000,
                issued_at_ms: 1,
                coordinator_session_id: "session".into(),
                checkpoint_digest: String::new(),
                checkpoint_weights_bytes: None,
                checkpoint_state_slot_bytes: None,
                startup_bytes: None,
            },
            ingress_gate_key: [7; 32],
            instance_index: 0,
            device_memory: false,
        },
        Default::default(),
    ));
    let idle_sample = |at: i64| {
        load.accept(
            "lab",
            LoadReport {
                host_id: "lab".into(),
                samples: vec![LoadSample {
                    deployment_id: fence.deployment_id.clone(),
                    generation: fence.generation,
                    owned_handle: launch.step_id.clone(),
                    sampled_at_ms: at,
                    ingress_in_flight: 0,
                    engine: Some(EngineLoad {
                        running: 0,
                        waiting: 0,
                        kv_usage_ppm: 0,
                    }),
                    latency: None,
                }],
            },
            capyctl_protocol::now_unix_ms(),
        )
        .unwrap()
    };
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let leases = || -> i64 {
        sql.query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let lifecycle = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    let lease = lifecycle
        .open_instance_lease(&fence.deployment_id, fence.generation, 8)
        .await
        .unwrap()
        .expect("a durable lease");
    // The host's last idle sample was taken before the client hung up.
    assert_eq!(idle_sample(capyctl_protocol::now_unix_ms()).applied, 1);
    tokio::time::sleep(Duration::from_millis(5)).await;
    lifecycle
        .close_request_lease(lease, crate::request_leases::LeaseEnd::Cancelling)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(leases(), 1, "an idle sample before the hang-up");
    // A sample taken after the hang-up settles it, on the host's receipt.
    assert_eq!(idle_sample(capyctl_protocol::now_unix_ms()).applied, 1);
    until(|| leases() == 0).await;
    let receipt: String = sql
        .query_row(
            "SELECT payload_json FROM management_events WHERE kind='request_cancellation_acknowledged' AND deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(receipt.contains("the host reported"), "{receipt}");
    drop(start);
    w.shutdown().await.unwrap();
}

/// SPEC §10 (amended 2026-10-01), found live 2026-10-01: a park or stop
/// accepted right after a client hung up left the remote launch out of the
/// Ready list, so the host's idle sample was never matched and the cancelling
/// lease held the park until its deadline. The launch's own sample still
/// settles the lease while the instance is draining for a stop.
// T17 T18 T38
#[tokio::test]
async fn a_remote_cancelling_lease_settles_while_a_stop_drains() {
    use crate::port::LifecyclePort;
    use capyctl_protocol::reports::{EngineLoad, LoadReport, LoadSample};
    let (dir, owner, fence, observations) = setup().await;
    let gate = Gate::new(false);
    *gate.association.lock().unwrap() = Some(owner.clone());
    gate.release.add_permits(1);
    let engine = Arc::new(RemoteQuiescence {
        gate,
        remote: Mutex::new(None),
    });
    let w = remote_worker(
        owner.clone(),
        observations,
        ScriptedHost::new(true),
        engine.clone(),
    );
    let start = w.start(&fence, 10000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(60)).await.unwrap(),
        InitializeStatus::Completed
    );
    make_remote(&dir, &owner, &fence);
    let launch = ready_launch(&owner);
    let authority = Arc::new(crate::enrollment::EnrollmentAuthority::new(
        owner.clone(),
        capyctl_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    let sessions = crate::agent_sessions::AgentSessions::new(authority);
    let load = sessions.load_table();
    *engine.remote.lock().unwrap() = Some(crate::remote_execution::engine(
        sessions,
        owner.clone(),
        crate::remote_execution::RemoteLaunchBinding {
            controller_id: "controller".into(),
            host_id: "lab".into(),
            member_id: "head".into(),
            profile_fingerprint: "fingerprint".into(),
            launch_command_id: launch.step_id.clone(),
            plan: capyctl_protocol::execution::SingleLaunchPlan {
                deployment_config: "{}".into(),
                profile_name: "local".into(),
                checkpoint_fingerprint: "checkpoint".into(),
                host_policy_fingerprint: "a".repeat(64),
                binding_id: launch.binding_id.clone(),
                incarnation: launch.incarnation.clone(),
                grant_id: "grant".into(),
                service_port: 30000,
                issued_at_ms: 1,
                coordinator_session_id: "session".into(),
                checkpoint_digest: String::new(),
                checkpoint_weights_bytes: None,
                checkpoint_state_slot_bytes: None,
                startup_bytes: None,
            },
            ingress_gate_key: [7; 32],
            instance_index: 0,
            device_memory: false,
        },
        Default::default(),
    ));
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let leases = || -> i64 {
        sql.query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let lifecycle = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    let lease = lifecycle
        .open_instance_lease(&fence.deployment_id, fence.generation, 8)
        .await
        .unwrap()
        .expect("a durable lease");
    lifecycle
        .close_request_lease(lease, crate::request_leases::LeaseEnd::Cancelling)
        .await
        .unwrap();
    // The stop is accepted while the lease is still cancelling; its drain
    // (30 s by default) waits for the lease.
    let stop = w
        .stop("owner", &fence, "stop-after-hang-up", 100_000)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let applied = load
        .accept(
            "lab",
            LoadReport {
                host_id: "lab".into(),
                samples: vec![LoadSample {
                    deployment_id: fence.deployment_id.clone(),
                    generation: fence.generation,
                    owned_handle: launch.step_id.clone(),
                    sampled_at_ms: capyctl_protocol::now_unix_ms(),
                    ingress_in_flight: 0,
                    engine: Some(EngineLoad {
                        running: 0,
                        waiting: 0,
                        kv_usage_ppm: 0,
                    }),
                    latency: None,
                }],
            },
            capyctl_protocol::now_unix_ms(),
        )
        .unwrap()
        .applied;
    assert_eq!(applied, 1);
    until(|| leases() == 0).await;
    assert_eq!(
        stop.wait(Duration::from_secs(60)).await.unwrap(),
        capyctl_store::ordinary_lifecycle::cleanup::OrdinaryCleanupStatus::Completed
    );
    drop(start);
    w.shutdown().await.unwrap();
}
