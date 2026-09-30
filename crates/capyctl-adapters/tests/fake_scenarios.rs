//! Scenario tests for the fake engine simulator.
//!
//! Each test encodes one behavioral contract from the design that the real
//! F1/F2 adapters must also honor; the fake engine is the executable spec.

use capyctl_adapters::ParkPolicy;
use capyctl_adapters::{
    AdapterError, CancellationOutcome, EngineAdapter, Launcher, MemberRef, Phase, Readiness,
    RenderedCommand, RequestRef,
};
use capyctl_testkit::{FakeEngine, FakeLauncher};
use std::time::Duration;

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d-1".into(),
        member_id: "m-1".into(),
    }
}

fn req(id: &str) -> RequestRef {
    RequestRef { id: id.into() }
}

fn cmd() -> RenderedCommand {
    RenderedCommand {
        argv: vec!["fake-engine".into()],
        env: Default::default(),
    }
}

/// The fake engine's retained-buffer byte count at park level 2
/// (weights and KV cache discarded, only buffers remain).
const BUFFER_RESIDUE: i64 = capyctl_testkit::BUFFER_RESIDUE;

#[tokio::test]
async fn slow_startup_liveness_is_not_readiness() {
    let e = FakeEngine::new().with_startup_delay(Duration::from_millis(50));
    let st = e.check_readiness(&member()).await.unwrap();
    assert!(matches!(st, Readiness::Initializing)); // early
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(matches!(
        e.check_readiness(&member()).await.unwrap(),
        Readiness::Ready
    ));
}

#[tokio::test]
async fn sleep_level_two_discards_weights_and_kv() {
    // The §9.1 deep-park security gate denies level 2 by default, so the
    // scenario opts in via host policy before exercising the retention
    // semantics. (Brief's verbatim test predates the gate; see task report.)
    let e = FakeEngine::new().with_policy(ParkPolicy::Enabled);
    e.park(&member(), capyctl_adapters::ParkLevel::Two)
        .await
        .unwrap();
    let obs = e.inspect(&member()).await.unwrap();
    assert_eq!(obs.retained_bytes, BUFFER_RESIDUE); // weights+KV gone, buffers kept
    assert_eq!(e.reload_weights_count(), 0); // not yet reloaded
    e.reload_weights(&member()).await.unwrap();
    assert_eq!(e.reload_weights_count(), 1); // exactly once
}

#[tokio::test]
async fn ambiguous_park_reports_uncertainty_not_success() {
    // The park effect itself requires the deep-park opt-in (§9.1 gate);
    // ambiguity is injected on top of an otherwise-permitted park.
    let e = FakeEngine::new()
        .ambiguous_park()
        .with_policy(ParkPolicy::Enabled);
    let out = e.park(&member(), capyctl_adapters::ParkLevel::Two).await;
    assert!(matches!(out, Err(AdapterError::Uncertain(_))));
}

#[tokio::test]
async fn cancellation_without_ack_reports_uncertainty() {
    let e = FakeEngine::new();
    let out = e.cancel_work(&member(), &req("r1"), false).await.unwrap();
    assert!(matches!(out, CancellationOutcome::Uncertain));
}

#[tokio::test]
async fn deep_park_denied_without_policy_opt_in() {
    // Security gate (design §9): level-2 park is experimental, denied by default.
    let e = FakeEngine::new().with_policy(ParkPolicy::Disabled);
    let out = e.park(&member(), capyctl_adapters::ParkLevel::Two).await;
    assert!(matches!(out, Err(AdapterError::PolicyDenied)));
    // Opt-in via explicit host policy enables the path:
    let e2 = FakeEngine::new().with_policy(ParkPolicy::Enabled);
    assert!(e2
        .park(&member(), capyctl_adapters::ParkLevel::Two)
        .await
        .is_ok());
}

#[tokio::test]
async fn pid_reuse_rejects_stale_handle() {
    let l = FakeLauncher::new().with_pid_reuse();
    let h = l.spawn(&cmd()).unwrap();
    l.terminate(&h, Duration::from_secs(1)).unwrap();
    let h2 = l.spawn(&cmd()).unwrap(); // same pid, new identity
    assert!(matches!(
        l.verify_handle(&h),
        capyctl_adapters::HandleStatus::StaleReused
    ));
    assert!(matches!(
        l.verify_handle(&h2),
        capyctl_adapters::HandleStatus::Valid
    ));
}

#[tokio::test]
async fn crash_at_phase_is_reported() {
    let e = FakeEngine::new().fail_at(Phase::Restore);
    assert!(matches!(
        e.restore(&member()).await,
        Err(AdapterError::Crash(p)) if matches!(p, Phase::Restore)
    ));
}

#[tokio::test]
async fn level_one_park_keeps_cpu_weight_backup() {
    let e = FakeEngine::new();
    let out = e
        .park(&member(), capyctl_adapters::ParkLevel::One)
        .await
        .unwrap();
    let obs = e.inspect(&member()).await.unwrap();
    assert!(matches!(out, capyctl_adapters::ParkOutcome::Parked { .. }));
    assert!(obs.retained_bytes > BUFFER_RESIDUE); // CPU weight backup retained
}

#[tokio::test]
async fn reload_weights_also_denied_without_policy_opt_in() {
    let e = FakeEngine::new().with_policy(ParkPolicy::Disabled);
    assert!(matches!(
        e.reload_weights(&member()).await,
        Err(AdapterError::PolicyDenied)
    ));
    let e2 = FakeEngine::new().with_policy(ParkPolicy::Enabled);
    assert!(e2.reload_weights(&member()).await.is_ok());
}
