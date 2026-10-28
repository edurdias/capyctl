//! SPEC §10 (amended 2026-10-01): a request whose client hung up keeps its
//! lease charged as cancelling until the engine reports quiescence, read by
//! the real worker against the switching lab's scripted engine. Amended
//! 2026-10-21: only a request the engine answered before its cancellation
//! settles that way; any other waits for the instance's verified exit.
//!
//! CPU and scripted engines only; nothing here qualifies a native engine.
use super::*;
use crate::request_leases::{LeaseEnd, RequestLease};

/// Long enough for several 250 ms quiescence ticks.
const TICKS: Duration = Duration::from_millis(600);

/// A cancellation of a request the engine answered first (a stream's first
/// output, or its complete error answer).
const ANSWERED: LeaseEnd = LeaseEnd::Cancelling { answered: true };

/// A cancellation before the engine answered (a deadline cut while waiting
/// for the first output, or a collected request cut before its answer).
const UNANSWERED: LeaseEnd = LeaseEnd::Cancelling { answered: false };

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
    h.close_lease(lease, ANSWERED).await;
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
    h.close_lease(hung_up, ANSWERED).await;
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
    h.close_lease(lease, ANSWERED).await;
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
    h.close_lease(lease, ANSWERED).await;
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
    h.close_lease(lease, ANSWERED).await;
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
    h.close_lease(lease, ANSWERED).await;
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

// T17 T38 (SPEC §10, amended 2026-10-21): a request cut before any engine
// output (a deadline cut while waiting for its first token) stays charged
// through quiescent readings: the engine is never asked about it. Only the
// instance's verified stop settles it, journaled with a receipt naming the
// stop.
#[tokio::test]
async fn a_cut_before_any_engine_output_settles_only_at_the_verified_stop() {
    let h = harness_with_ready_instance(true).await;
    let lease = h.open_lease().await;
    h.close_lease(lease, UNANSWERED).await;
    tokio::time::sleep(TICKS * 2).await;
    assert!(h.lab.engine.asked.lock().unwrap().is_empty(), "never asked");
    assert_eq!(h.outstanding(), 1, "still charged on a quiescent engine");
    assert_eq!(h.acknowledged(), 0);
    let id = h.lab.a.deployment_id.clone();
    let stop = h
        .lab
        .worker
        .commands()
        .administrative_stop("operator", &id, h.lab.a.revision, "stop", 100_000)
        .unwrap();
    until_succeeded(&h.lab, stop.operation_id(), &id).await;
    assert_eq!(h.outstanding(), 0, "the verified exit settles it");
    let acknowledged = acknowledgements(&h.lab, &id);
    assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
    assert_eq!(acknowledged[0]["count"], json!(1));
    let receipt = acknowledged[0]["receipt"].as_str().unwrap();
    assert!(
        receipt.starts_with("cancelled before the engine answered"),
        "{receipt}"
    );
    assert!(receipt.contains(stop.operation_id()), "{receipt}");
    h.lab.worker.shutdown().await.unwrap();
}

// T17 T16 (SPEC §10, amended 2026-10-21): a switch does not wait for a
// cancellation the engine never answered: the victim is released by a stop,
// never a park, and that stop's verified exit settles it.
#[tokio::test]
async fn a_switch_releases_an_unanswered_cancellation_by_a_stop() {
    let h = harness_with_ready_instance(true).await;
    let lease = h.open_lease().await;
    h.close_lease(lease, UNANSWERED).await;
    let b = h.lab.c.deployment_id.clone();
    tokio::time::timeout(
        Duration::from_secs(30),
        h.lab.port(Duration::from_secs(5)).activate_for_request(&b),
    )
    .await
    .unwrap()
    .unwrap();
    let a = h.lab.a.deployment_id.clone();
    assert_eq!(h.lab.state(&b), "ready");
    assert_eq!(h.lab.state(&a), "stopped");
    assert!(h.lab.operations(&a, "park").is_empty());
    assert_eq!(h.outstanding(), 0);
    assert_eq!(acknowledgements(&h.lab, &a).len(), 1);
    assert!(h.lab.engine.asked.lock().unwrap().is_empty());
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
    port.close_request_lease(lease, ANSWERED).await.unwrap();
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
    port.close_request_lease(lease, ANSWERED).await.unwrap();
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
    port.close_request_lease(lease, ANSWERED).await.unwrap();
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
    port.close_request_lease(lease, ANSWERED).await.unwrap();
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

// T17 (ADR 0032 §4): the router's acknowledgement by lease ID closes the
// cancelling lease through the group-committed writer with one
// acknowledgement naming the evidence; a second one changes nothing, and a
// lease that is not cancelling is left charged.
#[tokio::test]
async fn the_authority_acknowledges_one_cancelled_request_by_its_lease() {
    let h = harness_with_ready_instance(false).await;
    let live = h.open_lease().await;
    let live_id = live.id().to_owned();
    let cancelled = h.open_lease().await;
    let id = cancelled.id().to_owned();
    h.close_lease(cancelled, UNANSWERED).await;
    assert!(!h
        .port
        .acknowledge_cancelled_request(&live_id)
        .await
        .unwrap());
    assert!(h.port.acknowledge_cancelled_request(&id).await.unwrap());
    assert_eq!(h.outstanding(), 1, "the live lease stays charged");
    assert!(!h.port.acknowledge_cancelled_request(&id).await.unwrap());
    let acknowledged = acknowledgements(&h.lab, &h.lab.a.deployment_id);
    assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
    assert_eq!(
        acknowledged[0]["receipt"],
        json!(format!(
            "the engine finished this request after it was asked to abort it; request lease {id}"
        ))
    );
    h.close_lease(live, LeaseEnd::Completed).await;
    h.lab.worker.shutdown().await.unwrap();
}

/// ADR 0032 §7: the deployment limit the router grants under.
const LIMIT: usize = capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT as usize;

/// The restarts for held requests journaled for `deployment`, as payloads.
fn restarts(lab: &Lab, deployment: &str) -> Vec<Value> {
    let sql = lab.sql();
    let mut statement = sql
        .prepare("SELECT payload_json FROM management_events WHERE kind='instance_restarted_for_held_requests' AND deployment_id=?1")
        .unwrap();
    statement
        .query_map([deployment], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

fn retained_binding(lab: &Lab, deployment: &str) -> String {
    lab.sql()
        .query_row(
            "SELECT id FROM runtime_bindings WHERE deployment_id=?1 AND state!='released'",
            [deployment],
            |r| r.get(0),
        )
        .unwrap()
}

// T17 (ADR 0032 §7, SPEC §10): requests cut before the engine answered them
// fill the deployment's limit, so a grant is refused full; the restart is
// accepted, its verified exit settles all of them with the stop's receipt,
// it is journaled with the instance and the count, and the instance serves
// again under a new incarnation.
#[tokio::test]
async fn held_requests_filling_the_limit_restart_the_instance_which_serves_again() {
    let h = harness_with_ready_instance(false).await;
    let id = h.lab.a.deployment_id.clone();
    for _ in 0..LIMIT {
        let lease = h.open_lease().await;
        h.close_lease(lease, UNANSWERED).await;
    }
    assert!(matches!(
        h.port.open_instance_lease(&id, h.generation, LIMIT).await,
        Err(crate::request_leases::LeaseRefused::Full)
    ));
    assert_eq!(
        h.port.restart_for_held_requests(&id).await.unwrap(),
        crate::port::HeldRestart::Restarting
    );
    h.until_outstanding(0).await;
    let restarted = restarts(&h.lab, &id);
    assert_eq!(restarted.len(), 1, "{restarted:?}");
    assert_eq!(restarted[0]["instance_index"], json!(0));
    assert_eq!(restarted[0]["generation"], json!(h.generation));
    assert_eq!(restarted[0]["count"], json!(LIMIT));
    let acknowledged = acknowledgements(&h.lab, &id);
    assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
    assert_eq!(acknowledged[0]["count"], json!(LIMIT));
    let receipt = acknowledged[0]["receipt"].as_str().unwrap();
    assert!(
        receipt.contains(restarted[0]["operation_id"].as_str().unwrap()),
        "{receipt}"
    );
    until("the restarted instance to serve", || {
        let (state, open, generation, _) = h.lab.instance(&id, 0);
        state == "ready" && open && generation.is_some_and(|g| g != h.generation)
    })
    .await;
    let generation = h.lab.instance(&id, 0).2.unwrap();
    let lease = h
        .port
        .open_instance_lease(&id, generation, LIMIT)
        .await
        .unwrap()
        .unwrap();
    h.close_lease(lease, LeaseEnd::Completed).await;
    h.lab.worker.shutdown().await.unwrap();
}

// T17 (ADR 0032 §7): one request below the limit, or a live request among
// the held ones, restarts nothing.
#[tokio::test]
async fn held_requests_below_the_limit_or_beside_live_work_restart_nothing() {
    let h = harness_with_ready_instance(false).await;
    let id = h.lab.a.deployment_id.clone();
    for _ in 0..LIMIT - 1 {
        let lease = h.open_lease().await;
        h.close_lease(lease, UNANSWERED).await;
    }
    assert_eq!(
        h.port.restart_for_held_requests(&id).await.unwrap(),
        crate::port::HeldRestart::NotHeld
    );
    let live = h.open_lease().await;
    assert_eq!(
        h.port.restart_for_held_requests(&id).await.unwrap(),
        crate::port::HeldRestart::NotHeld
    );
    assert!(restarts(&h.lab, &id).is_empty());
    assert_eq!(h.lab.state(&id), "ready");
    h.close_lease(live, LeaseEnd::Completed).await;
    h.lab.worker.shutdown().await.unwrap();
}

// T17 (ADR 0032 §1–§3): an SGLang 0.5.22 launch with one tokenizer worker
// carries the request ID and is aborted by it through its own retained
// driver (the adapter's `cancel_work`, named by its binding); a vLLM lane is
// neither offered it nor sent anything.
#[tokio::test]
async fn an_sglang_0_5_22_lane_aborts_a_request_through_its_driver() {
    let lab = lab(15, 50).await;
    lab.ready(&lab.a).await;
    let port = lab.port(Duration::from_secs(5));
    let request = "01K00000000000000000000R01";
    let a = lab.a.deployment_id.clone();
    let a_generation = lab.instance(&a, 0).2.unwrap();
    assert!(
        !port
            .instance_endpoint(&a, a_generation)
            .unwrap()
            .unwrap()
            .request_abort
    );
    assert!(!port.runtime_endpoint(&a).unwrap().unwrap().request_abort);
    assert!(!port.abort_request(&a, a_generation, request).await);
    assert!(lab.engine.aborted.lock().unwrap().is_empty());
    let s = lab.deploy_build("sglang-s", SGLANG, "0.5.22", |_| {});
    tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&s))
        .await
        .unwrap()
        .unwrap();
    let generation = lab.instance(&s, 0).2.unwrap();
    assert!(
        port.instance_endpoint(&s, generation)
            .unwrap()
            .unwrap()
            .request_abort
    );
    assert!(port.abort_request(&s, generation, request).await);
    let aborted = lab.engine.aborted.lock().unwrap().clone();
    assert_eq!(
        aborted,
        vec![(
            MemberRef {
                deployment_id: s.clone(),
                member_id: retained_binding(&lab, &s),
            },
            request.to_owned()
        )]
    );
    // An incarnation that holds no runtime is sent nothing.
    assert!(!port.abort_request(&s, generation + 100, request).await);
    assert_eq!(lab.engine.aborted.lock().unwrap().len(), 1);
    lab.worker.shutdown().await.unwrap();
}

// T17 (ADR 0032 §1, ADR 0017): SGLang 0.5.22 with two tokenizer workers, or
// another SGLang build, is not offered the abort and is sent nothing.
#[tokio::test]
async fn other_sglang_launches_are_not_offered_the_abort() {
    for (name, build, workers) in [("sglang-w2", "0.5.22", 2), ("sglang-old", "0.5.21", 1)] {
        let lab = lab(15, 50).await;
        let s = lab.deploy_build(name, SGLANG, build, |d| {
            d["engine_config"]["sglang"] = json!({ "tokenizer_workers": workers });
        });
        let port = lab.port(Duration::from_secs(5));
        tokio::time::timeout(Duration::from_secs(30), port.activate_for_request(&s))
            .await
            .unwrap()
            .unwrap();
        let generation = lab.instance(&s, 0).2.unwrap();
        assert!(
            !port
                .instance_endpoint(&s, generation)
                .unwrap()
                .unwrap()
                .request_abort,
            "{name}"
        );
        assert!(
            !port
                .abort_request(&s, generation, "01K00000000000000000000R01")
                .await
        );
        assert!(lab.engine.aborted.lock().unwrap().is_empty(), "{name}");
        lab.worker.shutdown().await.unwrap();
    }
}

/// ADR 0037 §3: the router's tracing with a held `capyctl.settle` span for
/// `lease`, as the router holds one for a cut request, shared with the lab's
/// coordinator.
fn held_settle(h: &Harness, lease: &str) -> capyctl_trace::testing::Capture {
    use capyctl_trace::{CallerMode, Name, Parent, Service};
    let (tracing, spans) =
        capyctl_trace::testing::capture(1.0, CallerMode::Parent, Service::Server);
    let _ = h
        .lab
        .port(Duration::from_secs(10))
        .with_tracing(tracing.clone());
    tracing.hold(lease, tracing.span(Name::Settle, Parent::Root, &[]));
    spans
}

// T48 T17 (ADR 0037 §3, ADR 0032): the engine's idle reading settles an
// answered cancellation, and the router's held `capyctl.settle` span for
// its lease ends then, completed; not while the engine reads busy.
#[tokio::test]
async fn an_idle_reading_ends_the_held_settle_span() {
    let h = harness_with_ready_instance(false).await;
    let lease = h.open_lease().await;
    let spans = held_settle(&h, lease.id());
    h.close_lease(lease, ANSWERED).await;
    tokio::time::sleep(TICKS).await;
    assert!(spans.named("capyctl.settle").is_empty(), "busy: still held");
    h.set_quiet(true);
    h.until_outstanding(0).await;
    until("the settle span", || {
        !spans.named("capyctl.settle").is_empty()
    })
    .await;
    let settle = spans.named("capyctl.settle");
    assert_eq!(settle.len(), 1);
    assert_eq!(settle[0].attr("capyctl.outcome"), Some("completed"));
    h.lab.worker.shutdown().await.unwrap();
}

// T48 T17 (ADR 0037 §3, SPEC §10 amended 2026-10-21): a cancellation the
// engine never answered settles only at the instance's verified exit; its
// held settle span ends then, completed.
#[tokio::test]
async fn a_verified_exit_ends_the_held_settle_span() {
    let h = harness_with_ready_instance(true).await;
    let lease = h.open_lease().await;
    let spans = held_settle(&h, lease.id());
    h.close_lease(lease, UNANSWERED).await;
    tokio::time::sleep(TICKS).await;
    assert!(spans.named("capyctl.settle").is_empty(), "still held");
    let id = h.lab.a.deployment_id.clone();
    let stop = h
        .lab
        .worker
        .commands()
        .administrative_stop("operator", &id, h.lab.a.revision, "stop", 100_000)
        .unwrap();
    until_succeeded(&h.lab, stop.operation_id(), &id).await;
    assert_eq!(h.outstanding(), 0);
    until("the settle span", || {
        !spans.named("capyctl.settle").is_empty()
    })
    .await;
    let settle = spans.named("capyctl.settle");
    assert_eq!(settle.len(), 1, "ended with the verified exit");
    assert_eq!(settle[0].attr("capyctl.outcome"), Some("completed"));
    h.lab.worker.shutdown().await.unwrap();
}
