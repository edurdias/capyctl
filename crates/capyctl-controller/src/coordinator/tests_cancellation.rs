//! SPEC §10 (amended 2026-10-01): a request whose client hung up keeps its
//! lease charged as cancelling until the engine reports quiescence, read by
//! the real worker against the switching lab's scripted engine.
//!
//! CPU and scripted engines only; nothing here qualifies a native engine.
use super::*;
use crate::request_leases::{LeaseEnd, RequestLease};

/// Long enough for several 250 ms quiescence ticks.
const TICKS: Duration = Duration::from_millis(600);

struct Harness {
    lab: Lab,
    port: CoordinatorLifecycle,
    generation: i64,
}

/// The lab with deployment A READY; its engine answers `quiet` to the
/// quiescence question.
async fn harness_with_ready_instance(quiet: bool) -> Harness {
    let lab = lab(15, 50).await;
    lab.ready(&lab.a).await;
    lab.engine.quiet.store(quiet, Ordering::SeqCst);
    let generation = lab.instance(&lab.a.deployment_id, 0).2.unwrap();
    let port = lab.port(Duration::from_secs(10));
    Harness {
        lab,
        port,
        generation,
    }
}

impl Harness {
    async fn open_lease(&self) -> RequestLease {
        self.port
            .open_instance_lease(&self.lab.a.deployment_id, self.generation, 32)
            .await
            .unwrap()
            .unwrap()
    }
    async fn close_lease(&self, lease: RequestLease, end: LeaseEnd) {
        self.port.close_request_lease(lease, end).await.unwrap();
    }
    fn set_quiet(&self, quiet: bool) {
        self.lab.engine.quiet.store(quiet, Ordering::SeqCst);
    }
    fn outstanding(&self) -> i64 {
        leases(&self.lab, &self.lab.a.deployment_id)
    }
    fn acknowledged(&self) -> i64 {
        self.lab
            .sql()
            .query_row(
                "SELECT COUNT(*) FROM management_events WHERE kind='request_cancellation_acknowledged' AND deployment_id=?1",
                [&self.lab.a.deployment_id],
                |r| r.get(0),
            )
            .unwrap()
    }
    async fn until_outstanding(&self, count: i64) {
        let what = format!("{count} outstanding lease(s)");
        until(&what, || self.outstanding() == count).await;
    }
}

// T17, SPEC §10 (amended): a cancelling lease closes once the engine reads
// quiescent, and not before.
#[tokio::test]
async fn a_cancelling_lease_closes_on_engine_quiescence() {
    let h = harness_with_ready_instance(false).await;
    let lease = h.open_lease().await;
    h.close_lease(lease, LeaseEnd::Cancelling).await;
    until("a second quiescence question", || {
        h.lab.engine.asked.lock().unwrap().len() > 1
    })
    .await;
    assert_eq!(h.outstanding(), 1, "busy engine: still charged");
    h.set_quiet(true);
    h.until_outstanding(0).await;
    assert_eq!(h.acknowledged(), 1);
    h.lab.worker.shutdown().await.unwrap();
}

// T17: quiescence is engine-wide. A neighbour completing on the router does
// not close the cancelled request while the engine itself still reads busy.
#[tokio::test]
async fn a_completed_neighbour_does_not_close_a_cancelling_lease_on_a_busy_engine() {
    let h = harness_with_ready_instance(false).await;
    let other = h.open_lease().await;
    let hung_up = h.open_lease().await;
    h.close_lease(hung_up, LeaseEnd::Cancelling).await;
    tokio::time::sleep(TICKS).await;
    assert_eq!(h.outstanding(), 2);
    h.close_lease(other, LeaseEnd::Completed).await;
    tokio::time::sleep(TICKS).await;
    assert_eq!(h.outstanding(), 1, "busy engine: the cancelled one stays");
    assert_eq!(h.acknowledged(), 0);
    h.set_quiet(true);
    h.until_outstanding(0).await;
    h.lab.worker.shutdown().await.unwrap();
}

// T17: an idle sample the engine took before the hang-up closes nothing; one
// taken at or after it closes the lease. An ordinary lease is never closed by
// quiescence.
#[tokio::test]
async fn an_idle_sample_before_the_hang_up_closes_nothing() {
    let h = harness_with_ready_instance(true).await;
    let running = h.open_lease().await;
    tokio::time::sleep(TICKS).await;
    assert_eq!(h.outstanding(), 1, "an uncancelled lease stays charged");
    assert!(h.lab.engine.asked.lock().unwrap().is_empty());
    let lease = h.open_lease().await;
    let before = capyctl_protocol::now_unix_ms() - 1;
    *h.lab.engine.idle_at.lock().unwrap() = Some(before);
    tokio::time::sleep(Duration::from_millis(5)).await;
    h.close_lease(lease, LeaseEnd::Cancelling).await;
    tokio::time::sleep(TICKS).await;
    assert!(!h.lab.engine.asked.lock().unwrap().is_empty());
    assert_eq!(h.outstanding(), 2, "an idle sample before the hang-up");
    *h.lab.engine.idle_at.lock().unwrap() = Some(capyctl_protocol::now_unix_ms());
    h.until_outstanding(1).await;
    assert_eq!(h.acknowledged(), 1);
    h.close_lease(running, LeaseEnd::Completed).await;
    h.lab.worker.shutdown().await.unwrap();
}

// T17: an engine that never answers its quiescence question keeps its
// cancelled lease charged and is asked once at a time; it never delays the
// scheduler, so another deployment still starts promptly.
#[tokio::test]
async fn a_wedged_engine_never_delays_a_start() {
    let lab = lab(30, 50).await;
    lab.ready(&lab.a).await;
    lab.engine.wedged.store(true, Ordering::SeqCst);
    let generation = lab.instance(&lab.a.deployment_id, 0).2.unwrap();
    let h = Harness {
        port: lab.port(Duration::from_secs(10)),
        lab,
        generation,
    };
    let lease = h.open_lease().await;
    h.close_lease(lease, LeaseEnd::Cancelling).await;
    until("a quiescence question", || {
        !h.lab.engine.asked.lock().unwrap().is_empty()
    })
    .await;
    let started = std::time::Instant::now();
    h.lab.ready(&h.lab.c).await;
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "the start waited {:?} behind a wedged engine",
        started.elapsed()
    );
    assert_eq!(
        h.lab.engine.asked.lock().unwrap().len(),
        1,
        "one question in flight"
    );
    assert_eq!(h.outstanding(), 1, "uncertainty retains accounting");
    h.lab.worker.shutdown().await.unwrap();
}

// T17: settlement does not wait for an uncertain launch elsewhere; a paused
// launch pauses new activations, not the closing of cancelled requests.
#[tokio::test]
async fn a_cancelling_lease_settles_while_a_launch_is_paused() {
    let h = harness_with_ready_instance(false).await;
    let lease = h.open_lease().await;
    h.close_lease(lease, LeaseEnd::Cancelling).await;
    h.lab.worker.shared.pause(Paused {
        binding: "an-uncertain-launch".into(),
        status: WorkerStatus::Uncertain {
            operation_id: "op".into(),
            reason: "an uncertain launch".into(),
        },
        retry: None,
    });
    h.set_quiet(true);
    h.until_outstanding(0).await;
    assert_eq!(h.acknowledged(), 1);
    h.lab.worker.shared.unpause("an-uncertain-launch");
    h.lab.worker.shutdown().await.unwrap();
}

// T17 T16: a switch release waits for a cancelling lease, then proceeds.
#[tokio::test]
async fn a_switch_waits_for_a_cancelling_lease() {
    let h = harness_with_ready_instance(false).await;
    let lease = h.open_lease().await;
    h.close_lease(lease, LeaseEnd::Cancelling).await;
    let b = h.lab.c.deployment_id.clone();
    let switch = {
        let port = h.lab.port(Duration::from_secs(10));
        let b = b.clone();
        tokio::spawn(async move { port.activate_for_request(&b).await })
    };
    tokio::time::sleep(TICKS).await;
    assert!(
        !switch.is_finished(),
        "the switch drains the cancelling lease"
    );
    assert_eq!(h.lab.state(&h.lab.a.deployment_id), "ready");
    assert!(h.lab.operations(&h.lab.a.deployment_id, "park").is_empty());
    h.set_quiet(true);
    tokio::time::timeout(Duration::from_secs(30), switch)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(h.outstanding(), 0);
    assert_eq!(h.lab.state(&b), "ready");
    assert_eq!(h.lab.state(&h.lab.a.deployment_id), "parked");
    h.lab.worker.shutdown().await.unwrap();
}

// T17: an engine that leaves its question unanswered is asked again after one
// second, doubling while it stays silent, never more than 30 s apart.
#[test]
fn an_unanswered_question_backs_off_up_to_a_cap() {
    let waits: Vec<u64> = (1..=7).map(|n| quiescence_backoff(n).as_secs()).collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
    assert_eq!(quiescence_backoff(u32::MAX).as_secs(), 30);
}
