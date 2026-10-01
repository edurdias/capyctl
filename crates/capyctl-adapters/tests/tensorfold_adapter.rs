//! ADR 0023 contract for the TensorFold adapter (T41): readiness from
//! `/health` and the model list, the engine's own work counters, and the
//! Initialize step. Exercised against a local axum engine that behaves like
//! TensorFold 0.6.0 and a scripted process tool; nothing here qualifies a
//! native engine recipe.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

use capyctl_adapters::tensorfold::{PlanInputTensorfold, TensorfoldAdapter};
use capyctl_adapters::traits::{
    AdapterError, EngineAdapter, MemberRef, OwnedProcessLaunch, ParkLevel, RenderedCommand,
    RuntimeAction, RuntimeCommand, RuntimeError,
};
use capyctl_domain::completion::{
    ExecutionIdentities, Milestone, Presence, ProcessIdentity, StepExecutionContext,
    TransitionToken,
};

/// A port nobody is listening on: bound to learn a free one, then released.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// A process tool that records what it was asked to spawn and answers from a
/// script. It never signals or inspects a real process.
struct ScriptedTool {
    identity: ProcessIdentity,
    present: Mutex<Presence>,
    group: Vec<ProcessIdentity>,
    spawned: Mutex<Vec<RenderedCommand>>,
}

impl ScriptedTool {
    fn alive(identity: ProcessIdentity, workers: Vec<ProcessIdentity>) -> Self {
        let group = std::iter::once(identity.clone()).chain(workers).collect();
        Self {
            identity,
            present: Mutex::new(Presence::Alive),
            group,
            spawned: Mutex::new(Vec::new()),
        }
    }
}

impl OwnedProcessLaunch for ScriptedTool {
    fn spawn_durable(
        &self,
        _incarnation: &str,
        cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawned.lock().unwrap().push(cmd.clone());
        Ok(self.identity.clone())
    }

    fn present(&self, _identity: &ProcessIdentity) -> Presence {
        *self.present.lock().unwrap()
    }

    fn observe_group(&self, _api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        Ok(self.group.clone())
    }

    fn terminate_owned(
        &self,
        _identities: &[ProcessIdentity],
        _grace: Duration,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn api_identity() -> ProcessIdentity {
    ProcessIdentity {
        role: "api".into(),
        pid: 4242,
        boot_id: "boot".into(),
        start_ticks: 99,
    }
}

fn url(port: u16) -> reqwest::Url {
    format!("http://127.0.0.1:{port}").parse().unwrap()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[derive(Clone)]
struct Stub {
    model: String,
    ready_after: usize,
    polls: Arc<AtomicUsize>,
    running: Arc<AtomicUsize>,
    /// `busy` reported independently of `requests_running`, to make them disagree.
    busy_override: Arc<Mutex<Option<bool>>>,
    reasoning_only: bool,
}

async fn health(State(stub): State<Stub>) -> axum::response::Response {
    let seen = stub.polls.fetch_add(1, Ordering::SeqCst);
    if seen < stub.ready_after {
        // TensorFold answers /health only after the model is loaded; before
        // that the port is not listening at all, which a 503 stands in for.
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let running = stub.running.load(Ordering::SeqCst);
    let busy = stub.busy_override.lock().unwrap().unwrap_or(running > 0);
    Json(json!({"ok": true, "backend": "tensorfold", "busy": busy, "requests_running": running}))
        .into_response()
}

async fn models(State(stub): State<Stub>) -> Json<Value> {
    Json(
        json!({"object": "list", "data": [{"id": stub.model, "object": "model", "owned_by": "tensorfold"}]}),
    )
}

async fn chat(State(stub): State<Stub>, Json(body): Json<Value>) -> axum::response::Response {
    assert_eq!(
        body["model"], stub.model,
        "the forwarded request keeps the served name"
    );
    let delta = if stub.reasoning_only {
        json!({"reasoning_content": "The user wants"})
    } else {
        json!({"content": "Ready."})
    };
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 1, "model": stub.model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut end = chunk(json!({}), json!("length"));
    end["tensorfold"] = json!({"drafted": 3, "accepted": 2});
    end["usage"] = json!({"prompt_tokens": 3, "completion_tokens": 8, "total_tokens": 11});
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant"}), Value::Null),
        chunk(delta, Value::Null),
        end
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

async fn stub_engine(model: &str, ready_after: usize, reasoning_only: bool) -> (Stub, u16) {
    let stub = Stub {
        model: model.into(),
        ready_after,
        polls: Arc::default(),
        running: Arc::default(),
        busy_override: Arc::default(),
        reasoning_only,
    };
    let app = axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (stub, port)
}

fn plan(port: u16, warm_startup_ms: i64) -> PlanInputTensorfold {
    PlanInputTensorfold {
        engine_bin: "/opt/tf/bin/tensorfold".into(),
        engine_path_extra: Some("/opt/tf/bin".into()),
        model_path: "/srv/models/nemotron".into(),
        served_model_name: "nemotron".into(),
        port,
        context_length: 8192,
        warm_startup_ms,
        ..PlanInputTensorfold::default()
    }
}

fn initialize_command(deadline_in_ms: i64) -> RuntimeCommand {
    RuntimeCommand {
        action: RuntimeAction::Initialize,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d-1".into(),
                revision: 1,
                generation: 1,
                operation_id: "o-1".into(),
                step_id: "s-1".into(),
            },
            binding_id: "b-1".into(),
            incarnation: "i-1".into(),
            issued_at_ms: now_ms(),
            deadline_ms: now_ms() + deadline_in_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: Some("g-1".into()),
            launch_settings: Some(capyctl_testkit::tensorfold_launch_settings()),
        },
    }
}

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d-1".into(),
        member_id: "b-1".into(),
    }
}

// T41 (SPEC §6.1): spawn, wait for /health and the model list, probe, and
// report the single API process; no key reaches the engine.
#[tokio::test]
async fn initialize_waits_for_health_and_reports_one_process() {
    let (stub, port) = stub_engine("nemotron", 3, false).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![]));
    let adapter = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 60_000))
        .with_tools(tool.clone());
    let observation = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert!(
        stub.polls.load(Ordering::SeqCst) >= 4,
        "a listening port is not readiness"
    );
    assert_eq!(observation.identities, vec![api_identity()]);
    assert!(observation.facts.contains(&Milestone::ModelUsable));
    let spawned = tool.spawned.lock().unwrap();
    assert_eq!(spawned[0].argv[1], "serve");
    assert!(!spawned[0].env.keys().any(|k| k.contains("KEY")));
}

// T41 (Review Focus 1): reasoning tokens alone are an answer.
#[tokio::test]
async fn a_reasoning_only_probe_answer_is_an_answer() {
    let (_stub, port) = stub_engine("nemotron", 0, true).await;
    let adapter = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 60_000))
        .with_tools(Arc::new(ScriptedTool::alive(api_identity(), vec![])));
    adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
}

// T41 (Review Focus 5): with a build present the launch gives up at the
// ordinary bound; without one it waits for the full deadline.
#[tokio::test]
async fn a_warm_launch_gives_up_at_the_ordinary_bound() {
    let (_stub, port) = stub_engine("nemotron", usize::MAX, false).await;
    let warm = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 1_500))
        .with_extensions_built(true)
        .with_tools(Arc::new(ScriptedTool::alive(api_identity(), vec![])));
    let started = Instant::now();
    let error = warm
        .execute_persisted(&initialize_command(20_000))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    assert!(
        error.to_string().contains("ordinary startup bound"),
        "{error}"
    );
    let cold = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 1_500))
        .with_extensions_built(false)
        .with_tools(Arc::new(ScriptedTool::alive(api_identity(), vec![])));
    let started = Instant::now();
    let error = cold
        .execute_persisted(&initialize_command(5_000))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "the first build waits for the deadline"
    );
    assert!(error.to_string().contains("deadline"), "{error}");
}

// T41 (spec §5): idle needs requests_running 0 and busy false; disagreement
// is not idle; an unreachable engine is not idle.
#[tokio::test]
async fn idle_before_signal_reads_the_engines_own_counters() {
    let (stub, port) = stub_engine("nemotron", 0, false).await;
    let adapter = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into());
    assert_eq!(adapter.idle_before_signal(&member()).await, Some(true));
    stub.running.store(1, Ordering::SeqCst);
    assert_eq!(adapter.idle_before_signal(&member()).await, Some(false));
    stub.running.store(0, Ordering::SeqCst);
    *stub.busy_override.lock().unwrap() = Some(true);
    assert_eq!(adapter.idle_before_signal(&member()).await, Some(false));
    let gone = TensorfoldAdapter::new(url(free_port().await), "0.6.0".into(), "nemotron".into());
    assert_eq!(gone.idle_before_signal(&member()).await, Some(false));
    assert!(!adapter.prepare_park(&member()).await.unwrap().quiescent);
}

// T41 T21: no park, restore or reload path exists.
#[tokio::test]
async fn there_is_no_park_path() {
    let adapter = TensorfoldAdapter::new(url(1), "0.6.0".into(), "nemotron".into());
    assert!(matches!(
        adapter.park(&member(), ParkLevel::Two).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    assert!(matches!(
        adapter.restore(&member()).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    for action in [
        RuntimeAction::Park,
        RuntimeAction::Restore,
        RuntimeAction::ReloadWeights,
    ] {
        let mut command = initialize_command(1_000);
        command.action = action;
        assert_eq!(
            adapter.execute_persisted(&command).await.unwrap_err(),
            RuntimeError::Unsupported
        );
    }
}
