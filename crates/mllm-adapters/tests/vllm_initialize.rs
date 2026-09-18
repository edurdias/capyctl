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
use mllm_domain::launch::{ProfileLaunchSettings, VllmLaunchSettings, VllmRequestedBudget};

// ---------------------------------------------------------------- stub engine

#[derive(Clone)]
struct Stub {
    model: String,
    key: String,
    /// Model-list polls that answer with an empty list before the served name
    /// appears: a listening HTTP server is not model readiness (SPEC §6.1).
    ready_after: usize,
    polls: Arc<AtomicUsize>,
}

async fn models(State(stub): State<Stub>) -> Json<Value> {
    let seen = stub.polls.fetch_add(1, Ordering::SeqCst);
    let data = if seen >= stub.ready_after {
        json!([{ "id": stub.model, "object": "model" }])
    } else {
        json!([])
    };
    Json(json!({ "object": "list", "data": data }))
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
    let model = body["model"].as_str().unwrap_or_default().to_string();
    let chunks = vec![
        json!({
            "id": "probe-1", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ready"},
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
    let stub = Stub {
        model: model.into(),
        key: key.into(),
        ready_after,
        polls: Arc::new(AtomicUsize::new(0)),
    };
    let app = axum::Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (stub, addr.port())
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
        }
    }

    fn dies_on_spawn(identity: ProcessIdentity) -> Self {
        Self {
            group: vec![identity.clone()],
            gone_on_spawn: true,
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
        kv_cache_dtype: "auto".into(),
        block_size_tokens: 16,
        cpu_offload_bytes: 0,
        granted: GrantedBudget::default(),
        engine_args: vec![],
        sleep_flags: vec![],
        // Spec §3: the key rides the environment; it is never a plan field.
        api_key: None,
        engine_path_extra: Some("/opt/venv/bin".into()),
        engine_log: None,
        runtime_dir: None,
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
            launch_settings: Some(ProfileLaunchSettings::Vllm(VllmLaunchSettings {
                tensor_parallel_size: 1,
                pipeline_parallel_size: 1,
                enable_sleep_mode: false,
                kv_cache_dtype: "auto".into(),
                block_size_tokens: 16,
                cpu_offload_bytes: 0,
                requested_budget: VllmRequestedBudget {
                    kv_cache_bytes: 0,
                    swap_space_bytes: 0,
                    gpu_utilization_pct: 75,
                },
            })),
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

    let RuntimeError::Uncertain(message) = error else {
        panic!("a dead engine is uncertain ownership, got {error:?}");
    };
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

/// Spec §3: an unkeyed probe is refused by the engine, so the step fails rather
/// than reporting a model nobody proved answers.
// T10
#[tokio::test]
async fn a_probe_without_the_key_does_not_pass() {
    let (_stub, port) = stub_engine("gate-m", 0, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = adapter(port, Some("not-the-key"))
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
    assert!(message.contains("did not answer"), "{message}");
}

/// Actions other than Initialize stay refused until the S2 slice implements them:
/// an adapter must never appear to grant a control path it does not have.
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
    other_family.context.launch_settings =
        Some(ProfileLaunchSettings::Sglang(sglang_settings()));
    assert!(matches!(
        adapter.execute_persisted(&other_family).await,
        Err(RuntimeError::Unsupported)
    ));
}

/// Another family's launch settings, for the check that a vLLM builder refuses a
/// command carrying a plan it was never verified against.
fn sglang_settings() -> mllm_domain::launch::SglangLaunchSettings {
    mllm_domain::launch::SglangLaunchSettings {
        recipe: "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1".into(),
        tensor_parallel_size: 1,
        data_parallel_size: 1,
        tokenizer_workers: 1,
        model_dtype: "bfloat16".into(),
        context_tokens: 4096,
        max_running_requests: 8,
        max_total_tokens: 4096,
        prefill_cuda_graphs: false,
        decode_cuda_graphs: false,
        memory_saver: true,
        cpu_weight_backup: false,
        speculative_decoding: false,
        lora: false,
        trust_remote_code: false,
        disaggregation: false,
        external_cache: false,
        cpu_kv_offload: false,
        native_grpc: false,
        weight_restore: "disk_reload".into(),
        requested_budget: mllm_domain::launch::SglangRequestedBudget {
            kv_cache_bytes: 4_294_967_296,
            static_memory_fraction_bps: 7500,
        },
    }
}
