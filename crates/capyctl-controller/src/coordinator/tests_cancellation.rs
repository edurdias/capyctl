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
    // One question at a time: a second one means the first answer, the idle
    // sample from before the hang-up, was applied.
    until("a second quiescence question", || {
        h.lab.engine.asked.lock().unwrap().len() > 1
    })
    .await;
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
    // A scheduler held by the question would wait out its 3 s bound; the
    // margin below it is for a slow runner, not for the scheduler.
    assert!(
        started.elapsed() < Duration::from_millis(2_500),
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

/// ADR 0031 §6: a custom engine profile, with an idle signal when `idle`.
fn custom_profile(idle: bool) -> Value {
    let mut manifest = json!({
        "kind": "custom_engine",
        "schema_version": 1,
        "api": "chat",
        "version": "2.2.0",
        "args": ["--host", "{host}", "--port", "{port}", "--model", "{model_dir}"],
    });
    if idle {
        manifest["idle"] = json!({"url": "/metrics", "gauges": ["requests_running"]});
    }
    let mut profile = capyctl_config::registration::custom_profile_document(
        &capyctl_config::registration::CustomProfileSpec {
            executable: "/bin/true".into(),
            custom: capyctl_config::custom::CustomProfile::new(
                capyctl_config::custom::CustomManifest::from_value(manifest).unwrap(),
            ),
            installation_drift: capyctl_config::effective::InstallationDrift::Warn,
            env: Default::default(),
            approved_env: Vec::new(),
        },
    );
    // Every launch seals an inference key (SPEC §13.3).
    profile["security"]["credential_ref"] = json!("secret://engine-key");
    profile
}

/// A stopped custom deployment on the lab's host: the vLLM golden's model
/// and resources on a custom profile, restart-only.
fn deploy_custom(lab: &Lab, name: &str, idle: bool) -> DeploymentFence {
    let source: Value = serde_json::from_str(VLLM).unwrap();
    let mut host = source["input"]["host"].clone();
    host["runtime_profiles"]["local"] = custom_profile(idle);
    host["resource_policy"]["domains"]["unified"]["managed_limit"] =
        json!(format!("{}GiB", lab.managed_gib));
    host["resource_policy"]["queue"]["admission_window"] = json!(format!("{}ms", lab.window_ms));
    let mut deployment = source["input"]["deployment"].clone();
    deployment["name"] = json!(name);
    deployment["routes"] = json!([name]);
    deployment["runtime_profile_revision"] = json!(1);
    deployment["engine_config"] = json!({});
    deployment["residency"] = json!("restart_only");
    let o = lab.owner.lock().unwrap();
    let receipt = o
        .store()
        .create_stopped_managed_configuration(
            o.session(),
            "owner",
            name,
            &json!({ "config": deployment }).to_string(),
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

/// The lab with one READY custom deployment whose engine would answer every
/// quiescence question quiescent, and a router port on it.
async fn custom_ready(
    idle: bool,
    options: CoordinatorOptions,
) -> (Lab, DeploymentFence, CoordinatorLifecycle, i64) {
    let lab = lab_options(15, 50, None, options).await;
    let fence = deploy_custom(&lab, "custom", idle);
    lab.ready(&fence).await;
    lab.engine.quiet.store(true, Ordering::SeqCst);
    let generation = lab.instance(&fence.deployment_id, 0).2.unwrap();
    let port = lab.port(Duration::from_secs(10));
    (lab, fence, port, generation)
}

/// The acknowledgements journaled for `deployment`, as payloads.
fn acknowledgements(lab: &Lab, deployment: &str) -> Vec<Value> {
    let sql = lab.sql();
    let mut statement = sql
        .prepare("SELECT payload_json FROM management_events WHERE kind='request_cancellation_acknowledged' AND deployment_id=?1")
        .unwrap();
    statement
        .query_map([deployment], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

// T44 T17 (ADR 0031 §6, SPEC §10): on an engine that reports no idle signal a
// cancelling lease is never settled while the engine runs: the engine is
// never asked, even one that would answer quiescent.
#[tokio::test]
async fn a_cancellation_without_an_idle_signal_is_never_settled_while_the_engine_runs() {
    let (lab, fence, port, generation) = custom_ready(false, CoordinatorOptions::default()).await;
    let lease = port
        .open_instance_lease(&fence.deployment_id, generation, 32)
        .await
        .unwrap()
        .unwrap();
    port.close_request_lease(lease, LeaseEnd::Cancelling)
        .await
        .unwrap();
    tokio::time::sleep(TICKS * 2).await;
    assert!(lab.engine.asked.lock().unwrap().is_empty(), "never asked");
    assert_eq!(leases(&lab, &fence.deployment_id), 1, "still charged");
    assert!(acknowledgements(&lab, &fence.deployment_id).is_empty());
    lab.worker.shutdown().await.unwrap();
}

// T44 T17 (ADR 0031 §6): with an idle signal a custom engine is asked, and
// its idle reading settles the cancellation as for a built-in engine.
#[tokio::test]
async fn a_cancellation_with_an_idle_signal_settles_on_the_reading() {
    let (lab, fence, port, generation) = custom_ready(true, CoordinatorOptions::default()).await;
    let lease = port
        .open_instance_lease(&fence.deployment_id, generation, 32)
        .await
        .unwrap()
        .unwrap();
    port.close_request_lease(lease, LeaseEnd::Cancelling)
        .await
        .unwrap();
    until("the idle reading to settle it", || {
        leases(&lab, &fence.deployment_id) == 0
    })
    .await;
    assert!(!lab.engine.asked.lock().unwrap().is_empty());
    assert_eq!(acknowledgements(&lab, &fence.deployment_id).len(), 1);
    lab.worker.shutdown().await.unwrap();
}

// T44 T17 T10 (ADR 0031 §6, SPEC §10): a stop does not wait out its drain
// bound for a cancellation no idle signal can settle; the stop's verified
// exit settles it, journaled once with a receipt naming the stop.
#[tokio::test]
async fn a_stop_does_not_wait_for_a_cancellation_without_an_idle_signal() {
    let (lab, fence, port, generation) = custom_ready(
        false,
        CoordinatorOptions {
            stop_drain_timeout: Duration::from_secs(20),
            ..Default::default()
        },
    )
    .await;
    let id = fence.deployment_id.clone();
    let lease = port
        .open_instance_lease(&id, generation, 32)
        .await
        .unwrap()
        .unwrap();
    port.close_request_lease(lease, LeaseEnd::Cancelling)
        .await
        .unwrap();
    assert_eq!(leases(&lab, &id), 1);
    let started = std::time::Instant::now();
    let stop = lab
        .worker
        .commands()
        .administrative_stop("operator", &id, fence.revision, "stop", 100_000)
        .unwrap();
    until_succeeded(&lab, stop.operation_id(), &id).await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the stop waited {:?} for a cancellation",
        started.elapsed()
    );
    assert_eq!(leases(&lab, &id), 0, "the verified exit settles it");
    assert_eq!(lab.state(&id), "stopped");
    let acknowledged = acknowledgements(&lab, &id);
    assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
    assert_eq!(acknowledged[0]["count"], json!(1));
    let receipt = acknowledged[0]["receipt"].as_str().unwrap();
    assert!(receipt.contains(stop.operation_id()), "{receipt}");
    assert!(lab.engine.asked.lock().unwrap().is_empty());
    lab.worker.shutdown().await.unwrap();
}

// T44 T17 T16 (ADR 0031 §6, SPEC §10): a switch away from an engine that
// reports no idle signal does not wait for its cancellation: the victim is
// released by a stop at once, and that stop's verified exit settles it.
#[tokio::test]
async fn a_switch_does_not_wait_for_a_cancellation_without_an_idle_signal() {
    let (lab, fence, port, generation) = custom_ready(false, CoordinatorOptions::default()).await;
    let id = fence.deployment_id.clone();
    let lease = port
        .open_instance_lease(&id, generation, 32)
        .await
        .unwrap()
        .unwrap();
    port.close_request_lease(lease, LeaseEnd::Cancelling)
        .await
        .unwrap();
    let b = lab.c.deployment_id.clone();
    // A drain that waited for the cancellation would fail at this bound.
    tokio::time::timeout(
        Duration::from_secs(30),
        lab.port(Duration::from_secs(5)).activate_for_request(&b),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(lab.state(&b), "ready");
    assert_eq!(lab.state(&id), "stopped");
    assert!(lab.operations(&id, "park").is_empty());
    assert_eq!(leases(&lab, &id), 0);
    assert_eq!(acknowledgements(&lab, &id).len(), 1);
    lab.worker.shutdown().await.unwrap();
}
