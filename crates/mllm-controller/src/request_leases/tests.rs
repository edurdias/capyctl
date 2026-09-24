use super::*;
use mllm_domain::{DeploymentId, LifecycleState, OperationId};
use mllm_store::deployments::AcceptDeployment;
use mllm_store::Store;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A file-backed store with one Ready deployment whose dispatch gate is open, and
/// a backend applying batches to it under the store's current session. File
/// backed so a commit pays the same journal cost as production.
struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Arc<Mutex<Store>>,
    session: mllm_store::dispatch::CoordinatorSession,
    deployment: String,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("leases.sqlite3");
    let store = Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let id = DeploymentId::new();
    store
        .accept_deployment(AcceptDeployment {
            id,
            name: "a".into(),
            kind: "model".into(),
            route_model_id: Some("a".into()),
            desired_state: LifecycleState::Ready,
            schema_version: 1,
            idempotency_key: "a".into(),
            initial_operation_id: OperationId("op-a".into()),
        })
        .unwrap();
    store
        .set_observed_state(&id.to_string(), LifecycleState::Ready)
        .unwrap();
    // Fixture-only readiness: production opens the gate on completion evidence.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE deployments SET dispatch_enabled=1 WHERE id=?1",
            [id.to_string()],
        )
        .unwrap();
    Fixture {
        _directory: directory,
        path,
        store: Arc::new(Mutex::new(store)),
        session,
        deployment: id.to_string(),
    }
}

impl Fixture {
    fn backend(&self, calls: Arc<Mutex<Vec<usize>>>) -> LeaseBackend {
        let store = self.store.clone();
        let session = self.session.clone();
        Arc::new(move |writes: &[LeaseWrite]| {
            calls.lock().unwrap().push(writes.len());
            store
                .lock()
                .unwrap()
                .apply_request_lease_batch(&session, writes)
                .map_err(|error| LifecycleFault::Unavailable(error.to_string()))
        })
    }

    fn leases(&self) -> Vec<(String, String)> {
        let conn = rusqlite::Connection::open(&self.path).unwrap();
        let mut statement = conn
            .prepare("SELECT id,disposition FROM request_leases ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    fn close_gate(&self) {
        rusqlite::Connection::open(&self.path)
            .unwrap()
            .execute(
                "UPDATE deployments SET dispatch_enabled=0 WHERE id=?1",
                [&self.deployment],
            )
            .unwrap();
    }
}

// T17 T19: a lease is durable before dispatch, deleted on completion or on
// proof the request never reached the engine, and retained as uncertain
// otherwise. Nothing closes it on a timer.
#[tokio::test]
async fn a_lease_is_durable_until_evidence_closes_it() {
    let f = fixture();
    let writer = RequestLeaseWriter::spawn(f.backend(Default::default()));
    let completed = writer.open(&f.deployment, 8).await.unwrap();
    let refused = writer.open(&f.deployment, 8).await.unwrap();
    let unknown = writer.open(&f.deployment, 8).await.unwrap();
    assert_eq!(f.leases().len(), 3);
    assert!(f.leases().iter().all(|(_, d)| d == "inflight"));
    let unknown_id = unknown.id().to_owned();
    writer.close(completed, LeaseEnd::Completed).await.unwrap();
    writer.close(refused, LeaseEnd::NotAccepted).await.unwrap();
    writer.close(unknown, LeaseEnd::Uncertain).await.unwrap();
    assert_eq!(f.leases(), vec![(unknown_id, "uncertain".to_owned())]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.leases().len(), 1, "no timer closes a retained lease");
}

// T18 T19: a closed gate refuses the grant as retryable, and the
// per-deployment bound counts uncertain leases too.
#[tokio::test]
async fn grants_are_refused_when_the_gate_is_closed_or_the_bound_is_reached() {
    let f = fixture();
    let writer = RequestLeaseWriter::spawn(f.backend(Default::default()));
    let first = writer.open(&f.deployment, 1).await.unwrap();
    writer.close(first, LeaseEnd::Uncertain).await.unwrap();
    assert_eq!(
        writer.open(&f.deployment, 1).await.unwrap_err(),
        LeaseRefused::Full
    );
    f.close_gate();
    assert_eq!(
        writer.open(&f.deployment, 8).await.unwrap_err(),
        LeaseRefused::Closed
    );
    assert_eq!(
        writer.open("no-such-deployment", 8).await.unwrap_err(),
        LeaseRefused::Closed
    );
}

// T19: a storage failure grants nothing and closes nothing; the caller learns
// the ledger was unavailable.
#[tokio::test]
async fn a_failed_batch_grants_and_closes_nothing() {
    let failing: LeaseBackend =
        Arc::new(|_: &[LeaseWrite]| Err(LifecycleFault::Unavailable("disk".into())));
    let writer = RequestLeaseWriter::spawn(failing);
    assert!(matches!(
        writer.open("d", 8).await,
        Err(LeaseRefused::Unavailable(_))
    ));
}

// T17 T19, SPEC §10: a grant whose caller stopped waiting (its future was
// dropped, for example a client disconnect or a caller timeout) never
// dispatched anything, because the ticket never reached the router. The writer
// closes that lease as never accepted instead of leaving it charged forever.
#[tokio::test]
async fn a_grant_whose_caller_went_away_is_closed_as_not_accepted() {
    let f = fixture();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let entered_tx = Mutex::new(entered_tx);
    let inner = f.backend(Default::default());
    let first = std::sync::atomic::AtomicBool::new(true);
    let held: LeaseBackend = Arc::new(move |writes: &[LeaseWrite]| {
        if first.swap(false, Ordering::SeqCst) {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = release_rx.lock().unwrap().recv();
        }
        inner(writes)
    });
    let writer = RequestLeaseWriter::spawn(held);
    let deployment = f.deployment.clone();
    {
        let grant = writer.open(&deployment, 8);
        tokio::pin!(grant);
        // Drive the grant until the writer holds it inside its batch, then
        // drop the future: the caller has gone away.
        tokio::select! {
            _ = &mut grant => panic!("the held batch cannot answer yet"),
            _ = tokio::task::spawn_blocking(move || entered_rx.recv().unwrap()) => {}
        }
    }
    release_tx.send(()).unwrap();
    let started = Instant::now();
    while !f.leases().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "an orphaned grant stayed charged: {:?}",
            f.leases()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The writer is still serving: a fresh grant lands and closes normally.
    let lease = writer.open(&f.deployment, 8).await.unwrap();
    writer.close(lease, LeaseEnd::Completed).await.unwrap();
    assert!(f.leases().is_empty());
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[rank]
}

// Owner decision 2026-09-22 (SPEC §10): group commit with bounded added
// latency. Micro-benchmark: 32 concurrent dispatchers, 40 dispatches each; each
// dispatch pays one grant and one close. The measured overhead is printed as
// p50/p99 (run with --nocapture) and bounded loosely so the test is not flaky
// on a loaded machine. It also proves the writes were grouped: fewer commits
// than writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_commit_bounds_the_latency_a_dispatch_pays() {
    const TASKS: usize = 32;
    const EACH: usize = 40;
    let f = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::new(RequestLeaseWriter::spawn(f.backend(calls.clone())));
    let started = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..TASKS {
        let writer = writer.clone();
        let deployment = f.deployment.clone();
        tasks.push(tokio::spawn(async move {
            let mut samples = Vec::with_capacity(EACH);
            for _ in 0..EACH {
                let begin = Instant::now();
                let lease = writer.open(&deployment, TASKS).await.unwrap();
                let opened = begin.elapsed();
                let closing = Instant::now();
                writer.close(lease, LeaseEnd::Completed).await.unwrap();
                samples.push(opened + closing.elapsed());
            }
            samples
        }));
    }
    let mut samples = Vec::new();
    for task in tasks {
        samples.extend(task.await.unwrap());
    }
    let wall = started.elapsed();
    samples.sort();
    let p50 = percentile(&samples, 0.50);
    let p99 = percentile(&samples, 0.99);
    // Copied out: the backend records into `calls`, so holding its lock across
    // the next dispatches would stall the writer.
    let calls = calls.lock().unwrap().clone();
    let writes: usize = calls.iter().sum();
    let largest = calls.iter().copied().max().unwrap_or(0);
    println!(
        "request lease overhead per dispatch (grant+close): p50={:?} p99={:?} \
         dispatches={} commits={} writes={} largest_batch={} wall={:?}",
        p50,
        p99,
        samples.len(),
        calls.len(),
        writes,
        largest,
        wall
    );
    assert_eq!(writes, TASKS * EACH * 2);
    assert!(calls.len() < writes, "concurrent writes share commits");
    assert!(largest <= MAX_BATCH);
    assert!(
        f.leases().is_empty(),
        "every completed dispatch closed its lease"
    );
    assert!(p99 < Duration::from_millis(500), "p99 overhead {p99:?}");

    // Uncontended: one dispatcher alone pays one commit per write.
    let mut alone = Vec::new();
    for _ in 0..200 {
        let begin = Instant::now();
        let lease = writer.open(&f.deployment, TASKS).await.unwrap();
        writer.close(lease, LeaseEnd::Completed).await.unwrap();
        alone.push(begin.elapsed());
    }
    alone.sort();
    let (p50, p99) = (percentile(&alone, 0.50), percentile(&alone, 0.99));
    println!("request lease overhead uncontended (grant+close): p50={p50:?} p99={p99:?}");
    assert!(p99 < Duration::from_millis(500), "p99 overhead {p99:?}");
}
