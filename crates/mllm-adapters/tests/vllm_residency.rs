//! SPEC §9.1 deep park for vLLM as persisted steps (T16, T20, T21).
//!
//! A local axum engine models vLLM's development-mode residency surface: a
//! level-2 sleep that drops weights and KV, separate weight and KV wakes, a
//! `reload_weights` collective, a prefix-cache reset, `/is_sleeping`, and the
//! running and waiting gauges on `/metrics`. Every route is keyed, as mllm's
//! guard keys them. Nothing here qualifies a native vLLM recipe: it proves the
//! step order, the refusals and the evidence, not that vLLM parks on a Spark.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::json;

use mllm_adapters::traits::{
    EngineAdapter, MemberRef, RuntimeAction, RuntimeCommand, RuntimeError,
};
use mllm_adapters::vllm::VllmAdapter;
use mllm_adapters::ParkPolicy;
use mllm_domain::completion::{
    ExecutionIdentities, Milestone, ProcessIdentity, StepExecutionContext, TransitionToken,
};

const KEY: &str = "k3y";

#[derive(Default)]
struct Engine {
    calls: Vec<String>,
    sleeping: bool,
    weights_awake: bool,
    weights_loaded: bool,
    kv_awake: bool,
    running: u32,
    fail: Option<String>,
    metrics: bool,
}

type Shared = Arc<Mutex<Engine>>;

fn keyed(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {KEY}"))
}

fn record(engine: &Shared, call: &str) -> Result<(), StatusCode> {
    let mut e = engine.lock().unwrap();
    e.calls.push(call.to_string());
    if e.fail.as_deref() == Some(call) {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(())
}

async fn sleep(
    State(engine): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    if !keyed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let call = format!("sleep:{}", q.get("level").cloned().unwrap_or_default());
    if let Err(status) = record(&engine, &call) {
        return status.into_response();
    }
    let mut e = engine.lock().unwrap();
    e.sleeping = true;
    e.weights_awake = false;
    e.weights_loaded = false;
    e.kv_awake = false;
    StatusCode::OK.into_response()
}

async fn wake(
    State(engine): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    if !keyed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let tag = q.get("tags").cloned().unwrap_or_default();
    if let Err(status) = record(&engine, &format!("wake:{tag}")) {
        return status.into_response();
    }
    let mut e = engine.lock().unwrap();
    match tag.as_str() {
        "weights" => e.weights_awake = true,
        "kv_cache" => e.kv_awake = true,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    }
    e.sleeping = !(e.weights_awake && e.kv_awake);
    StatusCode::OK.into_response()
}

async fn collective(
    State(engine): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    if !keyed(&headers) || body["method"] != "reload_weights" {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Err(status) = record(&engine, "reload_weights") {
        return status.into_response();
    }
    let mut e = engine.lock().unwrap();
    if !e.weights_awake {
        return StatusCode::CONFLICT.into_response();
    }
    e.weights_loaded = true;
    StatusCode::OK.into_response()
}

async fn reset(State(engine): State<Shared>, headers: HeaderMap) -> axum::response::Response {
    if !keyed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Err(status) = record(&engine, "reset_prefix_cache") {
        return status.into_response();
    }
    Json(json!({"success": true})).into_response()
}

async fn is_sleeping(State(engine): State<Shared>, headers: HeaderMap) -> axum::response::Response {
    if !keyed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({"is_sleeping": engine.lock().unwrap().sleeping})).into_response()
}

async fn metrics(State(engine): State<Shared>, headers: HeaderMap) -> axum::response::Response {
    if !keyed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let e = engine.lock().unwrap();
    if !e.metrics {
        return "# no gauges\n".into_response();
    }
    format!(
        "# HELP vllm:num_requests_running running\n\
         vllm:num_requests_running{{engine=\"0\",model_name=\"m\"}} {}.0\n\
         vllm:num_requests_running_total 7\n\
         vllm:num_requests_waiting{{engine=\"0\",model_name=\"m\"}} 0.0\n",
        e.running
    )
    .into_response()
}

/// W5: the readiness probe after a wake is one completion on the inference
/// path, answered only by an awake engine with its weights reloaded.
async fn chat(
    State(engine): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::sse::{Event, Sse};
    if !keyed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Err(status) = record(&engine, "chat") {
        return status.into_response();
    }
    {
        let e = engine.lock().unwrap();
        if e.sleeping || (e.weights_awake && !e.weights_loaded) {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let model = body["model"].as_str().unwrap_or_default().to_string();
    let chunks = [
        json!({"id": "p", "object": "chat.completion.chunk", "created": 1, "model": model,
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ready"}, "finish_reason": null}]}),
        json!({"id": "p", "object": "chat.completion.chunk", "created": 1, "model": model,
               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ];
    let events: Vec<Result<Event, std::convert::Infallible>> = chunks
        .iter()
        .map(|chunk| Ok(Event::default().data(chunk.to_string())))
        .chain(std::iter::once(Ok(Event::default().data("[DONE]"))))
        .collect();
    Sse::new(futures::stream::iter(events)).into_response()
}

async fn serve(engine: Shared) -> u16 {
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/sleep", post(sleep))
        .route("/wake_up", post(wake))
        .route("/collective_rpc", post(collective))
        .route("/reset_prefix_cache", post(reset))
        .route("/is_sleeping", get(is_sleeping))
        .route("/metrics", get(metrics))
        .with_state(engine);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn identities() -> Vec<ProcessIdentity> {
    vec![
        ProcessIdentity {
            role: "api".into(),
            pid: 4242,
            boot_id: "boot".into(),
            start_ticks: 99,
        },
        ProcessIdentity {
            role: "worker-0".into(),
            pid: 4243,
            boot_id: "boot".into(),
            start_ticks: 100,
        },
    ]
}

fn step(action: RuntimeAction, step_id: &str) -> RuntimeCommand {
    RuntimeCommand {
        action,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d-1".into(),
                revision: 1,
                generation: 1,
                operation_id: "o-1".into(),
                step_id: step_id.into(),
            },
            binding_id: "b-1".into(),
            incarnation: "i-1".into(),
            issued_at_ms: now_ms(),
            deadline_ms: now_ms() + 30_000,
            identities: ExecutionIdentities::Retained(identities()),
            completion_target: None,
            grant_id: Some("g-1".into()),
            launch_settings: None,
        },
    }
}

fn adapter(port: u16, policy: ParkPolicy) -> VllmAdapter {
    VllmAdapter::new(
        format!("http://127.0.0.1:{port}").parse().unwrap(),
        None,
        "fp".into(),
        policy,
        "m".into(),
    )
    .with_engine_key(KEY.into())
}

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d-1".into(),
        member_id: "b-1".into(),
    }
}

/// SPEC §9.1: a level-2 sleep, then weights wake, `reload_weights`, KV wake
/// and a prefix-cache reset, in that order, each a separate step with its own
/// milestone. The engine key is presented on every control call.
// T16 T21
#[tokio::test]
async fn a_deep_park_and_its_restoration_run_in_the_documented_order() {
    let engine = Shared::default();
    engine.lock().unwrap().metrics = true;
    let port = serve(engine.clone()).await;
    let vllm = adapter(port, ParkPolicy::Enabled);
    assert!(vllm.prepare_park(&member()).await.unwrap().quiescent);

    let parked = vllm
        .execute_persisted(&step(RuntimeAction::Park, "s-park"))
        .await
        .unwrap();
    assert_eq!(parked.facts, vec![Milestone::MemoryReleased]);
    assert_eq!(parked.identities, identities());
    assert_eq!(
        (parked.binding_id.as_str(), parked.incarnation.as_str()),
        ("b-1", "i-1")
    );
    assert!(engine.lock().unwrap().sleeping);

    let mut facts = Vec::new();
    for (action, id) in [
        (RuntimeAction::Restore, "s-wake"),
        (RuntimeAction::ReloadWeights, "s-reload"),
        (RuntimeAction::InvalidateCache, "s-cache"),
    ] {
        facts.extend(
            vllm.execute_persisted(&step(action, id))
                .await
                .unwrap()
                .facts,
        );
    }
    assert_eq!(
        facts,
        vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid
        ]
    );
    let e = engine.lock().unwrap();
    assert_eq!(
        e.calls,
        [
            "sleep:2",
            "wake:weights",
            "reload_weights",
            "wake:kv_cache",
            "reset_prefix_cache"
        ]
    );
    assert!(!e.sleeping && e.weights_loaded);
}

/// SPEC §9.1 / T21: an opted-out host never reaches a sleep, wake or collective
/// route, whatever step it is asked for.
// T21
#[tokio::test]
async fn an_opted_out_host_makes_no_residency_call() {
    let engine = Shared::default();
    let port = serve(engine.clone()).await;
    let vllm = adapter(port, ParkPolicy::Disabled);
    for (action, id) in [
        (RuntimeAction::Park, "a"),
        (RuntimeAction::Restore, "b"),
        (RuntimeAction::ReloadWeights, "c"),
        (RuntimeAction::InvalidateCache, "d"),
    ] {
        assert_eq!(
            vllm.execute_persisted(&step(action, id)).await.unwrap_err(),
            RuntimeError::Unsupported
        );
    }
    assert!(engine.lock().unwrap().calls.is_empty());
}

/// SPEC §13.2 / T20: a collective whose outcome is unknown is never repeated.
/// The failed step is uncertain, and the adapter refuses every later residency
/// step without an engine call, including a retry under a new step id.
// T20
#[tokio::test]
async fn a_failed_reload_is_uncertain_and_never_repeated() {
    let engine = Shared::default();
    let port = serve(engine.clone()).await;
    let vllm = adapter(port, ParkPolicy::Enabled);
    vllm.execute_persisted(&step(RuntimeAction::Park, "p"))
        .await
        .unwrap();
    vllm.execute_persisted(&step(RuntimeAction::Restore, "w"))
        .await
        .unwrap();
    engine.lock().unwrap().fail = Some("reload_weights".into());
    assert!(matches!(
        vllm.execute_persisted(&step(RuntimeAction::ReloadWeights, "r"))
            .await,
        Err(RuntimeError::Uncertain(_))
    ));
    engine.lock().unwrap().fail = None;
    for (action, id) in [
        (RuntimeAction::ReloadWeights, "r2"),
        (RuntimeAction::InvalidateCache, "c"),
    ] {
        assert_eq!(
            vllm.execute_persisted(&step(action, id)).await.unwrap_err(),
            RuntimeError::Unsupported
        );
    }
    assert_eq!(
        engine.lock().unwrap().calls,
        ["sleep:2", "wake:weights", "reload_weights"]
    );
}

/// A step id runs once per adapter, and a step without retained identities,
/// past its deadline or carrying launch settings is refused before any call.
// T34
#[tokio::test]
async fn malformed_or_repeated_steps_are_refused_before_any_engine_call() {
    let engine = Shared::default();
    let port = serve(engine.clone()).await;
    let vllm = adapter(port, ParkPolicy::Enabled);
    let mut owned = step(RuntimeAction::Park, "x1");
    owned.context.identities = ExecutionIdentities::OwnedLaunch;
    let mut expired = step(RuntimeAction::Park, "x2");
    expired.context.deadline_ms = now_ms() - 1;
    let mut nameless = step(RuntimeAction::Park, "x3");
    nameless.context.binding_id.clear();
    for command in [owned, expired, nameless] {
        assert_eq!(
            vllm.execute_persisted(&command).await.unwrap_err(),
            RuntimeError::Unsupported
        );
    }
    assert!(engine.lock().unwrap().calls.is_empty());
    vllm.execute_persisted(&step(RuntimeAction::Park, "once"))
        .await
        .unwrap();
    assert_eq!(
        vllm.execute_persisted(&step(RuntimeAction::Park, "once"))
            .await
            .unwrap_err(),
        RuntimeError::Unsupported
    );
    assert_eq!(engine.lock().unwrap().calls, ["sleep:2"]);
}

/// SPEC §10 step 4: quiescence is the engine's running and waiting gauges both
/// at zero. Running work, or gauges that are missing, are not quiescence.
// T16
#[tokio::test]
async fn quiescence_needs_both_gauges_at_zero() {
    let engine = Shared::default();
    let port = serve(engine.clone()).await;
    let vllm = adapter(port, ParkPolicy::Enabled);
    assert!(!vllm.prepare_park(&member()).await.unwrap().quiescent);
    engine.lock().unwrap().metrics = true;
    engine.lock().unwrap().running = 2;
    assert!(!vllm.prepare_park(&member()).await.unwrap().quiescent);
    engine.lock().unwrap().running = 0;
    assert!(vllm.prepare_park(&member()).await.unwrap().quiescent);
    // An engine that cannot be reached is not quiescent either.
    let gone = adapter(1, ParkPolicy::Enabled);
    assert!(!gone.prepare_park(&member()).await.unwrap().quiescent);
}

/// SPEC §6.1 (W5): an embedded wake reopens dispatch only after a fresh
/// completion through the inference path proves a usable model; a probe of an
/// engine still asleep is refused and never counted as readiness.
// T16 T20
#[tokio::test]
async fn a_readiness_probe_proves_the_woken_model_usable() {
    let engine = Shared::default();
    let port = serve(engine.clone()).await;
    let vllm = adapter(port, ParkPolicy::Enabled);
    vllm.execute_persisted(&step(RuntimeAction::Park, "p"))
        .await
        .unwrap();
    // Asleep: the probe refuses without reaching the inference path.
    assert!(matches!(
        vllm.execute_persisted(&step(RuntimeAction::Probe, "early"))
            .await,
        Err(RuntimeError::Uncertain(_))
    ));
    assert!(!engine.lock().unwrap().calls.contains(&"chat".to_string()));

    let engine = Shared::default();
    let port = serve(engine.clone()).await;
    let vllm = adapter(port, ParkPolicy::Enabled);
    let mut facts = Vec::new();
    for (action, id) in [
        (RuntimeAction::Park, "p"),
        (RuntimeAction::Restore, "w"),
        (RuntimeAction::ReloadWeights, "r"),
        (RuntimeAction::InvalidateCache, "c"),
        (RuntimeAction::Probe, "probe"),
    ] {
        facts.extend(vllm.execute_persisted(&step(action, id)).await.unwrap().facts);
    }
    assert_eq!(
        facts,
        vec![
            Milestone::MemoryReleased,
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable
        ]
    );
    assert_eq!(engine.lock().unwrap().calls.last().map(String::as_str), Some("chat"));
}
