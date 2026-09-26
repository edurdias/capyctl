//! Spec §4 contract for the vLLM builder's Initialize step (T10).
//!
//! The builder is exercised against a local axum engine that behaves like vLLM
//! (a model list that only names the served model once startup is far enough
//! along, and a chat surface that answers only for the right key) and a scripted
//! process tool that records what it was asked to spawn without touching a real
//! process. Nothing here qualifies a native engine recipe: it proves the step's
//! sequence, its refusals and its evidence, not that vLLM starts.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

use mllm_adapters::traits::{
    EngineAdapter, OwnedProcessLaunch, RenderedCommand, RuntimeAction, RuntimeCommand, RuntimeError,
};
use mllm_adapters::vllm::{GrantedBudget, PlanInputVllm, VllmAdapter};
use mllm_adapters::ParkPolicy;
use mllm_domain::completion::{
    ExecutionIdentities, Milestone, Presence, ProcessIdentity, StepExecutionContext,
    TransitionToken,
};
use mllm_domain::launch::LaunchSettings;

// ---------------------------------------------------------------- stub engine

#[derive(Clone)]
struct Stub {
    model: String,
    key: String,
    /// Model-list polls that answer with an empty list before the served name
    /// appears: a listening HTTP server is not model readiness (SPEC §6.1).
    ready_after: usize,
    polls: Arc<AtomicUsize>,
    chat: ChatBehaviour,
}

/// What the chat surface does once the model is listed. The three failures are
/// separate exits of step 5 and each has its own reason.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChatBehaviour {
    /// Streams a short completion, as a healthy engine does.
    Answers,
    /// Accepts the request and never answers: the model lists itself and then
    /// stalls on its first completion.
    Stalls,
    /// Refuses the request outright.
    Refuses,
    /// Answers with an empty assistant message.
    Empty,
}

/// vLLM guards every `/v1` route with the same key, `/v1/models` included, so an
/// unkeyed readiness poll is refused there before it ever reaches the chat probe.
async fn models(State(stub): State<Stub>, headers: HeaderMap) -> axum::response::Response {
    let presented = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if presented != format!("Bearer {}", stub.key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let seen = stub.polls.fetch_add(1, Ordering::SeqCst);
    let data = if seen >= stub.ready_after {
        json!([{ "id": stub.model, "object": "model" }])
    } else {
        json!([])
    };
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn chat(
    State(stub): State<Stub>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let presented = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if presented != format!("Bearer {}", stub.key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match stub.chat {
        ChatBehaviour::Answers | ChatBehaviour::Empty => {}
        ChatBehaviour::Stalls => {
            // Never answers. The builder's own bound is what has to end this.
            std::future::pending::<()>().await;
        }
        ChatBehaviour::Refuses => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let content = if stub.chat == ChatBehaviour::Empty {
        ""
    } else {
        "ready"
    };
    let model = body["model"].as_str().unwrap_or_default().to_string();
    let chunks = vec![
        json!({
            "id": "probe-1", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": content},
                         "finish_reason": serde_json::Value::Null}],
        }),
        json!({
            "id": "probe-1", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        }),
    ];
    let events = chunks
        .into_iter()
        .map(|chunk| {
            Ok::<Event, std::convert::Infallible>(Event::default().data(chunk.to_string()))
        })
        .chain(std::iter::once(Ok(Event::default().data("[DONE]"))));
    Sse::new(futures::stream::iter(events.collect::<Vec<_>>())).into_response()
}

async fn stub_engine(model: &str, ready_after: usize, key: &str) -> (Stub, u16) {
    serve_stub(model, ready_after, key, ChatBehaviour::Answers, 0).await
}

/// `port` of 0 lets the kernel choose; any other value is the port the engine
/// must come up on, which is what a launch through a leased endpoint looks like.
async fn serve_stub(
    model: &str,
    ready_after: usize,
    key: &str,
    chat_behaviour: ChatBehaviour,
    port: u16,
) -> (Stub, u16) {
    let stub = Stub {
        model: model.into(),
        key: key.into(),
        ready_after,
        polls: Arc::new(AtomicUsize::new(0)),
        chat: chat_behaviour,
    };
    let app = axum::Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (stub, addr.port())
}

/// A port nobody is listening on: bound to learn a free one, then released.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// ---------------------------------------------------------------- process tool

/// A process tool that records what it was asked to spawn and answers from a
/// script. It never signals or inspects a real process.
struct ScriptedTool {
    identity: ProcessIdentity,
    present: Mutex<Presence>,
    group: Vec<ProcessIdentity>,
    spawned: Mutex<Vec<RenderedCommand>>,
    /// The engine dies the moment it is spawned (the crash-before-readiness case).
    gone_on_spawn: bool,
    /// Presence checks the builder has made, one per readiness poll.
    present_calls: Arc<AtomicUsize>,
}

impl ScriptedTool {
    fn alive(identity: ProcessIdentity, workers: Vec<ProcessIdentity>) -> Self {
        let group = std::iter::once(identity.clone()).chain(workers).collect();
        Self {
            identity,
            present: Mutex::new(Presence::Alive),
            group,
            spawned: Mutex::new(Vec::new()),
            gone_on_spawn: false,
            present_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn dies_on_spawn(identity: ProcessIdentity) -> Self {
        Self {
            group: vec![identity.clone()],
            gone_on_spawn: true,
            ..Self::alive(identity, Vec::new())
        }
    }

    /// A process whose presence cannot be established: retention, never absence.
    fn unknown_presence(identity: ProcessIdentity) -> Self {
        Self {
            present: Mutex::new(Presence::Unknown),
            ..Self::alive(identity, Vec::new())
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
        if self.gone_on_spawn {
            *self.present.lock().unwrap() = Presence::Gone;
        }
        Ok(self.identity.clone())
    }

    fn present(&self, _identity: &ProcessIdentity) -> Presence {
        self.present_calls.fetch_add(1, Ordering::SeqCst);
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

// -------------------------------------------------------------------- fixtures

fn api_identity() -> ProcessIdentity {
    ProcessIdentity {
        role: "api".into(),
        pid: 4242,
        boot_id: "boot".into(),
        start_ticks: 99,
    }
}

fn worker0() -> ProcessIdentity {
    ProcessIdentity {
        role: "worker-0".into(),
        pid: 4243,
        boot_id: "boot".into(),
        start_ticks: 100,
    }
}

fn url(port: u16) -> reqwest::Url {
    format!("http://127.0.0.1:{port}").parse().unwrap()
}

fn plan(port: u16) -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/venv/bin/vllm".into(),
        model_path: "/srv/models/gate-m".into(),
        port,
        served_model_name: "gate-m".into(),
        tensor_parallel_size: 1,
        pipeline_parallel_size: 1,
        cpu_offload_bytes: 0,
        granted: GrantedBudget::default(),
        engine_args: vec![],
        sleep_flags: vec![],
        // Spec §3: the key rides the environment; it is never a plan field.
        api_key: None,
        engine_path_extra: Some("/opt/venv/bin".into()),
        engine_log: None,
        runtime_dir: Some("/opt/mllm/runtime".into()),
        ..PlanInputVllm::default()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
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
            launch_settings: Some(mllm_testkit::vllm_launch_settings()),
        },
    }
}

fn adapter(port: u16, key: Option<&str>) -> VllmAdapter {
    VllmAdapter::new(
        url(port),
        key.map(str::to_string),
        "fp".into(),
        ParkPolicy::Enabled,
        "gate-m".into(),
    )
}

// ----------------------------------------------------------------------- tests

/// Spec §4: render, spawn, readiness, probe, enumerate, observe.
// T10
#[tokio::test]
async fn initialize_spawns_waits_probes_and_reports_the_group() {
    let (_stub, port) = stub_engine("gate-m", 2, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool.clone())
        .with_engine_key("k3y".into());

    let observation = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();

    let spawned = tool.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1);
    // Spec §3: the engine key reaches the child through the environment only.
    assert_eq!(
        spawned[0].env.get("VLLM_API_KEY").map(String::as_str),
        Some("k3y")
    );
    assert!(!spawned[0].argv.iter().any(|a| a == "k3y"));
    assert!(spawned[0]
        .env
        .get("PATH")
        .unwrap()
        .starts_with("/opt/venv/bin:"));
    assert_eq!(
        observation
            .identities
            .iter()
            .map(|i| i.role.as_str())
            .collect::<Vec<_>>(),
        ["api", "worker-0"]
    );
    assert_eq!(
        observation.facts,
        vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable
        ]
    );
    assert_eq!(observation.binding_id, "b-1");
    assert_eq!(observation.incarnation, "i-1");
    assert!(!observation.receipt.contains("k3y"));
}

/// SPEC §13.3 / T21: the engine's environment is a closed allowlist. Nothing
/// of the agent's own environment beyond it reaches vLLM; its plugins are
/// pinned off, it writes no bytecode, and with an admin key the development
/// routes are keyed apart from inference (the guard reads it).
// T21
#[tokio::test]
async fn the_engine_environment_is_a_closed_allowlist() {
    let (_stub, port) = stub_engine("gate-m", 2, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let mut launch = plan(port);
    launch.sleep_flags = vec!["--enable-sleep-mode".into()];
    launch.extra_approvals = Some(r#"{"options":[],"paths":[],"trust_remote_code":false}"#.into());
    let adapter = adapter(port, Some("k3y"))
        .with_launch(launch)
        .with_tools(tool.clone())
        .with_engine_key("k3y".into())
        .with_admin_key("adm1n".into());
    adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    let spawned = tool.spawned.lock().unwrap();
    let env = &spawned[0].env;
    let allowed = [
        "PATH",
        "HOME",
        "CUDA_VISIBLE_DEVICES",
        "HF_HUB_OFFLINE",
        "TRANSFORMERS_OFFLINE",
        "VLLM_API_KEY",
        "MLLM_VLLM_ADMIN_KEY",
        "VLLM_SERVER_DEV_MODE",
        "PYTHONPATH",
        "VLLM_PLUGINS",
        "PYTHONDONTWRITEBYTECODE",
        "MLLM_ENGINE_LOG",
        "MLLM_EXTRA_APPROVALS",
        "CUDA_HOME",
        "MAX_JOBS",
        "FLASHINFER_NVCC_THREADS",
    ];
    for name in env.keys() {
        assert!(
            allowed.contains(&name.as_str()),
            "{name} is not on the allowlist"
        );
    }
    assert_eq!(env.get("VLLM_PLUGINS").map(String::as_str), Some(""));
    assert_eq!(
        env.get("PYTHONDONTWRITEBYTECODE").map(String::as_str),
        Some("1")
    );
    assert_eq!(env.get("VLLM_API_KEY").map(String::as_str), Some("k3y"));
    assert_eq!(
        env.get("MLLM_VLLM_ADMIN_KEY").map(String::as_str),
        Some("adm1n")
    );
    assert!(env.contains_key("MLLM_EXTRA_APPROVALS"));
    // PATH is the engine's own bin and fixed system directories only: no
    // profile `cuda_home`, no CUDA bin (SPEC §13.3 as amended 2026-09-25).
    assert_eq!(
        env.get("PATH").map(String::as_str),
        Some("/opt/venv/bin:/usr/local/bin:/usr/bin:/bin")
    );
    assert!(!env.contains_key("CUDA_HOME"));
    // Owner decision 2026-09-25: the JIT build limits are always set.
    let jobs: usize = env["MAX_JOBS"].parse().unwrap();
    assert!(jobs >= 1, "{jobs}");
    assert_eq!(env["FLASHINFER_NVCC_THREADS"], "1");
}

/// SPEC §13.3 amendment (owner decision 2026-09-25): a profile's host-approved
/// `cuda_home` puts `<cuda_home>/bin` after the engine's own bin and sets
/// `CUDA_HOME` (vLLM treats FlashInfer as absent without `nvcc` on PATH, found
/// live); a profile `env` build limit overrides the computed one.
// T21
#[tokio::test]
async fn a_profile_cuda_home_and_build_limit_reach_the_engine() {
    let (_stub, port) = stub_engine("gate-m", 2, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let mut launch = plan(port);
    launch.cuda_home = Some("/usr/local/cuda-13.0".into());
    launch.build_env = [("MAX_JOBS".to_string(), "3".to_string())].into();
    let adapter = adapter(port, Some("k3y"))
        .with_launch(launch)
        .with_tools(tool.clone())
        .with_engine_key("k3y".into());
    adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    let spawned = tool.spawned.lock().unwrap();
    let env = &spawned[0].env;
    assert_eq!(
        env.get("PATH").map(String::as_str),
        Some("/opt/venv/bin:/usr/local/cuda-13.0/bin:/usr/local/bin:/usr/bin:/bin")
    );
    assert_eq!(env["CUDA_HOME"], "/usr/local/cuda-13.0");
    assert_eq!(env["MAX_JOBS"], "3");
}

/// Spec §4 step 4: a process that dies before readiness ends the step with the
/// log tail, so the operator reads the engine's own reason for leaving.
// T10
#[tokio::test]
async fn a_process_gone_before_readiness_fails_with_the_log_tail() {
    let (_stub, port) = stub_engine("gate-m", usize::MAX, "k3y").await;
    let log = std::env::temp_dir().join(format!(
        "mllm-task8-gone-{}-{}.log",
        std::process::id(),
        now_ms()
    ));
    std::fs::write(
        &log,
        "loading weights\nCUDA out of memory\nengine core failed to start\n",
    )
    .unwrap();
    let mut plan = plan(port);
    plan.engine_log = Some(log.to_string_lossy().into_owned());
    let tool = Arc::new(ScriptedTool::dies_on_spawn(api_identity()));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan)
        .with_tools(tool)
        .with_engine_key("k3y".into());

    let error = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();

    // SPEC §§6.4, 13.2: an engine gone before readiness is a launch failure
    // whose first line is the bounded summary; the log tail follows it.
    let RuntimeError::LaunchFailed(message) = error else {
        panic!("a dead engine is a launch failure, got {error:?}");
    };
    assert!(
        message.starts_with("the engine exited before readiness"),
        "{message}"
    );
    for line in [
        "loading weights",
        "CUDA out of memory",
        "engine core failed to start",
    ] {
        assert!(message.contains(line), "missing `{line}` in {message}");
    }
    std::fs::remove_file(&log).ok();
}

/// Spec §4: the builder stops two seconds before the context deadline, with the
/// process alive, so its own reason is the one the coordinator records.
// T10
#[tokio::test]
async fn a_deadline_with_the_process_alive_is_reported_as_such() {
    let (_stub, port) = stub_engine("gate-m", usize::MAX, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());

    let started = Instant::now();
    let error = adapter
        .execute_persisted(&initialize_command(3_000))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();

    let RuntimeError::Uncertain(message) = error else {
        panic!("a running engine past its bound is uncertain, got {error:?}");
    };
    assert!(message.contains("deadline"), "{message}");
    assert!(message.contains("alive"), "{message}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
}

/// Spec §4: every wait is bounded by the context deadline, the probe included.
/// A model that lists itself and then stalls on its first completion used to run
/// on the chat client's own bounds, which are minutes long, so the coordinator's
/// bare timeout decided the step and the builder's reason was lost.
// T10
#[tokio::test]
async fn a_probe_that_never_answers_ends_on_the_builders_own_deadline() {
    let (_stub, port) = serve_stub("gate-m", 0, "k3y", ChatBehaviour::Stalls, 0).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());

    let started = Instant::now();
    let error = adapter
        .execute_persisted(&initialize_command(4_000))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();

    let RuntimeError::Uncertain(message) = error else {
        panic!("a stalled probe is uncertain, got {error:?}");
    };
    assert!(message.contains("probe deadline"), "{message}");
    assert!(message.contains("alive"), "{message}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
}

/// The shape every live launch has: the process is up and its port is not
/// listening yet, for the whole weight-staging window. The builder must keep
/// polling through connection refused, which is what `check_readiness` maps to
/// Initializing, and finish when the engine finally binds. Without a test at this
/// shape, a change that turned a refused connection into a readiness error would
/// pass the suite and fail every launch on the host.
// T10
#[tokio::test]
async fn a_port_that_is_not_listening_yet_is_waited_out_not_failed() {
    let port = free_port().await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    // The engine binds only after the builder has watched the process a few
    // times, so every one of those polls met a refused connection.
    let polls_before_binding = 2;
    let watched = tool.present_calls.clone();
    let engine = tokio::spawn(async move {
        while watched.load(Ordering::SeqCst) < polls_before_binding {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        serve_stub("gate-m", 0, "k3y", ChatBehaviour::Answers, port).await
    });
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool.clone())
        .with_engine_key("k3y".into());

    let observation = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();

    let (stub, bound) = engine.await.unwrap();
    assert_eq!(bound, port, "the engine came up on the leased port");
    assert!(
        tool.present_calls.load(Ordering::SeqCst) >= polls_before_binding,
        "the builder polled the process while the port was closed"
    );
    assert!(
        stub.polls.load(Ordering::SeqCst) >= 1,
        "the builder asked the engine once it was listening"
    );
    assert_eq!(
        observation
            .identities
            .iter()
            .map(|i| i.role.as_str())
            .collect::<Vec<_>>(),
        ["api", "worker-0"]
    );
}

/// SPEC §6.1: the served model appearing in the list is not a model that answers.
/// A refused probe and an empty answer are separate exits, and neither reports the
/// step as done.
// T10
#[tokio::test]
async fn a_probe_that_is_refused_or_answers_with_nothing_fails_the_step() {
    for (behaviour, expected) in [
        (
            ChatBehaviour::Refuses,
            "engine listed the model but did not answer",
        ),
        (ChatBehaviour::Empty, "engine answered with empty content"),
    ] {
        let (_stub, port) = serve_stub("gate-m", 0, "k3y", behaviour, 0).await;
        let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
        let adapter = adapter(port, Some("k3y"))
            .with_launch(plan(port))
            .with_tools(tool)
            .with_engine_key("k3y".into());

        let error = adapter
            .execute_persisted(&initialize_command(30_000))
            .await
            .unwrap_err();

        let RuntimeError::Uncertain(message) = error else {
            panic!("an unanswered probe is uncertain, got {error:?}");
        };
        assert!(message.contains(expected), "{message}");
    }
}

/// Spec §4 step 6: a group that is only its API process does not cover the
/// processes holding the device, so reporting it would record an ownership set
/// that is not the launch.
// T10
#[tokio::test]
async fn a_group_without_a_worker_is_not_reported_as_the_launch() {
    let (_stub, port) = stub_engine("gate-m", 0, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), Vec::new()));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());

    let error = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();

    let RuntimeError::Uncertain(message) = error else {
        panic!("an incomplete group is uncertain, got {error:?}");
    };
    assert!(message.contains("engine group incomplete"), "{message}");
    assert!(message.contains("api:4242"), "{message}");
}

/// Spec §4 step 4: presence that cannot be established is retention, never
/// absence. The step fails and says which it was, instead of waiting out the
/// deadline or treating the engine as gone.
// T10
#[tokio::test]
async fn presence_that_cannot_be_established_ends_the_step() {
    let (_stub, port) = stub_engine("gate-m", usize::MAX, "k3y").await;
    let tool = Arc::new(ScriptedTool::unknown_presence(api_identity()));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());

    let error = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();

    let RuntimeError::Uncertain(message) = error else {
        panic!("unknown presence is uncertain, got {error:?}");
    };
    assert!(
        message.contains("presence could not be established"),
        "{message}"
    );
}

/// One launch per incarnation: a repeat would start a second engine holding the
/// same device memory while the first is still recorded.
// T10
#[tokio::test]
async fn a_second_initialize_for_the_same_incarnation_is_unsupported() {
    let (_stub, port) = stub_engine("gate-m", 0, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool.clone())
        .with_engine_key("k3y".into());

    assert!(adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .is_ok());
    assert!(matches!(
        adapter.execute_persisted(&initialize_command(30_000)).await,
        Err(RuntimeError::Unsupported)
    ));
    assert_eq!(tool.spawned.lock().unwrap().len(), 1);
}

/// Spec §3: the engine holds a key this adapter was not given, so its readiness
/// poll is refused, and the step fails rather than waiting out its deadline on an
/// engine that is up and will never answer it.
// T10
#[tokio::test]
async fn a_probe_with_the_wrong_key_does_not_pass() {
    let (_stub, port) = stub_engine("gate-m", 0, "0ther").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, None)
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());

    let error = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();

    let RuntimeError::Uncertain(message) = error else {
        panic!("a refused probe is uncertain, got {error:?}");
    };
    assert!(message.contains("readiness"), "{message}");
}

/// Spec §3: the engine key is the credential the adapter presents, whatever the
/// adapter was built with. The director builds it with none, because the key is
/// sealed per launch and attached afterwards; a stale one must not win either.
// T10
#[tokio::test]
async fn the_engine_key_is_the_one_presented() {
    let (_stub, port) = stub_engine("gate-m", 0, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("not-the-key"))
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());
    assert!(adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .is_ok());
}

/// Actions other than Initialize are refused for a launch-shaped step: Stop and
/// Probe have no persisted vLLM path, and Park and Restore (W4, `vllm_residency.rs`)
/// require retained identities and no launch settings. An adapter must never
/// appear to grant a control path it does not have.
#[tokio::test]
async fn other_actions_are_still_unsupported() {
    let (_stub, port) = stub_engine("gate-m", 0, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());
    for action in [
        RuntimeAction::Park,
        RuntimeAction::Restore,
        RuntimeAction::Stop,
        RuntimeAction::Probe,
    ] {
        let mut command = initialize_command(30_000);
        command.action = action;
        assert!(matches!(
            adapter.execute_persisted(&command).await,
            Err(RuntimeError::Unsupported)
        ));
    }
}

/// A builder without its tools, key or plan cannot launch anything: it refuses
/// rather than half-running the step.
#[tokio::test]
async fn a_builder_without_its_parts_refuses() {
    let (_stub, port) = stub_engine("gate-m", 0, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let no_tools = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_engine_key("k3y".into());
    let no_key = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool.clone());
    let no_plan = adapter(port, Some("k3y"))
        .with_tools(tool)
        .with_engine_key("k3y".into());
    for adapter in [no_tools, no_key, no_plan] {
        assert!(matches!(
            adapter.execute_persisted(&initialize_command(30_000)).await,
            Err(RuntimeError::Unsupported)
        ));
    }
}

/// Spec §4: the step is the owned-launch one. A context carrying retained
/// identities, or another engine's launch settings, is not this step.
#[tokio::test]
async fn a_context_that_is_not_an_owned_vllm_launch_is_unsupported() {
    let (_stub, port) = stub_engine("gate-m", 0, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("k3y"))
        .with_launch(plan(port))
        .with_tools(tool)
        .with_engine_key("k3y".into());

    let mut retained = initialize_command(30_000);
    retained.context.identities = ExecutionIdentities::Retained(vec![api_identity()]);
    assert!(matches!(
        adapter.execute_persisted(&retained).await,
        Err(RuntimeError::Unsupported)
    ));

    let mut other_family = initialize_command(30_000);
    other_family.context.launch_settings = Some(LaunchSettings::Sglang(sglang_settings()));
    assert!(matches!(
        adapter.execute_persisted(&other_family).await,
        Err(RuntimeError::Unsupported)
    ));
}

/// Another family's launch settings, for the check that a vLLM builder refuses a
/// command carrying a plan it was never verified against.
fn sglang_settings() -> mllm_domain::launch::SglangLaunchSettings {
    mllm_testkit::sglang_launch_settings()
}
