//! Contract tests for the vLLM adapter behind the F0 EngineAdapter trait.
//! Runs against a mock engine HTTP surface; the adapter must honor the
//! deep-park policy gate, uncertainty semantics, and readiness rules.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::Json;
use mllm_adapters::fake::{FakeLauncher, ParkPolicy};
use mllm_adapters::traits::EngineAdapter;
use mllm_adapters::vllm::VllmAdapter;
use mllm_adapters::{MemberRef, ParkLevel, Readiness};
use harness::{run_conformance, ParkGateMode};

#[derive(Clone, Default)]
struct MockState {
    sleep_hits: Arc<AtomicUsize>,
    wake_hits: Arc<AtomicUsize>,
    rpc_hits: Arc<AtomicUsize>,
}

async fn models() -> Json<serde_json::Value> {
    Json(serde_json::json!({"data": [{"id": "toy-model"}]}))
}

async fn do_sleep(State(st): State<MockState>) -> Json<serde_json::Value> {
    st.sleep_hits.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({"sleep": true}))
}

async fn do_wake(State(st): State<MockState>) -> Json<serde_json::Value> {
    st.wake_hits.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({"awake": true}))
}

async fn rpc(State(st): State<MockState>) -> Json<serde_json::Value> {
    st.rpc_hits.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({"ok": true}))
}

async fn spawn_mock() -> (SocketAddr, MockState) {
    let st = MockState::default();
    let app = axum::Router::new()
        .route("/v1/models", get(models))
        .route("/sleep", post(do_sleep))
        .route("/wake_up", post(do_wake))
        .route("/collective_rpc", post(rpc))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, st)
}

fn member() -> MemberRef {
    MemberRef { deployment_id: "d".into(), member_id: "m".into() }
}

#[tokio::test]
async fn readiness_requires_served_model_not_liveness() {
    let (addr, _st) = spawn_mock().await;
    let a = VllmAdapter::new(
        format!("http://{addr}").parse().unwrap(),
        None,
        "vllm-test-1".into(),
        ParkPolicy::Denied,
        "toy-model".into(),
    );
    // Mock serves "toy-model" → Ready.
    assert!(matches!(a.check_readiness(&member()).await.unwrap(), Readiness::Ready));
}

#[tokio::test]
async fn park_denied_by_default_without_engine_call() {
    let (addr, st) = spawn_mock().await;
    let a = VllmAdapter::new(
        format!("http://{addr}").parse().unwrap(),
        None,
        "vllm-test-1".into(),
        ParkPolicy::Denied,
        "toy-model".into(),
    );
    let out = a.park(&member(), ParkLevel::Two).await;
    assert!(matches!(out, Err(mllm_adapters::AdapterError::PolicyDenied)));
    assert_eq!(st.sleep_hits.load(Ordering::SeqCst), 0, "no engine call under denial");
    // Level 1 (restart-level) is also policy-gated on the vllm-sleep profile:
    let out1 = a.park(&member(), ParkLevel::One).await;
    assert!(matches!(out1, Err(mllm_adapters::AdapterError::PolicyDenied)));
}

#[tokio::test]
async fn allowed_policy_parks_and_restores_with_collective_once() {
    let (addr, st) = spawn_mock().await;
    let a = VllmAdapter::new(
        format!("http://{addr}").parse().unwrap(),
        None,
        "vllm-test-1".into(),
        ParkPolicy::ExperimentalAllowed,
        "toy-model".into(),
    );
    let out = a.park(&member(), ParkLevel::Two).await.unwrap();
    assert!(matches!(out, mllm_adapters::ParkOutcome::Parked { .. }));
    assert_eq!(st.sleep_hits.load(Ordering::SeqCst), 1);

    // Parked-state observability: inspect reports Parked, readiness not Ready.
    let st_obs = a.inspect(&member()).await.unwrap();
    assert!(matches!(st_obs.phase, mllm_adapters::Phase::Parked));
    assert_eq!(st_obs.build_fingerprint.as_deref(), Some("vllm-test-1"));

    a.restore(&member()).await.unwrap();
    assert_eq!(st.wake_hits.load(Ordering::SeqCst), 1, "wake once");
    assert_eq!(st.rpc_hits.load(Ordering::SeqCst), 1, "collective once via lead");
    let after = a.inspect(&member()).await.unwrap();
    assert!(matches!(after.phase, mllm_adapters::Phase::Ready));
}

#[tokio::test]
async fn cancel_without_ack_is_uncertain_no_call() {
    let (addr, _st) = spawn_mock().await;
    let a = VllmAdapter::new(
        format!("http://{addr}").parse().unwrap(),
        None,
        "vllm-test-1".into(),
        ParkPolicy::Denied,
        "toy-model".into(),
    );
    let out = a
        .cancel_work(&member(), &mllm_adapters::RequestRef { id: "r1".into() }, false)
        .await
        .unwrap();
    assert!(matches!(out, mllm_adapters::CancellationOutcome::Uncertain));
}

#[tokio::test]
async fn passes_conformance_suite() {
    let (addr, _st) = spawn_mock().await;
    let a = VllmAdapter::new(
        format!("http://{addr}").parse().unwrap(),
        None,
        "vllm-test-1".into(),
        ParkPolicy::Denied,
        "toy-model".into(),
    );
    let launcher = FakeLauncher::new();
    let results = run_conformance(&a, &launcher, ParkGateMode::ProfileGated).await;
    let failures: Vec<_> = results.iter().filter(|r| !r.passed()).collect();
    assert!(failures.is_empty(), "conformance failures: {failures:?}");
}