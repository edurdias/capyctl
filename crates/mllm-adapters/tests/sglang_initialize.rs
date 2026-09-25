//! Spec §4 contract for the SGLang builder's Initialize step.
//!
//! The builder is exercised against a local axum engine surface (a model list
//! that only names the served model once startup is far enough along, and a
//! chat surface that answers only for the right key) and a scripted process
//! tool that records what it was asked to spawn — including the protected
//! descriptor contents — without touching a real process. Nothing here
//! qualifies a native engine recipe: it proves the step's sequence, its
//! refusals and its evidence, not that SGLang starts.

use std::io::{Read, Seek};
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use mllm_adapters::protected::ProtectedLaunchDescriptors;
use mllm_adapters::sglang::SglangAdapter;
use mllm_adapters::traits::{
    AdapterError, EngineAdapter, MemberRef, OwnedProcessLaunch, ParkLevel, RenderedCommand,
    RuntimeAction, RuntimeCommand, RuntimeError,
};
use mllm_domain::completion::{
    ExecutionIdentities, Milestone, Presence, ProcessIdentity, StepExecutionContext,
    TransitionToken,
};
use mllm_domain::launch::{
    LaunchSettings, NativeDeviceSelection, NativeLaunch, NativeLaunchMetadata, SglangLaunchSettings,
};
use serde_json::{json, Value};

const BINDING: &str = "01K00000000000000000000001";
const INCARNATION: &str = "01K00000000000000000000002";
const DEPLOYMENT: &str = "01K00000000000000000000003";
const OPERATION: &str = "01K00000000000000000000004";
const STEP: &str = "01K00000000000000000000005";
const SESSION: &str = "01K00000000000000000000006";
const MODEL: &str = "toy";
const INFERENCE: &str = "inference-secret";
const ADMIN: &str = "admin-secret";
const CHECKPOINT: &str = "/private/checkpoints/qwen";
const REVISION: &str = "cdbee75f17c01a7cc42f958dc650907174af0554";
const RECIPE: &str = "sglang_engine_config_v2";

// ---------------------------------------------------------------- stub engine

#[derive(Clone)]
struct Stub {
    model: String,
    key: String,
    /// Model-list polls that answer with an empty list before the served name
    /// appears: a listening HTTP server is not model readiness (SPEC §6.1).
    ready_after: usize,
    polls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

struct RecordedRequest {
    path: String,
    authorization: String,
    body: Value,
}

/// SGLang guards every `/v1` route with the same key, `/v1/models` included,
/// so an unkeyed readiness poll is refused there before it ever reaches the
/// chat probe.
async fn models(State(stub): State<Stub>, headers: HeaderMap) -> axum::response::Response {
    stub.requests.lock().unwrap().push(RecordedRequest {
        path: "/v1/models".into(),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string(),
        body: Value::Null,
    });
    let presented = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
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
    stub.requests.lock().unwrap().push(RecordedRequest {
        path: "/v1/chat/completions".into(),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string(),
        body: body.clone(),
    });
    let presented = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
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

async fn serve_stub(model: &str, ready_after: usize, key: &str, port: u16) -> (Stub, u16) {
    let stub = Stub {
        model: model.into(),
        key: key.into(),
        ready_after,
        polls: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
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
/// script. It never signals or inspects a real process. The protected
/// descriptor contents are captured through duplicated descriptors so the
/// rendered command can be checked against what the launcher was handed.
struct ScriptedTool {
    identity: ProcessIdentity,
    present: Mutex<Presence>,
    group: Vec<ProcessIdentity>,
    spawned: Mutex<Vec<RenderedCommand>>,
    descriptors: Mutex<Vec<[Vec<u8>; 3]>>,
    gone_on_spawn: bool,
    present_calls: Arc<AtomicUsize>,
}

/// Read each protected descriptor through a duplicated descriptor, then rewind
/// so the original stays positioned at zero for a real child to read.
fn descriptor_contents(descriptors: &ProtectedLaunchDescriptors) -> [Vec<u8>; 3] {
    descriptors.numbers().map(|fd: RawFd| {
        let dup = nix::unistd::dup(fd).unwrap();
        let mut file = unsafe { std::fs::File::from_raw_fd(dup) };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        file.rewind().unwrap();
        bytes
    })
}

impl ScriptedTool {
    fn alive(identity: ProcessIdentity, workers: Vec<ProcessIdentity>) -> Self {
        let group = std::iter::once(identity.clone()).chain(workers).collect();
        Self {
            identity,
            present: Mutex::new(Presence::Alive),
            group,
            spawned: Mutex::new(Vec::new()),
            descriptors: Mutex::new(Vec::new()),
            gone_on_spawn: false,
            present_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The engine dies the moment it is spawned (the crash-before-readiness case).
    fn dies_on_spawn(identity: ProcessIdentity) -> Self {
        Self {
            gone_on_spawn: true,
            ..Self::alive(identity, Vec::new())
        }
    }
}

impl OwnedProcessLaunch for ScriptedTool {
    fn spawn_durable(
        &self,
        _incarnation: &str,
        _cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn spawn_durable_protected(
        &self,
        _incarnation: &str,
        cmd: &RenderedCommand,
        descriptors: &ProtectedLaunchDescriptors,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawned.lock().unwrap().push(cmd.clone());
        self.descriptors
            .lock()
            .unwrap()
            .push(descriptor_contents(descriptors));
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

fn wrapper() -> &'static std::path::Path {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::path::Path::new("/usr/bin/true")
            .canonicalize()
            .unwrap()
    })
}

fn settings() -> SglangLaunchSettings {
    mllm_testkit::sglang_launch_settings()
}

fn frozen_launch(port: u16) -> NativeLaunch {
    frozen_launch_with_device(port, None)
}

/// The same launch with the host policy's service-authorized physical UUID,
/// which is what the guarded launcher sets the child's CUDA namespace from.
fn frozen_launch_with_device(port: u16, physical_gpu_uuid: Option<&str>) -> NativeLaunch {
    frozen_launch_pinned(port, physical_gpu_uuid, None)
}

fn frozen_launch_pinned(
    port: u16,
    physical_gpu_uuid: Option<&str>,
    cuda_pci_index: Option<u32>,
) -> NativeLaunch {
    NativeLaunch::from_frozen_store(
        NativeLaunchMetadata {
            engine: "sglang".into(),
            recipe: RECIPE.into(),
            checkpoint_revision: REVISION.into(),
            served_name: MODEL.into(),
            binding_id: BINDING.into(),
            incarnation: INCARNATION.into(),
            endpoint: format!("http://127.0.0.1:{port}"),
            rendered_settings_digest: "a".repeat(64),
            placement_digest: None,
            device: NativeDeviceSelection {
                host_id: "host-a".into(),
                hardware_fingerprint: "hardware-v1".into(),
                device_id: "gpu0".into(),
                memory_domain: "uma".into(),
                physical_gpu_uuid: physical_gpu_uuid.map(str::to_owned),
                cuda_pci_index,
            },
        },
        CHECKPOINT.into(),
        "/opt/sglang/bin/python3".into(),
        format!("sglang-inference-{BINDING}"),
        format!("sglang-admin-{BINDING}"),
        settings(),
    )
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
                deployment_id: DEPLOYMENT.into(),
                revision: 1,
                generation: 1,
                operation_id: OPERATION.into(),
                step_id: STEP.into(),
            },
            binding_id: BINDING.into(),
            incarnation: INCARNATION.into(),
            issued_at_ms: now_ms() - 10,
            deadline_ms: now_ms() + deadline_in_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: Some("g-1".into()),
            launch_settings: Some(LaunchSettings::Sglang(settings())),
        },
    }
}

fn launch_log() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "mllm-sglang-initialize-{}-{}-{}.log",
        std::process::id(),
        now_ms(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// The fully equipped builder: launch, tools, credentials, wrapper, log and
/// the coordinator session ULID the descriptor's launch scope names.
fn equipped(launch: NativeLaunch, tool: Arc<ScriptedTool>, log: &std::path::Path) -> SglangAdapter {
    SglangAdapter::from_frozen(&launch, None)
        .unwrap()
        .with_credentials(INFERENCE.into(), ADMIN.into())
        .with_launch(launch)
        .with_tools(tool)
        .with_wrapper(wrapper().to_path_buf())
        .with_log(log.to_string_lossy().into_owned())
        .with_session(SESSION)
}

// ----------------------------------------------------------------------- tests

/// A builder without its launch, its tools, its credentials, its wrapper or
/// its coordinator session cannot launch anything: it refuses rather than
/// half-running the step.
#[tokio::test]
async fn a_builder_without_its_parts_refuses() {
    let (_stub, port) = serve_stub(MODEL, 0, INFERENCE, 0).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let no_launch = SglangAdapter::from_frozen(&frozen_launch(port), None)
        .unwrap()
        .with_credentials(INFERENCE.into(), ADMIN.into())
        .with_tools(tool.clone())
        .with_wrapper(wrapper().to_path_buf())
        .with_session(SESSION);
    let no_tools = SglangAdapter::from_frozen(&frozen_launch(port), None)
        .unwrap()
        .with_credentials(INFERENCE.into(), ADMIN.into())
        .with_launch(frozen_launch(port))
        .with_wrapper(wrapper().to_path_buf())
        .with_session(SESSION);
    let no_credentials = SglangAdapter::from_frozen(&frozen_launch(port), None)
        .unwrap()
        .with_launch(frozen_launch(port))
        .with_tools(tool.clone())
        .with_wrapper(wrapper().to_path_buf())
        .with_session(SESSION);
    let no_wrapper = SglangAdapter::from_frozen(&frozen_launch(port), None)
        .unwrap()
        .with_credentials(INFERENCE.into(), ADMIN.into())
        .with_launch(frozen_launch(port))
        .with_tools(tool.clone())
        .with_session(SESSION);
    let no_session = SglangAdapter::from_frozen(&frozen_launch(port), None)
        .unwrap()
        .with_credentials(INFERENCE.into(), ADMIN.into())
        .with_launch(frozen_launch(port))
        .with_tools(tool.clone())
        .with_wrapper(wrapper().to_path_buf());
    let empty_session = SglangAdapter::from_frozen(&frozen_launch(port), None)
        .unwrap()
        .with_credentials(INFERENCE.into(), ADMIN.into())
        .with_launch(frozen_launch(port))
        .with_tools(tool)
        .with_wrapper(wrapper().to_path_buf())
        .with_session("");
    for adapter in [
        no_launch,
        no_tools,
        no_credentials,
        no_wrapper,
        no_session,
        empty_session,
    ] {
        assert!(
            matches!(
                adapter.execute_persisted(&initialize_command(30_000)).await,
                Err(RuntimeError::Unsupported)
            ),
            "a missing part must refuse the step"
        );
    }
}

/// Spec §4: render, protected spawn, readiness, probe, enumerate, observe.
#[tokio::test]
async fn initialize_spawns_protected_waits_probes_and_reports_the_group() {
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let (_stub, port) = serve_stub(MODEL, 2, INFERENCE, 0).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let launch = frozen_launch(port);
    let adapter = equipped(launch, tool.clone(), &log)
        .with_extra_approvals(r#"{"options":[],"paths":[],"trust_remote_code":false}"#.into());

    let command = initialize_command(30_000);
    let observation = adapter.execute_persisted(&command).await.unwrap();

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
    assert_eq!(observation.token, command.context.token);
    assert_eq!(observation.binding_id, BINDING);
    assert_eq!(observation.incarnation, INCARNATION);
    assert_eq!(
        observation.receipt,
        format!("sglang {RECIPE} ready on http://127.0.0.1:{port}; probe answered")
    );

    let spawned = tool.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1);
    let argv = &spawned[0].argv;
    assert_eq!(
        &argv[..4],
        [
            "/opt/sglang/bin/python3",
            // SPEC §9.1 / T21: -B, no bytecode is written beside checked source.
            "-BIS",
            wrapper().to_str().unwrap(),
            "--public-settings-json"
        ]
    );
    let fds: Vec<i32> = argv[5..].iter().filter_map(|a| a.parse().ok()).collect();
    assert_eq!(
        &argv[5..],
        [
            "--launch-descriptor-fd",
            &argv[6],
            "--inference-credential-fd",
            &argv[8],
            "--admin-credential-fd",
            &argv[10]
        ]
    );
    assert_eq!(fds.len(), 3, "three distinct protected descriptor numbers");
    assert!(fds.iter().all(|fd| *fd >= 3));
    assert!(fds[0] != fds[1] && fds[1] != fds[2] && fds[0] != fds[2]);

    // SPEC §13.3: credentials ride protected descriptors, never argv or env.
    let env = &spawned[0].env;
    assert_eq!(
        env.get("MLLM_ENGINE_LOG").map(String::as_str),
        Some(log.to_string_lossy().as_ref())
    );
    // T21: SPEC §9.1, no bytecode beside checked runtime source.
    assert_eq!(
        env.get("PYTHONDONTWRITEBYTECODE").map(String::as_str),
        Some("1")
    );
    // T21: ADR 0014 §8, the host approvals the entry gates extras with.
    assert_eq!(
        env.get("MLLM_EXTRA_APPROVALS").map(String::as_str),
        Some(r#"{"options":[],"paths":[],"trust_remote_code":false}"#)
    );
    let rendered = argv.join(" ");
    for secret in [INFERENCE, ADMIN] {
        assert!(!rendered.contains(secret), "a credential reached argv");
        assert!(
            !env.values().any(|value| value.contains(secret)),
            "a credential reached the environment"
        );
    }

    // The launcher was handed exactly the private inputs the renderer pinned.
    let captured = tool.descriptors.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let [private, inference, admin] = &captured[0];
    let private: Value = serde_json::from_slice(private).unwrap();
    assert_eq!(private["schema_version"], 2);
    assert_eq!(private["kind"], "sglang_private_launch");
    assert_eq!(private["checkpoint_root"], CHECKPOINT);
    let public: Value = serde_json::from_str(&argv[4]).unwrap();
    assert_eq!(private["public_settings"], public);
    // The launch scope is the descriptor contract's v2 addition: the session
    // this builder was given, and the armed execution context, byte-compatible
    // with the controller's shared descriptor builder.
    let scope = &private["launch_scope"];
    assert_eq!(scope["session_id"], SESSION);
    assert_eq!(scope["deployment_id"], DEPLOYMENT);
    assert_eq!(scope["operation_id"], OPERATION);
    assert_eq!(scope["step_id"], STEP);
    assert_eq!(scope["revision"], 1);
    assert_eq!(scope["generation"], 1);
    assert_eq!(scope["binding_id"], BINDING);
    assert_eq!(scope["incarnation"], INCARNATION);
    assert_eq!(scope["issued_at_ms"], command.context.issued_at_ms);
    assert_eq!(scope["deadline_ms"], command.context.deadline_ms);
    assert_eq!(std::str::from_utf8(inference).unwrap(), INFERENCE);
    assert_eq!(std::str::from_utf8(admin).unwrap(), ADMIN);

    // The engine was asked about the served name with the launch's own key,
    // and the probe went through the authenticated inference path.
    let requests = _stub.requests.lock().unwrap();
    assert!(requests
        .iter()
        .any(|r| r.path == "/v1/models" && r.authorization == format!("Bearer {INFERENCE}")));
    let probe = requests
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .unwrap();
    assert_eq!(probe.authorization, format!("Bearer {INFERENCE}"));
    assert_eq!(probe.body["model"], MODEL);
    assert_eq!(probe.body["max_tokens"], 8);
    assert_eq!(probe.body["temperature"], 0);
    assert_eq!(probe.body["messages"][0]["content"], "Say ready.");
    assert!(tool.present_calls.load(Ordering::SeqCst) >= 1);
    assert!(_stub.polls.load(Ordering::SeqCst) >= 1);
    std::fs::remove_file(&log).ok();
}

/// The shape every live launch has: the process is up and its port is not
/// listening yet. The builder must keep polling through connection refused,
/// which is what `check_readiness` maps to Initializing, and finish when the
/// engine finally binds.
#[tokio::test]
async fn a_port_that_is_not_listening_yet_is_waited_out_not_failed() {
    let port = free_port().await;
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let watched = tool.present_calls.clone();
    let engine = tokio::spawn(async move {
        while watched.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        serve_stub(MODEL, 0, INFERENCE, port).await
    });
    let launch = frozen_launch(port);
    let adapter = equipped(launch, tool.clone(), &log);

    let observation = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();

    let (stub, bound) = engine.await.unwrap();
    assert_eq!(bound, port, "the engine came up on the leased port");
    assert!(
        tool.present_calls.load(Ordering::SeqCst) >= 2,
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
    std::fs::remove_file(&log).ok();
}

/// Spec §3: the engine holds a key this adapter was not given, so its readiness
/// poll is refused, and the step fails rather than waiting out its deadline on
/// an engine that is up and will never answer it.
// T10
#[tokio::test]
async fn a_readiness_poll_with_the_wrong_key_fails_the_step() {
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let (_stub, port) = serve_stub(MODEL, 0, "0ther", 0).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let launch = frozen_launch(port);
    let adapter = equipped(launch, tool, &log);

    let error = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();

    let RuntimeError::Uncertain(message) = error else {
        panic!("a refused probe is uncertain, got {error:?}");
    };
    assert!(message.contains("readiness"), "{message}");
    std::fs::remove_file(&log).ok();
}

/// Spec §4 step 4: a process that dies before readiness ends the step with the
/// log tail, so the operator reads the engine's own reason for leaving, and
/// every credential the engine echoed is blanked before the error is recorded.
#[tokio::test]
async fn an_engine_gone_before_readiness_fails_with_a_redacted_log_tail() {
    let log = launch_log();
    let echoed_key = format!("Bearer {}", "0".repeat(64));
    std::fs::write(
        &log,
        format!("loading weights\n{echoed_key}\nCUDA out of memory\nengine core failed to start\n"),
    )
    .unwrap();
    let (_stub, port) = serve_stub(MODEL, usize::MAX, INFERENCE, 0).await;
    let tool = Arc::new(ScriptedTool::dies_on_spawn(api_identity()));
    let launch = frozen_launch(port);
    let adapter = equipped(launch, tool, &log);

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
    assert!(
        !message.contains(&"0".repeat(64)),
        "a leaked key in {message}"
    );
    assert!(message.contains("<redacted>"), "{message}");
    std::fs::remove_file(&log).ok();
}

/// An adapter built without an observer refuses its control actions rather
/// than appearing to grant park (SPEC §9 posture).
// T16
#[tokio::test]
async fn an_observer_less_adapter_refuses_every_control_action() {
    let (_stub, port) = serve_stub(MODEL, 0, INFERENCE, 0).await;
    let launch = frozen_launch(port);
    let adapter = SglangAdapter::from_frozen(&launch, None)
        .unwrap()
        .with_credentials(INFERENCE.into(), ADMIN.into());
    let member = MemberRef {
        deployment_id: "d-1".into(),
        member_id: "m-1".into(),
    };
    assert!(matches!(
        adapter.park(&member, ParkLevel::One).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    assert!(matches!(
        adapter.restore(&member).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    assert!(matches!(
        adapter.reload_weights(&member).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    // A launch adapter holds no launch credential here: readiness is refused,
    // never quietly Initializing.
    let unkeyed = SglangAdapter::from_frozen(&frozen_launch(port), None).unwrap();
    assert!(unkeyed.check_readiness(&member).await.is_err());
    let command = RuntimeCommand {
        action: RuntimeAction::Drain,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d-1".into(),
                revision: 1,
                generation: 1,
                operation_id: "o-1".into(),
                step_id: "s-drain".into(),
            },
            binding_id: BINDING.into(),
            incarnation: INCARNATION.into(),
            issued_at_ms: now_ms() - 10,
            deadline_ms: now_ms() + 5_000,
            identities: ExecutionIdentities::Retained(vec![api_identity(), worker0()]),
            completion_target: None,
            grant_id: Some("g-1".into()),
            launch_settings: None,
        },
    };
    assert!(matches!(
        adapter.execute_persisted(&command).await,
        Err(RuntimeError::Unsupported)
    ));
}

/// The descriptor contract pin. The served name is the deployment's route
/// name (1..=256 printable bytes, no whitespace), the two kinds are the ones
/// `runtime/sglang_entry.py` accepts, and the private descriptor is schema
/// version 2 whose launch scope cross-checks against the public settings
/// exactly as the entry's `_validate_launch_scope` requires. The literals
/// live here and nowhere else in this file.
#[tokio::test]
async fn the_descriptor_contract_pin_holds() {
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let (_stub, port) = serve_stub(MODEL, 0, INFERENCE, 0).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let launch = frozen_launch(port);
    let adapter = equipped(launch, tool.clone(), &log);

    let command = initialize_command(30_000);
    adapter.execute_persisted(&command).await.unwrap();

    let spawned = tool.spawned.lock().unwrap();
    let public: Value = serde_json::from_str(&spawned[0].argv[4]).unwrap();
    assert_eq!(public["served_name"], "toy");
    assert_eq!(public["kind"], "sglang_launch");
    let captured = tool.descriptors.lock().unwrap();
    let private: Value = serde_json::from_slice(&captured[0][0]).unwrap();
    assert_eq!(private["kind"], "sglang_private_launch");
    assert_eq!(private["schema_version"], 2);
    // The entry checks the scope's identity against the public settings before
    // anything else about it, so the pin holds the same cross-check.
    let scope = &private["launch_scope"];
    assert_eq!(scope["session_id"], SESSION);
    assert_eq!(scope["binding_id"], public["binding_id"]);
    assert_eq!(scope["incarnation"], public["incarnation"]);
    assert_eq!(scope["revision"], command.context.token.revision);
    assert_eq!(scope["generation"], command.context.token.generation);
    assert_eq!(scope["issued_at_ms"], command.context.issued_at_ms);
    assert_eq!(scope["deadline_ms"], command.context.deadline_ms);
    assert!(
        scope["deadline_ms"].as_i64().unwrap() > scope["issued_at_ms"].as_i64().unwrap(),
        "the scope must bound its own issuance"
    );
    std::fs::remove_file(&log).ok();
}

/// Spec §4: the step is the owned-launch one. A context carrying retained
/// identities, or another engine's launch settings, is not this step.
#[tokio::test]
async fn a_context_that_is_not_an_owned_sglang_launch_is_unsupported() {
    let (_stub, port) = serve_stub(MODEL, 0, INFERENCE, 0).await;
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let launch = frozen_launch(port);
    let adapter = equipped(launch, tool, &log);

    let mut retained = initialize_command(30_000);
    retained.context.identities = ExecutionIdentities::Retained(vec![api_identity()]);
    assert!(matches!(
        adapter.execute_persisted(&retained).await,
        Err(RuntimeError::Unsupported)
    ));

    let mut other_family = initialize_command(30_000);
    other_family.context.launch_settings = Some(mllm_testkit::vllm_launch_settings());
    assert!(matches!(
        adapter.execute_persisted(&other_family).await,
        Err(RuntimeError::Unsupported)
    ));
    std::fs::remove_file(&log).ok();
}

/// One launch per incarnation: a repeat would start a second engine holding
/// the same device memory while the first is still recorded as owned.
#[tokio::test]
async fn a_second_initialize_for_the_same_incarnation_is_unsupported() {
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let (_stub, port) = serve_stub(MODEL, 0, INFERENCE, 0).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let launch = frozen_launch(port);
    let adapter = equipped(launch, tool.clone(), &log);

    assert!(adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .is_ok());
    assert!(matches!(
        adapter.execute_persisted(&initialize_command(30_000)).await,
        Err(RuntimeError::Unsupported)
    ));
    assert_eq!(tool.spawned.lock().unwrap().len(), 1);
    std::fs::remove_file(&log).ok();
}

/// The guarded launcher sets the child's `CUDA_VISIBLE_DEVICES` from the host
/// policy's service-frozen device mapping — never from profile env, which
/// rejects that name (`engine_policy.rs::SAFE_ENV`), and never into argv, which
/// the redaction contract forbids. The UUID is a launch parameter, not
/// descriptor content: the public settings' device object stays closed at the
/// four reviewed selectors, and the entry corroborates the inherited namespace
/// against the published placement digest instead (`runtime/sglang_device.py`).
#[tokio::test]
async fn the_guarded_launcher_sets_the_devices_cuda_namespace() {
    let uuid = "GPU-1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d";
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let (_stub, port) = serve_stub(MODEL, 0, INFERENCE, 0).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let launch = frozen_launch_with_device(port, Some(uuid));
    let adapter = equipped(launch, tool.clone(), &log);

    adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();

    let public_device_keys;
    {
        let spawned = tool.spawned.lock().unwrap();
        // T22: compiler tools resolve in the selected environment, not shell PATH.
        let engine_bin = std::path::Path::new(&spawned[0].argv[0]).parent().unwrap();
        assert_eq!(
            spawned[0].env["PATH"],
            format!("{}:/usr/bin:/bin", engine_bin.display())
        );
        assert_eq!(
            spawned[0]
                .env
                .get("CUDA_VISIBLE_DEVICES")
                .map(String::as_str),
            Some(uuid),
            "the child inherits exactly the mapped device's physical UUID"
        );
        // The namespace is a guarded launch parameter: it must not have
        // travelled as an argument the engine log or a journal could quote
        // back.
        let rendered = spawned[0].argv.join(" ");
        assert!(!rendered.contains("CUDA_VISIBLE_DEVICES"));
        assert!(!rendered.contains(uuid));

        // And the descriptor's device object stays closed at the four
        // reviewed selectors — the UUID never enters public settings.
        let public: Value = serde_json::from_str(&spawned[0].argv[4]).unwrap();
        public_device_keys = public["device"]
            .as_object()
            .unwrap()
            .keys()
            .map(|key| key.to_owned())
            .collect::<Vec<String>>();
    }
    assert_eq!(
        public_device_keys,
        [
            "device_id".to_owned(),
            "hardware_fingerprint".to_owned(),
            "host_id".to_owned(),
            "memory_domain".to_owned()
        ]
    );

    // A host that published no inventory UUID leaves the namespace unset, and
    // the placement gate then fails closed at the entry — the launcher does
    // not invent one.
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let launch = frozen_launch_with_device(port, None);
    let adapter = equipped(launch, tool.clone(), &log);
    adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert!(
        !tool.spawned.lock().unwrap()[0]
            .env
            .contains_key("CUDA_VISIBLE_DEVICES"),
        "no mapping, no namespace"
    );
    std::fs::remove_file(&log).ok();
}

/// Discrete GPU design §7 (controller ruling): a GPU the host published no
/// UUID for, on a host with a choice of GPU, is pinned by its index in PCI bus
/// order; the child never inherits every GPU. A published UUID wins.
// T27 T21
#[tokio::test]
async fn the_guarded_launcher_pins_an_unpublished_gpu_by_its_pci_index() {
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    let (_stub, port) = serve_stub(MODEL, 0, INFERENCE, 0).await;
    let cuda = |env: &std::collections::BTreeMap<String, String>| {
        env.iter()
            .filter(|(name, _)| name.starts_with("CUDA_"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>()
    };
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = equipped(
        frozen_launch_pinned(port, None, Some(1)),
        tool.clone(),
        &log,
    );
    adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert_eq!(
        cuda(&tool.spawned.lock().unwrap()[0].env),
        [
            ("CUDA_DEVICE_ORDER".to_string(), "PCI_BUS_ID".to_string()),
            ("CUDA_VISIBLE_DEVICES".to_string(), "1".to_string())
        ]
    );
    let uuid = "GPU-1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d";
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = equipped(
        frozen_launch_pinned(port, Some(uuid), None),
        tool.clone(),
        &log,
    );
    adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert_eq!(
        cuda(&tool.spawned.lock().unwrap()[0].env),
        [("CUDA_VISIBLE_DEVICES".to_string(), uuid.to_string())]
    );
    std::fs::remove_file(&log).ok();
}

/// SPEC §8.2 / T21 (found live 2026-09-23): the host-named rendezvous directory
/// reaches the entry as `MLLM_RENDEZVOUS_DIR`; without one none is set and the
/// entry makes its own.
// T21
#[tokio::test]
async fn a_host_named_rendezvous_directory_reaches_the_entry() {
    let log = launch_log();
    std::fs::write(&log, "").unwrap();
    for named in [Some("/state/rendezvous/01K00000000000000000000002"), None] {
        let (_stub, port) = serve_stub(MODEL, 2, INFERENCE, 0).await;
        let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
        let mut adapter = equipped(frozen_launch(port), tool.clone(), &log);
        if let Some(dir) = named {
            adapter = adapter.with_rendezvous_dir(dir.into());
        }
        adapter
            .execute_persisted(&initialize_command(30_000))
            .await
            .unwrap();
        let spawned = tool.spawned.lock().unwrap();
        assert_eq!(
            spawned[0]
                .env
                .get("MLLM_RENDEZVOUS_DIR")
                .map(String::as_str),
            named
        );
    }
}
