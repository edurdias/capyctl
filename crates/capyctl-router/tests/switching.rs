//! Switching engine (F1 design §5): single wake join (T15), A→B→A
//! alternation with release evidence (T16), bounded non-resetting fairness
//! window (T19), and the switch-failure branch (A reopens, B fail-fast).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use capyctl_adapters::traits::{
    AdapterError, CancellationOutcome, EngineAdapter, MemberRef, ParkLevel, ParkOutcome, Phase,
    PlanInput, Readiness, ReloadOutcome, RenderedCommand, RequestRef, RestoreOutcome,
    WorkObservation,
};
use capyctl_controller::{Controller, DeployRequest};
use capyctl_store::Store;

fn controller() -> (Arc<Controller>, Arc<Mutex<Store>>) {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let c = Arc::new(Controller::new(
        store.clone(),
        Arc::new(capyctl_testkit::FakeEngine::new()),
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    (c, store)
}

/// An adapter whose engine never reaches quiescence (drain wedges).
struct StuckAdapter;

#[async_trait]
impl EngineAdapter for StuckAdapter {
    async fn inspect(
        &self,
        _: &MemberRef,
    ) -> Result<capyctl_adapters::traits::EngineState, AdapterError> {
        Ok(capyctl_adapters::traits::EngineState {
            phase: Phase::Ready,
            retained_bytes: 0,
            build_fingerprint: Some("stuck".into()),
        })
    }
    async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Ok(RenderedCommand {
            argv: vec!["sleep".into(), "30".into()],
            env: Default::default(),
        })
    }
    async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
        Ok(Readiness::Ready)
    }
    async fn prepare_park(
        &self,
        _: &MemberRef,
    ) -> Result<capyctl_adapters::traits::Quiescence, AdapterError> {
        Ok(capyctl_adapters::traits::Quiescence { quiescent: false })
    }
    async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Ok(WorkObservation::Streaming {
            request_ref: "r1".into(),
        })
    }
    async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Ok(ParkOutcome::Parked { retained_bytes: 0 })
    }
    async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Ok(RestoreOutcome::Restored)
    }
    async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Ok(ReloadOutcome::Reloaded)
    }
    async fn cancel_work(
        &self,
        _: &MemberRef,
        _: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Ok(CancellationOutcome::Uncertain)
    }
}

fn req(name: &str) -> DeployRequest {
    DeployRequest {
        name: name.into(),
        kind: "model".into(),
        manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(name.into()),
    }
}

async fn deploy_and_start(c: &Controller, name: &str) -> String {
    let id = c.submit_deploy(req(name)).await.unwrap();
    let op = c
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    c.wait_terminal(&op).await.unwrap();
    id
}

fn start_op_count(store: &Arc<Mutex<Store>>, dep: &str) -> usize {
    store
        .lock()
        .unwrap()
        .operations_of_kind(dep, "start")
        .unwrap()
        .len()
}

#[tokio::test]
async fn simultaneous_activations_join_one_wake() {
    let (c, store) = controller();
    let id = c.submit_deploy(req("wake-m")).await.unwrap(); // STOPPED

    let sw = Arc::new(capyctl_router::switch::SwitchEngine::new(
        c.clone(),
        Duration::from_secs(5),
    ));
    let sw2 = sw.clone();
    let id2 = id.clone();
    let (ga, gb) = tokio::join!(sw.switch_to(&id), sw2.switch_to(&id2));

    // Both activations succeeded and joined the SAME wake operation.
    let gen = ga.unwrap();
    assert_eq!(gen, hb_gen(&gb));
    assert_eq!(
        start_op_count(&store, &id),
        1,
        "exactly one Start operation (T15)"
    );
}

fn hb_gen(r: &Result<u64, capyctl_router::switch::SwitchError>) -> u64 {
    r.clone().unwrap()
}

#[tokio::test]
async fn follower_joins_instant_leader_without_hang() {
    // T15 join-slot regression: the target is READY already, so the leader
    // completes INSTANTLY — between the follower's claim check and its
    // await. The follower must still return (the published outcome is a
    // retained watch value, never a one-shot notify that can be lost).
    let (c, store) = controller();
    let id = deploy_and_start(&c, "instant-m").await;
    let sw = Arc::new(capyctl_router::switch::SwitchEngine::new(
        c.clone(),
        Duration::from_secs(5),
    ));
    let sw2 = sw.clone();
    let id2 = id.clone();
    let (ga, gb) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), sw.switch_to(&id)),
        tokio::time::timeout(Duration::from_secs(5), sw2.switch_to(&id2)),
    );
    let gen = ga.unwrap().unwrap();
    assert_eq!(gen, hb_gen(&gb.unwrap()));
    // A READY target needs no wake: exactly the original Start remains.
    assert_eq!(start_op_count(&store, &id), 1);
}

#[tokio::test]
async fn a_to_b_to_a_alternates_with_release_evidence() {
    let (c, store) = controller();
    let a = deploy_and_start(&c, "model-a").await;
    let sw = capyctl_router::switch::SwitchEngine::new(c.clone(), Duration::from_secs(5));

    let gen1 = sw.switch_to(&a).await.unwrap(); // already ready → no-op switch
    let b = c.submit_deploy(req("model-b")).await.unwrap();
    let gen2 = sw.switch_to(&b).await.unwrap();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_deployment(&a)
            .unwrap()
            .unwrap()
            .observed_state,
        capyctl_domain::LifecycleState::Stopped,
        "stock model release must terminate the process holding the shared port"
    );
    // Generations are PER-DEPLOYMENT (monotonic within a deployment): B's
    // generation advanced through its own wake (1 → 2).
    assert!(gen2 >= 1, "B activated with its own generation {gen2}");
    let _ = gen1;

    // Release evidence for A: quiescent/parked/terminated journaled.
    let evidence = store
        .lock()
        .unwrap()
        .journal_evidence_of(&a)
        .unwrap()
        .join("\n");
    assert!(
        evidence.contains("quiescent")
            || evidence.contains("terminated")
            || evidence.contains("parked"),
        "A's release evidence journaled: {evidence}"
    );

    // Back to A: correct generation accounting (T16).
    let gen3 = sw.switch_to(&a).await.unwrap();
    assert!(
        gen3 > gen1,
        "A's own generation advanced through park→wake (T16)"
    );
    let state_a = store
        .lock()
        .unwrap()
        .get_deployment(&a)
        .unwrap()
        .unwrap()
        .observed_state;
    assert_eq!(state_a, capyctl_domain::LifecycleState::Ready);
}

#[tokio::test]
async fn qualified_sleep_profile_keeps_park_restore_switch_path() {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let policy = capyctl_adapters::ParkPolicy::Enabled;
    let c = Arc::new(Controller::new_with_policy(
        store.clone(),
        Arc::new(capyctl_testkit::FakeEngine::new().with_policy(policy)),
        Arc::new(capyctl_testkit::FakeLauncher::new()),
        policy,
    ));
    let a = c
        .submit_deploy(DeployRequest {
            kind: "vllm-sleep".into(),
            ..req("sleep-a")
        })
        .await
        .unwrap();
    let b = c.submit_deploy(req("stock-b")).await.unwrap();
    let sw = capyctl_router::switch::SwitchEngine::new(c.clone(), Duration::from_secs(5));
    let gen1 = sw.switch_to(&a).await.unwrap();
    sw.switch_to(&b).await.unwrap();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_deployment(&a)
            .unwrap()
            .unwrap()
            .observed_state,
        capyctl_domain::LifecycleState::Parked
    );
    let gen2 = sw.switch_to(&a).await.unwrap();
    assert!(gen2 > gen1);
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_deployment(&a)
            .unwrap()
            .unwrap()
            .observed_state,
        capyctl_domain::LifecycleState::Ready
    );
}

#[tokio::test]
async fn switch_failure_reopens_a_and_fails_b_fast() {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let c = Arc::new(Controller::new(
        store.clone(),
        Arc::new(StuckAdapter),
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    let a = deploy_and_start(&c, "stuck-a").await;
    let sw = capyctl_router::switch::SwitchEngine::new(c.clone(), Duration::from_millis(200));

    // A holds live work that never drains: the switch to B must FAIL.
    let b = c.submit_deploy(req("stuck-b")).await.unwrap();
    let out = sw.switch_to(&b).await;
    assert!(out.is_err(), "drain that never quiesces fails the switch");

    // A is reopened: not suspended, on-demand eligible, window preserved.
    assert!(!store.lock().unwrap().is_suspended(&a).unwrap());
    // The failed-switch event is journaled (SPEC §17 failed-switches metric).
    let evidence = store
        .lock()
        .unwrap()
        .journal_evidence_of(&a)
        .unwrap()
        .join("\n");
    assert!(
        evidence.contains("switch_failed"),
        "failed-switch event: {evidence}"
    );
}

#[test]
fn fairness_window_is_bounded_and_non_resetting() {
    // T19: busy A cannot push the window out forever — the window closes a
    // fixed interval after it opens, regardless of new A traffic.
    let mut w = capyctl_router::switch::AdmissionWindow::open(Duration::from_millis(100));
    for _ in 0..50 {
        w.try_extend(); // no-op by contract: never resets
    }
    std::thread::sleep(Duration::from_millis(150));
    assert!(w.expired(), "window closes even under sustained A load");
}
