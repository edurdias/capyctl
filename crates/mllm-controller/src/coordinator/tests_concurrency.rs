//! ADR 0015: per-instance concurrent lifecycle work.
//!
//! Live M16 (2026-09-23) found one slow or dead activation holding every other
//! deployment's start and stop for up to fifteen minutes. These drive the real
//! scheduler over the fixture's two deployments, each with its own scripted
//! engine, so nothing here qualifies a native engine or a host.
use super::*;
use std::time::Instant;

/// A worker whose driver factory hands each deployment its own scripted engine.
fn lane_worker(
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

fn owners(owner: &SharedCoordinatorState) -> Vec<String> {
    owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners
        .keys()
        .cloned()
        .collect()
}

fn admission_closed(sql: &rusqlite::Connection, deployment: &str) -> bool {
    sql.query_row(
        "SELECT admission_enabled=0 FROM deployments WHERE id=?1",
        [deployment],
        |r| r.get(0),
    )
    .unwrap()
}

/// ADR 0015 invariant 4, the M16 finding: an Initialize that hangs until its
/// deadline holds nothing but its own instance. Another deployment starts and
/// stops meanwhile, and the hung one then reaches its deadline alone and pauses
/// uncertain with its reservation retained, exactly as before.
// T10 T20
#[tokio::test]
async fn a_hung_initialize_never_holds_another_deployments_start_or_stop() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let hung = Gate::new(false);
    let quick = Gate::new(false);
    quick.release.add_permits(1);
    let w = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), hung.clone()),
            (other.deployment_id.clone(), quick.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    // The service clock stands still at 1900, so this deadline bounds the hung
    // Initialize to four seconds of real time.
    let hung_start = w.start(&fence, 5900).unwrap();
    hung.entered().await;
    let began = Instant::now();
    let quick_start = w.start(&other, 60_000).unwrap();
    assert_eq!(
        quick_start.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let stop = w.stop("owner", &other, "stop-other", 60_000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(10)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "the other deployment waited {:?} behind a hung load",
        began.elapsed()
    );
    assert!(
        hung.active.load(Ordering::SeqCst),
        "the hung load is still in flight"
    );
    assert_eq!(w.status(), WorkerStatus::Running);
    assert_eq!(owners(&owner), vec![fence.deployment_id.clone()]);
    // The hung Initialize reaches its own deadline and pauses uncertain.
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    assert_eq!(
        hung_start.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Uncertain
    );
    assert!(!hung.active.load(Ordering::SeqCst));
    assert_peak(&owner, &fence);
    assert_eq!(*hung.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    assert_eq!(
        *quick.calls.lock().unwrap(),
        vec![RuntimeAction::Initialize]
    );
    drop(hung_start);
    w.shutdown().await.unwrap();
}

/// ADR 0015 invariants 1 and 4: a Stop of a Ready deployment completes while
/// another deployment is still loading, and the loading one then reaches Ready
/// undisturbed on its own lane.
// T10
#[tokio::test]
async fn a_stop_completes_while_another_deployment_is_loading() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let loading = Gate::new(false);
    let ready = Gate::new(false);
    ready.release.add_permits(1);
    let w = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), loading.clone()),
            (other.deployment_id.clone(), ready.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    let first = w.start(&other, 60_000).unwrap();
    assert_eq!(
        first.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let load = w.start(&fence, 60_000).unwrap();
    loading.entered().await;
    let began = Instant::now();
    let stop = w.stop("owner", &other, "stop-ready", 60_000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(10)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert!(began.elapsed() < Duration::from_secs(3));
    assert!(loading.active.load(Ordering::SeqCst));
    loading.release.add_permits(1);
    assert_eq!(
        load.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(w.status(), WorkerStatus::Running);
    // Exact accounting: the stopped deployment released on its evidence, the
    // loaded one holds its own reservation, and nothing else is charged.
    assert_eq!(owners(&owner), vec![fence.deployment_id.clone()]);
    w.shutdown().await.unwrap();
}

/// ADR 0015 invariant 3: two starts that race for one host's capacity are
/// serialized by the store's arm transaction. Both observe the host before
/// either arms; exactly one is admitted and reaches Ready, the other is refused
/// at its arm (never sent, never charged) and, once its budget is spent, closes
/// its own admission. The ledger holds exactly the winner.
// T26 T29
#[tokio::test]
async fn two_starts_racing_for_one_hosts_capacity_admit_exactly_one() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    {
        // Room for one cold footprint (10 GiB), not two.
        let o = owner.lock().unwrap();
        let mut controls = o.store().resource_policy("lab").unwrap().unwrap().controls;
        controls.domains.get_mut("unified").unwrap().managed_limit = 15_i64 << 30;
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                1,
                "one-fits",
                &controls,
                &observations,
                1800,
            )
            .unwrap();
    }
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (a, b) = (Gate::new(false), Gate::new(false));
    a.release.add_permits(1);
    b.release.add_permits(1);
    let w = lane_worker(
        owner.clone(),
        Arc::new(HeldObservation {
            entered: entered.clone(),
            release: release.clone(),
            observations,
        }),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(20),
            ..Default::default()
        },
    );
    let first = w.start(&fence, 60_000).unwrap();
    let second = w.start(&other, 60_000).unwrap();
    // Both starts are past discovery and observing the host at once.
    tokio::time::timeout(Duration::from_secs(30), entered.acquire_many(2))
        .await
        .unwrap()
        .unwrap()
        .forget();
    release.add_permits(64);
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let (winner, loser) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let closed_first = admission_closed(&sql, &fence.deployment_id);
            let closed_second = admission_closed(&sql, &other.deployment_id);
            if closed_first || closed_second {
                assert!(!(closed_first && closed_second), "only the loser gives up");
                break if closed_second {
                    ((&fence, &first, &a), (&other, &b))
                } else {
                    ((&other, &second, &b), (&fence, &a))
                };
            }
            assert_eq!(w.status(), WorkerStatus::Running);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the refused start gives up within its budget");
    assert_eq!(
        winner.1.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(
        *winner.2.calls.lock().unwrap(),
        vec![RuntimeAction::Initialize]
    );
    assert!(
        loser.1.calls.lock().unwrap().is_empty(),
        "the loser was never sent"
    );
    assert_eq!(owners(&owner), vec![winner.0.deployment_id.clone()]);
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store()
            .runtime_binding(&loser.0.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "reserved"
    );
    assert_eq!(w.status(), WorkerStatus::Running);
    drop((first, second));
    w.shutdown().await.unwrap();
}

/// ADR 0015 invariant 4 (runbook limitation 4): a failed start waiting out its
/// retry cooldown is a hold on that start, not a sleep of the worker. Another
/// deployment starts meanwhile, and the held start can still be stopped.
// T10
#[tokio::test]
async fn a_retry_cooldown_holds_only_its_own_start() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let quick = Gate::new(false);
    quick.release.add_permits(1);
    // No scripted engine for `fence`: its driver cannot be built, a failure
    // known not to have landed, retried after the default 30 s cooldown.
    let w = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([(other.deployment_id.clone(), quick.clone())]),
        CoordinatorOptions::default(),
    );
    let failing = w.start(&fence, 60_000).unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while sql
            .query_row(
                "SELECT COUNT(*) FROM deployment_attempts WHERE deployment_id=?1",
                [&fence.deployment_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 0
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the first attempt is recorded");
    let began = Instant::now();
    let quick_start = w.start(&other, 60_000).unwrap();
    assert_eq!(
        quick_start.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    // The held start is still planned, and a Stop of it completes at once:
    // nothing was armed, so its binding is released without any effect.
    w.commands()
        .stop(
            "owner",
            &fence.deployment_id,
            fence.revision,
            "stop-held",
            60_000,
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while owner
            .lock()
            .unwrap()
            .store()
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Stop of a held start completes");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "work waited {:?} behind another deployment's cooldown",
        began.elapsed()
    );
    assert_eq!(owners(&owner), vec![other.deployment_id.clone()]);
    assert_eq!(w.status(), WorkerStatus::Running);
    drop(failing);
    w.shutdown().await.unwrap();
}

/// ADR 0015 invariant 6: shutdown with two Initializes in flight cancels and
/// joins both before ownership can be reacquired; each is recorded uncertain
/// with its reservation retained, and a restarted worker resends neither.
// T33
#[tokio::test]
async fn shutdown_joins_every_in_flight_initialize_and_restart_resends_none() {
    let (dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let (a, b) = (Gate::new(false), Gate::new(false));
    let w = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations.clone())),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    let first = w.start(&fence, 60_000).unwrap();
    a.entered().await;
    let second = w.start(&other, 60_000).unwrap();
    b.entered().await;
    assert!(a.active.load(Ordering::SeqCst) && b.active.load(Ordering::SeqCst));
    drop((first, second));
    drop(owner);
    assert!(crate::ownership::OwnedCoordinatorState::open(dir.path()).is_err());
    assert!(matches!(
        w.shutdown().await.unwrap(),
        WorkerStatus::Uncertain { .. }
    ));
    assert!(!a.active.load(Ordering::SeqCst) && !b.active.load(Ordering::SeqCst));
    let owner = Arc::new(Mutex::new(
        crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
    ));
    assert_peak(&owner, &fence);
    assert_peak(&owner, &other);
    let (c, d) = (Gate::new(false), Gate::new(false));
    let restarted = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), c.clone()),
            (other.deployment_id.clone(), d.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(c.calls.lock().unwrap().is_empty() && d.calls.lock().unwrap().is_empty());
    assert_peak(&owner, &fence);
    assert_peak(&owner, &other);
    assert_eq!(*a.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    assert_eq!(*b.calls.lock().unwrap(), vec![RuntimeAction::Initialize]);
    restarted.shutdown().await.unwrap();
}

/// ADR 0015: the bound on concurrent activations is enforced, and a start past
/// it waits for a slot rather than being refused.
// T29
#[tokio::test]
async fn the_activation_bound_queues_starts_past_it() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let (a, b) = (Gate::new(false), Gate::new(false));
    b.release.add_permits(1);
    let w = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), a.clone()),
            (other.deployment_id.clone(), b.clone()),
        ]),
        CoordinatorOptions {
            max_concurrent_effects: 1,
            ..Default::default()
        },
    );
    let first = w.start(&fence, 60_000).unwrap();
    a.entered().await;
    let second = w.start(&other, 60_000).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(b.calls.lock().unwrap().is_empty(), "no slot, no send");
    a.release.add_permits(1);
    assert_eq!(
        first.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    assert_eq!(
        second.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    w.shutdown().await.unwrap();
}

/// ADR 0015: the bound is validated like every other option.
#[tokio::test]
async fn the_activation_bound_must_be_positive_and_bounded() {
    for bound in [0, 257] {
        let (_dir, owner, _fence, observations) = setup().await;
        assert!(matches!(
            OwnedCoordinator::spawn(
                owner,
                Arc::new(Observations(observations)),
                Arc::new(|| Ok(1900)),
                CoordinatorOptions {
                    max_concurrent_effects: bound,
                    ..Default::default()
                },
                Arc::new(|_| Err(CoordinatorError::Service("unused".into()))),
            ),
            Err(CoordinatorError::Invalid)
        ));
    }
}

/// A worker whose `failing` deployment's cleanup never proves anything: the
/// host dropped while its armed Stop was on the wire. Every other deployment
/// gets the ordinary scripted engine.
fn dropped_host_worker(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    gates: BTreeMap<String, Arc<Gate>>,
    failing: String,
) -> OwnedCoordinator {
    OwnedCoordinator::spawn(
        owner,
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
        Arc::new(move |work| {
            let deployment = &work.fence().deployment_id;
            let gate = gates.get(deployment).cloned().ok_or_else(|| {
                CoordinatorError::Service("no scripted engine for this deployment".into())
            })?;
            if *deployment != failing {
                return Ok(test_driver(gate));
            }
            Ok(Arc::new(Driver {
                engine: gate,
                cleanup: Arc::new(|_| {
                    Box::pin(async {
                        Err(CoordinatorError::Service(
                            "host connection dropped during cleanup".into(),
                        ))
                    })
                }),
                tools: None,
                settle: None,
            }))
        }),
    )
    .unwrap()
}

/// Review finding (ADR 0015 consequence "a halt is still worker-wide"; ADR
/// 0011 decision 4): a host that drops in the middle of an armed Stop left the
/// cleanup unclassifiable, and the scheduler halted the whole worker, which
/// cancelled every other instance's load. The unproven cleanup now retains its
/// binding uncertain and pauses new activations like an uncertain Initialize,
/// while the other deployment's in-flight load continues to Ready. SPEC §6.1:
/// nothing is released without evidence.
// T20 T21
#[tokio::test]
async fn a_dropped_host_mid_stop_pauses_its_binding_and_other_loads_continue() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let stopping = Gate::new(false);
    stopping.release.add_permits(1);
    let loading = Gate::new(false);
    let w = dropped_host_worker(
        owner.clone(),
        observations,
        BTreeMap::from([
            (fence.deployment_id.clone(), stopping.clone()),
            (other.deployment_id.clone(), loading.clone()),
        ]),
        fence.deployment_id.clone(),
    );
    let ready = w.start(&fence, 60_000).unwrap();
    assert_eq!(
        ready.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let load = w.start(&other, 60_000).unwrap();
    loading.entered().await;
    let stop = w.stop("owner", &fence, "stop-dropped", 60_000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    // The armed cleanup is retained, not completed and not released.
    assert_eq!(
        stop.wait(Duration::from_millis(300)).await.unwrap_err().to_string(),
        CoordinatorError::CallerTimeout.to_string()
    );
    assert!(loading.active.load(Ordering::SeqCst), "the other load kept running");
    loading.release.add_permits(1);
    assert_eq!(
        load.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    // Both reservations stay charged: the dropped host's until evidence.
    let mut expected = vec![fence.deployment_id.clone(), other.deployment_id.clone()];
    expected.sort();
    let mut held = owners(&owner);
    held.sort();
    assert_eq!(held, expected);
    // The pause holds new activations until that binding is settled.
    assert!(matches!(
        w.start(&other, 60_000),
        Err(CoordinatorError::Stopped(_))
    ));
    assert!(matches!(w.status(), WorkerStatus::Uncertain { .. }));
    drop((ready, load, stop));
    assert!(matches!(
        w.shutdown().await.unwrap(),
        WorkerStatus::Uncertain { .. }
    ));
}

/// ADR 0015 follow-up (SPEC §13.2, ADR 0011 decision 4): an armed Stop whose
/// host dropped mid-send used to wait, uncertain and pausing every activation,
/// until a restarted coordinator adopted it. The scheduler now re-plans it in
/// its own session after the retry cooldown; once the host answers, one fresh
/// fenced Terminate completes it on gone evidence, the reservation is released
/// on that evidence, and activations resume.
// T20 T32 T10
#[tokio::test]
async fn an_unproven_cleanup_is_retried_in_session_and_lifts_its_pause() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let gate = Gate::new(false);
    gate.release.add_permits(1);
    let loading = Gate::new(false);
    loading.release.add_permits(1);
    // The host is unreachable for the first Terminate and answers afterwards.
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failing = fence.deployment_id.clone();
    let gates = BTreeMap::from([
        (fence.deployment_id.clone(), gate.clone()),
        (other.deployment_id.clone(), loading.clone()),
    ]);
    let counted = attempts.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
        Arc::new(move |work| {
            let deployment = &work.fence().deployment_id;
            let gate = gates.get(deployment).cloned().ok_or_else(|| {
                CoordinatorError::Service("no scripted engine for this deployment".into())
            })?;
            if *deployment != failing {
                return Ok(test_driver(gate));
            }
            let real = test_driver(gate.clone());
            let counted = counted.clone();
            Ok(Arc::new(Driver {
                engine: gate,
                cleanup: Arc::new(move |context| {
                    let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
                    let real = real.clone();
                    Box::pin(async move {
                        if first {
                            return Err(CoordinatorError::Service(
                                "host connection dropped during cleanup".into(),
                            ));
                        }
                        (real.cleanup)(context).await
                    })
                }),
                tools: None,
                settle: None,
            }))
        }),
    )
    .unwrap();
    let ready = w.start(&fence, 60_000).unwrap();
    assert_eq!(
        ready.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let stop = w.stop("owner", &fence, "stop-retried", 60_000).unwrap();
    assert_eq!(
        stop.wait(Duration::from_secs(10)).await.unwrap(),
        OrdinaryCleanupStatus::Completed
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry, no more");
    assert!(!owners(&owner).contains(&fence.deployment_id), "released on evidence");
    // The pause is lifted in session: a new activation is admitted.
    let started = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if w.status() == WorkerStatus::Running {
                if let Ok(start) = w.start(&other, 60_000) {
                    return start;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("activations resume once the cleanup settles");
    assert_eq!(
        started.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let retried = owner
        .lock()
        .unwrap()
        .store()
        .journal_evidence_of(&fence.deployment_id)
        .unwrap()
        .iter()
        .filter(|entry| entry.contains("retried in this session"))
        .count();
    assert_eq!(retried, 1);
    drop((ready, stop, started));
    w.shutdown().await.unwrap();
}

/// Review finding: an uncertain Initialize paused new activations only when
/// the scheduler applied the task's outcome, so a start command could be
/// admitted after the store already recorded the launch uncertain. The pause
/// is now taken in the same owner-mutex critical section that records it.
// T20
#[tokio::test]
async fn an_uncertain_initialize_pauses_admission_as_it_is_recorded() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let hung = Gate::new(false);
    let w = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([(fence.deployment_id.clone(), hung.clone())]),
        CoordinatorOptions::default(),
    );
    let hung_start = w.start(&fence, 2_900).unwrap();
    hung.entered().await;
    assert_eq!(
        hung_start.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Uncertain
    );
    assert!(
        matches!(
            w.start(&other, 60_000),
            Err(CoordinatorError::Stopped(_))
        ),
        "a start was admitted after the launch was recorded uncertain"
    );
    drop(hung_start);
    w.shutdown().await.unwrap();
}

/// Review finding: a cleanup whose runtime this worker does not retain was
/// raised as `Stopped`, read as shutdown, and halted the worker, cancelling
/// every other instance's in-flight effect. It is a distinct, unclassifiable
/// cleanup: the binding stays uncertain with its reservation charged (SPEC
/// §6.1), new activations pause, and another instance's load in flight still
/// reaches Ready.
// T20
#[tokio::test]
async fn a_cleanup_without_a_retained_runtime_pauses_instead_of_halting() {
    let (_dir, owner, fence, observations) = setup().await;
    let other = fixture::owned_source().await.other.clone();
    let first = Gate::new(false);
    first.release.add_permits(1);
    let loading = Gate::new(false);
    let w = lane_worker(
        owner.clone(),
        Arc::new(Observations(observations)),
        BTreeMap::from([
            (fence.deployment_id.clone(), first.clone()),
            (other.deployment_id.clone(), loading.clone()),
        ]),
        CoordinatorOptions::default(),
    );
    let ready = w.start(&fence, 60_000).unwrap();
    assert_eq!(
        ready.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    // The runtime built for the Ready binding is no longer retained here.
    let binding = owner
        .lock()
        .unwrap()
        .store()
        .runtime_binding(&fence.deployment_id)
        .unwrap()
        .unwrap()
        .id;
    assert!(w.shared.retained.lock().unwrap().remove(&binding).is_some());
    let load = w.start(&other, 60_000).unwrap();
    loading.entered().await;
    let stop = w.stop("owner", &fence, "stop-unretained", 60_000).unwrap();
    assert!(matches!(stopped(&w).await, WorkerStatus::Uncertain { .. }));
    loading.release.add_permits(1);
    assert_eq!(
        load.wait(Duration::from_secs(10)).await.unwrap(),
        InitializeStatus::Completed
    );
    let mut held = owners(&owner);
    held.sort();
    let mut expected = vec![fence.deployment_id.clone(), other.deployment_id.clone()];
    expected.sort();
    assert_eq!(held, expected, "nothing is released without evidence");
    assert!(matches!(w.status(), WorkerStatus::Uncertain { .. }));
    drop((ready, load, stop));
    w.shutdown().await.unwrap();
}

/// Review finding (SPEC §6.5, §17): a start deferred while parked instances
/// are reclaimed journaled `start_deferred` and was retried every poll (100 ms)
/// for as long as the reclaim took. The reason is now journaled once per
/// distinct reason, and the hold backs off while the same reason repeats, up
/// to a bound; a new reason, or a start that stops being deferred, starts over.
// T16
#[tokio::test]
async fn a_repeated_deferral_is_journaled_once_and_backs_off() {
    let (_dir, owner, _fence, observations) = setup().await;
    let w = lane_worker(
        owner,
        Arc::new(Observations(observations)),
        BTreeMap::new(),
        CoordinatorOptions::default(),
    );
    let poll = CoordinatorOptions::default().poll_interval;
    let (new, first) = w.shared.defer("binding", "reclaiming parked instances");
    assert!(new);
    assert_eq!(first, poll);
    let mut last = first;
    for _ in 0..8 {
        let (new, hold) = w.shared.defer("binding", "reclaiming parked instances");
        assert!(!new, "the same reason is journaled once");
        assert!(hold >= last, "the hold never shrinks while the reason repeats");
        last = hold;
    }
    assert!(last > poll, "the hold backed off");
    assert!(last <= Duration::from_secs(2), "the hold is bounded");
    let (new, hold) = w.shared.defer("binding", "another reason");
    assert!(new, "a different reason is journaled");
    assert_eq!(hold, poll);
    w.shared.forget_deferral("binding");
    assert!(w.shared.defer("binding", "another reason").0);
    w.shutdown().await.unwrap();
}
