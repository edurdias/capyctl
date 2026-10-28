//! ADR 0038 contract for the TensorRT-LLM adapter (T49): readiness in order
//! (`/health`, `/version`, `/v1/models`, the chat probe, one `/metrics` read
//! with the built-pool check), the descendant and listener checks, the load
//! read from `/metrics` with an empty read unknown, never idle, and the
//! restart-only surface with no idle signal and no cancellation
//! acknowledgement. Exercised against a local axum engine with
//! `trtllm-serve` 1.2.1's shapes (`serve/openai_server.py`,
//! `executor/jsonSerialization.cpp`), a scripted process tool and a fake
//! `/proc`; nothing here qualifies TensorRT-LLM (only the live rows TR1–TR11,
//! TR1a–TR11a and TR1b–TR11b do).

use std::path::Path;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

use capyctl_adapters::process_tree::ENGINE_PROCESS_ESCAPED;
use capyctl_adapters::tensorrt_llm::{
    check_ownership, IterationLoad, OwnershipFailure, PlanInputTensorrtLlm, TensorrtLlmAdapter,
    EFFECTIVE_ARGS_MISMATCH, INSTALLATION_DRIFT, KV_CACHE_SHORT,
};
use capyctl_adapters::traits::{
    AdapterError, CancellationOutcome, ChatForward, EngineAdapter, MemberRef, OwnedProcessLaunch,
    ParkLevel, RenderedCommand, RequestRef, RuntimeAction, RuntimeCommand, RuntimeError,
    WorkObservation,
};
use capyctl_domain::completion::{
    ExecutionIdentities, Milestone, Presence, ProcessIdentity, StepExecutionContext,
    TransitionToken,
};
use capyctl_domain::launch::{
    CommonEngineSettings, LaunchSettings, MemoryRequest, TensorrtLlmLaunchSettings,
};

const PID: u32 = 4242;
const ESCAPED_PID: u32 = 4300;
const SERVED: &str = "qwen3-4b";
const VERSION: &str = "1.2.1";

/// A process tool that records what it was asked to spawn and stop and
/// answers from a script. It never signals or inspects a real process.
struct ScriptedTool {
    present: Presence,
    stops: bool,
    spawned: OnceLock<RenderedCommand>,
    terminated: Mutex<Option<Vec<ProcessIdentity>>>,
}

impl ScriptedTool {
    fn new(present: Presence) -> Arc<Self> {
        Self::stopping(present, true)
    }

    fn stopping(present: Presence, stops: bool) -> Arc<Self> {
        Arc::new(Self {
            present,
            stops,
            spawned: OnceLock::new(),
            terminated: Mutex::new(None),
        })
    }

    fn terminated(&self) -> Option<Vec<ProcessIdentity>> {
        self.terminated.lock().unwrap().clone()
    }
}

impl OwnedProcessLaunch for ScriptedTool {
    fn spawn_durable(
        &self,
        _incarnation: &str,
        cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawned
            .set(cmd.clone())
            .map_err(|_| RuntimeError::Uncertain("spawned twice".into()))?;
        Ok(api_identity())
    }

    fn present(&self, _identity: &ProcessIdentity) -> Presence {
        self.present
    }

    fn observe_group(&self, _api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        Ok(vec![api_identity()])
    }

    fn terminate_owned(
        &self,
        identities: &[ProcessIdentity],
        _grace: Duration,
    ) -> Result<(), RuntimeError> {
        *self.terminated.lock().unwrap() = Some(identities.to_vec());
        if self.stops {
            Ok(())
        } else {
            Err(RuntimeError::Uncertain(
                "a recorded process is still alive after SIGKILL".into(),
            ))
        }
    }
}

fn api_identity() -> ProcessIdentity {
    ProcessIdentity {
        role: "api".into(),
        pid: PID,
        boot_id: "boot".into(),
        start_ticks: 99,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// One `/metrics` entry as 1.2.1 writes it, with a pool of `blocks` blocks
/// of 32 tokens.
fn entry(blocks: u64, active: u64, queued: u64, used: u64) -> Value {
    json!({"timestamp": "10-28-2026 10:00:00", "iter": 3, "iterLatencyMS": 12.5,
        "numActiveRequests": active, "numQueuedRequests": queued, "numCompletedRequests": 0,
        "maxNumActiveRequests": 32, "gpuMemUsage": 123456789, "cpuMemUsage": 0,
        "pinnedMemUsage": 0,
        "inflightBatchingStats": {"numScheduledRequests": active, "numContextRequests": 0,
            "numGenRequests": active, "numPausedRequests": 0, "numCtxTokens": 0,
            "microBatchId": 0, "avgNumDecodedTokensPerIter": 1.0},
        "kvCacheStats": {"maxNumBlocks": blocks, "freeNumBlocks": blocks - used,
            "usedNumBlocks": used, "tokensPerBlock": 32, "allocTotalBlocks": used,
            "allocNewBlocks": used, "reusedBlocks": 0, "missedBlocks": 0, "cacheHitRate": 0.0}})
}

/// What the stub reports; tests set it before the stub runs.
struct Script {
    /// `/health` reads answered 503 before the executor runs.
    loading: AtomicUsize,
    version: Mutex<Value>,
    models: Mutex<Vec<String>>,
    /// The `/metrics` body, returned on every read.
    metrics: Mutex<Value>,
    empty_answer: AtomicBool,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            loading: AtomicUsize::new(0),
            version: Mutex::new(json!({"version": VERSION})),
            models: Mutex::new(vec![SERVED.into()]),
            // 910 blocks of 32 tokens: 29,120 tokens, between one window
            // (16,384) and `max_tokens` (29,127) plus one block.
            metrics: Mutex::new(json!([entry(910, 1, 0, 1)])),
            empty_answer: AtomicBool::new(false),
        }
    }
}

#[derive(Clone)]
struct Stub {
    script: Arc<Script>,
    health_reads: Arc<AtomicUsize>,
    chats: Arc<AtomicUsize>,
}

async fn health(State(stub): State<Stub>) -> axum::response::Response {
    stub.health_reads.fetch_add(1, SeqCst);
    let loading = stub
        .script
        .loading
        .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
        .is_ok();
    if loading {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    StatusCode::OK.into_response()
}

async fn version(State(stub): State<Stub>) -> axum::response::Response {
    Json(stub.script.version.lock().unwrap().clone()).into_response()
}

async fn models(State(stub): State<Stub>) -> axum::response::Response {
    let data: Vec<Value> = stub
        .script
        .models
        .lock()
        .unwrap()
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 1, "owned_by": "tensorrt_llm"}))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn metrics(State(stub): State<Stub>) -> axum::response::Response {
    Json(stub.script.metrics.lock().unwrap().clone()).into_response()
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
        "model": SERVED, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

async fn chat(State(stub): State<Stub>, Json(body): Json<Value>) -> axum::response::Response {
    stub.chats.fetch_add(1, SeqCst);
    assert_eq!(body["model"], SERVED, "the probe names the served model");
    let delta = if stub.script.empty_answer.load(SeqCst) {
        json!({"content": ""})
    } else {
        json!({"content": "Ready."})
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        chunk(delta, Value::Null),
        chunk(json!({}), json!("length")),
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

async fn stub_engine(change: impl FnOnce(&Script)) -> (Stub, u16) {
    let script = Script::default();
    change(&script);
    let stub = Stub {
        script: Arc::new(script),
        health_reads: Arc::default(),
        chats: Arc::default(),
    };
    let app = axum::Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/v1/models", get(models))
        .route("/metrics", get(metrics))
        .route("/v1/chat/completions", post(chat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (stub, port)
}

fn stat(pid: u32, parent: u32, group: u32, start: u64) -> String {
    format!(
        "{pid} (trtllm-serve) S {parent} {group} {group} 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 \
         {start} 1000 10 18446744073709551615\n"
    )
}

/// A fake `/proc` in which the API process (its own group's leader) holds
/// one listening socket on `address`; with `escaped`, it has a child that
/// left its process group, as Open MPI's singleton daemon would.
fn fake_proc(address: [u8; 4], port: u16, escaped: bool) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let process = root.path().join(PID.to_string());
    std::fs::create_dir_all(process.join("fd")).unwrap();
    std::fs::create_dir_all(process.join("net")).unwrap();
    std::fs::write(process.join("stat"), stat(PID, 1, PID, 99)).unwrap();
    std::os::unix::fs::symlink("socket:[777]", process.join("fd/3")).unwrap();
    std::os::unix::fs::symlink("/dev/null", process.join("fd/0")).unwrap();
    std::fs::write(
        process.join("net/tcp"),
        format!(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             \x20  0: {:08X}:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1 0 100 0 0 10 0\n",
            u32::from_ne_bytes(address)
        ),
    )
    .unwrap();
    std::fs::write(process.join("net/tcp6"), "  sl  local_address\n").unwrap();
    // An unrelated process, and a non-process entry.
    std::fs::create_dir_all(root.path().join("1")).unwrap();
    std::fs::write(root.path().join("1/stat"), stat(1, 0, 1, 1)).unwrap();
    std::fs::create_dir_all(root.path().join("self")).unwrap();
    if escaped {
        let child = root.path().join(ESCAPED_PID.to_string());
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("stat"), stat(ESCAPED_PID, PID, ESCAPED_PID, 120)).unwrap();
    }
    root
}

/// The checkpoint and the role's runtime directory one launch uses.
struct Dirs {
    checkpoint: tempfile::TempDir,
    runtime: tempfile::TempDir,
}

impl Dirs {
    fn new() -> Self {
        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(checkpoint.path().join("config.json"), "{}").unwrap();
        Self {
            checkpoint,
            runtime: tempfile::tempdir().unwrap(),
        }
    }

    fn launch_dir(&self) -> std::path::PathBuf {
        self.runtime.path().join("tensorrt-llm/i-1")
    }
}

fn plan(port: u16, dirs: &Dirs) -> PlanInputTensorrtLlm {
    PlanInputTensorrtLlm {
        engine_bin: "/home/u/tensorrt-llm-1.2.1-venv/bin/trtllm-serve".into(),
        engine_path_extra: Some("/home/u/tensorrt-llm-1.2.1-venv/bin".into()),
        version: VERSION.into(),
        served_model_name: SERVED.into(),
        checkpoint_dir: dirs.checkpoint.path().to_string_lossy().into_owned(),
        port,
        max_seq_len: 16384,
        max_batch_size: 32,
        max_tokens: 29127,
        tokens_per_block: 32,
        launch_dir: dirs.launch_dir().to_string_lossy().into_owned(),
        ..PlanInputTensorrtLlm::default()
    }
}

fn adapter(port: u16) -> TensorrtLlmAdapter {
    TensorrtLlmAdapter::new(
        format!("http://127.0.0.1:{port}").parse().unwrap(),
        VERSION.into(),
        SERVED.into(),
    )
}

fn launching(
    port: u16,
    dirs: &Dirs,
    tool: Arc<ScriptedTool>,
    proc_root: &Path,
) -> TensorrtLlmAdapter {
    adapter(port)
        .with_launch(plan(port, dirs))
        .with_tools(tool)
        .with_proc_root(proc_root.to_path_buf())
}

fn launch_settings() -> LaunchSettings {
    LaunchSettings::TensorrtLlm(TensorrtLlmLaunchSettings {
        common: CommonEngineSettings {
            context_length: Some(16384),
            ..CommonEngineSettings::default()
        },
        memory: MemoryRequest {
            request_bytes: 24 << 30,
            kv_cache_bytes: 4 << 30,
            margin_bytes: 0,
            weights_bytes: None,
            startup_bytes: None,
            device_total_bytes: None,
            overhead_bytes: None,
            startup_graphs_bytes: None,
            startup_eager_cap_bytes: None,
            state_slot_bytes: None,
            state_bytes: None,
            member: None,
            disk_tables: None,
            gguf: None,
        },
        tool_call_parser: None,
        reasoning_parser: None,
        llm_api_options: serde_json::Map::new(),
        extra_args: Vec::new(),
        provenance: Default::default(),
    })
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
            launch_settings: Some(launch_settings()),
        },
    }
}

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d-1".into(),
        member_id: "b-1".into(),
    }
}

// T49 T21 (ADR 0038 §3, §5, §9, SPEC §6.1): the launch directory is made, the
// rendered command spawned with the closed environment, `/health` 503 waited
// through, every step of readiness passed in order, and the one process
// reported; no key reaches the engine.
#[tokio::test]
async fn initialize_passes_every_step_in_order() {
    let (stub, port) = stub_engine(|s| s.loading.store(3, SeqCst)).await;
    let dirs = Dirs::new();
    let proc_root = fake_proc([127, 0, 0, 1], port, false);
    let tool = ScriptedTool::new(Presence::Alive);
    let observation = launching(port, &dirs, tool.clone(), proc_root.path())
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert!(
        stub.health_reads.load(SeqCst) >= 4,
        "a listening port is not readiness"
    );
    assert_eq!(stub.chats.load(SeqCst), 1);
    assert_eq!(observation.identities, vec![api_identity()]);
    assert!(observation.facts.contains(&Milestone::ModelUsable));
    assert!(
        observation.receipt.contains("KV pool of 29120 tokens"),
        "{}",
        observation.receipt
    );
    assert!(tool.terminated().is_none(), "a ready launch is not stopped");
    let spawned = tool.spawned.get().expect("one spawn");
    assert_eq!(spawned.argv[1], "serve");
    assert_eq!(
        spawned.argv[2],
        dirs.launch_dir()
            .join("model")
            .join(SERVED)
            .to_string_lossy()
    );
    assert!(!spawned.env.keys().any(|k| k.contains("KEY")));
    assert_eq!(spawned.env["TLLM_WORKER_USE_SINGLE_PROCESS"], "1");
    assert_eq!(spawned.env["HF_HUB_OFFLINE"], "1");
    assert_eq!(
        spawned.env["TMPDIR"],
        dirs.launch_dir().join("tmp").to_string_lossy()
    );
    assert!(dirs.launch_dir().join("llm_api_options.yaml").is_file());
    assert_eq!(
        std::fs::read_link(dirs.launch_dir().join("model").join(SERVED)).unwrap(),
        dirs.checkpoint.path()
    );
}

/// One way the stub answers otherwise than a correct launch.
type Change = fn(&Script);

// T49 T14 (ADR 0038 §4, §5, SPEC §8.2): readiness fails at each step, with
// its code: another version (`installation_drift`), a model list that is not
// exactly the served name, an empty probe answer, a `/metrics` read with no
// iteration entry, and a built pool above `max_tokens` plus one block
// (`effective_args_mismatch`); nothing after the failed step runs.
#[tokio::test]
async fn readiness_fails_at_each_step() {
    let cases: [(&str, &str, bool, Change); 6] = [
        (INSTALLATION_DRIFT, "1.3.0", false, |s| {
            *s.version.lock().unwrap() = json!({"version": "1.3.0"})
        }),
        (INSTALLATION_DRIFT, "no version", false, |s| {
            *s.version.lock().unwrap() = json!({})
        }),
        (EFFECTIVE_ARGS_MISMATCH, "/v1/models", false, |s| {
            *s.models.lock().unwrap() = vec!["model".into()]
        }),
        (EFFECTIVE_ARGS_MISMATCH, "/v1/models", false, |s| {
            *s.models.lock().unwrap() = vec![SERVED.into(), "other".into()]
        }),
        (EFFECTIVE_ARGS_MISMATCH, "no iteration entry", true, |s| {
            *s.metrics.lock().unwrap() = json!([])
        }),
        (EFFECTIVE_ARGS_MISMATCH, "32000 tokens", true, |s| {
            *s.metrics.lock().unwrap() = json!([entry(1000, 1, 0, 1)])
        }),
    ];
    for (code, what, probed, change) in cases {
        let (stub, port) = stub_engine(change).await;
        let dirs = Dirs::new();
        let proc_root = fake_proc([127, 0, 0, 1], port, false);
        let error = launching(
            port,
            &dirs,
            ScriptedTool::new(Presence::Alive),
            proc_root.path(),
        )
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();
        let RuntimeError::Uncertain(text) = &error else {
            panic!("{what}: {error:?}");
        };
        assert!(
            text.starts_with(code) && text.contains(what),
            "{what}: {text}"
        );
        assert_eq!(stub.chats.load(SeqCst) == 1, probed, "{what}");
    }
    let (_stub, port) = stub_engine(|s| s.empty_answer.store(true, SeqCst)).await;
    let dirs = Dirs::new();
    let proc_root = fake_proc([127, 0, 0, 1], port, false);
    let error = launching(
        port,
        &dirs,
        ScriptedTool::new(Presence::Alive),
        proc_root.path(),
    )
    .execute_persisted(&initialize_command(30_000))
    .await
    .unwrap_err();
    assert!(error.to_string().contains("empty content"), "{error}");
}

// T49 (ADR 0038 §4, §16): a built pool below one window stops the launch and
// fails it `kv_cache_short`, naming both figures.
#[tokio::test]
async fn a_pool_below_one_window_fails_kv_cache_short() {
    let (_stub, port) =
        stub_engine(|s| *s.metrics.lock().unwrap() = json!([entry(100, 1, 0, 1)])).await;
    let dirs = Dirs::new();
    let proc_root = fake_proc([127, 0, 0, 1], port, false);
    let tool = ScriptedTool::new(Presence::Alive);
    let error = launching(port, &dirs, tool.clone(), proc_root.path())
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();
    let RuntimeError::LaunchFailed(text) = &error else {
        panic!("{error:?}");
    };
    assert!(text.starts_with(KV_CACHE_SHORT), "{text}");
    assert!(
        text.contains("3200 tokens") && text.contains("16384 tokens"),
        "{text}"
    );
    assert!(text.ends_with("the engine was stopped"), "{text}");
    assert_eq!(tool.terminated(), Some(vec![api_identity()]));
}

// T49 T13 (SPEC §6.4): an engine that exits before readiness fails the
// launch with its summary.
#[tokio::test]
async fn an_engine_that_exits_fails_the_launch() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let dirs = Dirs::new();
    let proc_root = fake_proc([127, 0, 0, 1], port, false);
    let error = launching(
        port,
        &dirs,
        ScriptedTool::new(Presence::Gone),
        proc_root.path(),
    )
    .execute_persisted(&initialize_command(30_000))
    .await
    .unwrap_err();
    assert!(matches!(error, RuntimeError::LaunchFailed(_)), "{error:?}");
}

// T49 (ADR 0038 §9, §16): a descendant of the engine outside the launch's
// process group stops the launch with that process in the set proven gone,
// and fails it `engine_process_escaped`, naming the process; when it cannot
// be proven gone the launch stays uncertain, never failed and released.
#[tokio::test]
async fn a_descendant_outside_the_group_fails_engine_process_escaped() {
    for stops in [true, false] {
        let (_stub, port) = stub_engine(|_| {}).await;
        let dirs = Dirs::new();
        let proc_root = fake_proc([127, 0, 0, 1], port, true);
        let tool = ScriptedTool::stopping(Presence::Alive, stops);
        let error = launching(port, &dirs, tool.clone(), proc_root.path())
            .execute_persisted(&initialize_command(30_000))
            .await
            .unwrap_err();
        let text = match (&error, stops) {
            (RuntimeError::LaunchFailed(text), true) => text,
            (RuntimeError::Uncertain(text), false) => text,
            _ => panic!("stops {stops}: {error:?}"),
        };
        assert!(text.starts_with(ENGINE_PROCESS_ESCAPED), "{text}");
        assert!(text.contains(&format!("process {ESCAPED_PID}")), "{text}");
        let stopped = tool.terminated().expect("the launch was stopped");
        assert_eq!(stopped[0], api_identity());
        assert_eq!(stopped.len(), 2);
        assert_eq!(stopped[1].pid, ESCAPED_PID);
        assert_eq!(stopped[1].start_ticks, 120);
        assert_eq!(stopped[1].role, "escaped-0");
    }
}

// T49 T21 (ADR 0038 §9, ADR 0031 §7): a listener of an owned process off
// loopback, on a launch on one GPU, stops the launch and fails it
// `engine_listener_exposed`.
#[tokio::test]
async fn a_listener_off_loopback_fails_engine_listener_exposed() {
    let (_stub, port) = stub_engine(|_| {}).await;
    let dirs = Dirs::new();
    let proc_root = fake_proc([0, 0, 0, 0], port, false);
    let tool = ScriptedTool::new(Presence::Alive);
    let error = launching(port, &dirs, tool.clone(), proc_root.path())
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();
    let RuntimeError::LaunchFailed(text) = &error else {
        panic!("{error:?}");
    };
    assert!(
        text.starts_with(&format!(
            "engine_listener_exposed: the api process {PID} listens on address 0.0.0.0 port {port}"
        )),
        "{text}"
    );
    assert_eq!(tool.terminated(), Some(vec![api_identity()]));
}

// T49 (ADR 0038 §9): the re-proof entry point the host calls on a running
// launch: clean, escaped, exposed, or unreadable without a recorded leader.
#[test]
fn the_reproof_checks_both() {
    let present = |_: &ProcessIdentity| Presence::Alive;
    let clean = fake_proc([127, 0, 0, 1], 8101, false);
    check_ownership(clean.path(), &[api_identity()], &present).unwrap();
    let escaped = fake_proc([127, 0, 0, 1], 8101, true);
    let failure = check_ownership(escaped.path(), &[api_identity()], &present).unwrap_err();
    assert_eq!(failure.code(), Some(ENGINE_PROCESS_ESCAPED));
    assert_eq!(failure.stop_set(&[api_identity()]).len(), 2);
    let exposed = fake_proc([0, 0, 0, 0], 8101, false);
    let failure = check_ownership(exposed.path(), &[api_identity()], &present).unwrap_err();
    assert!(matches!(failure, OwnershipFailure::Exposed(_)));
    assert_eq!(failure.code(), Some("engine_listener_exposed"));
    assert_eq!(failure.stop_set(&[api_identity()]), vec![api_identity()]);
    let failure = check_ownership(clean.path(), &[], &present).unwrap_err();
    assert_eq!(failure.code(), None);
}

// T49 T17 (ADR 0038 §6, §7, ADR 0031 §6): the load is the newest `/metrics`
// entry; a read with no entry is unknown, never zero and never idle; the
// adapter declares no idle signal, never shows the engine quiescent and
// never reads `/metrics` for work, so the host's sampler stays its only
// reader.
#[tokio::test]
async fn an_empty_metrics_read_is_unknown_never_idle() {
    let (stub, port) = stub_engine(|s| {
        *s.metrics.lock().unwrap() = json!([entry(910, 1, 0, 1), entry(910, 3, 2, 455)])
    })
    .await;
    let engine = adapter(port);
    assert_eq!(
        engine.load().await.unwrap(),
        Some(IterationLoad {
            running: 3,
            waiting: 2,
            kv_usage_ppm: 500_000
        })
    );
    *stub.script.metrics.lock().unwrap() = json!([]);
    assert_eq!(engine.load().await.unwrap(), None);
    assert_eq!(
        engine.observe_work(&member()).await.unwrap(),
        WorkObservation::Unknown
    );
    assert_eq!(engine.idle_before_signal(&member()).await, None);
    assert!(!engine.engine_quiescent(&member(), 0).await);
    assert!(!engine.prepare_park(&member()).await.unwrap().quiescent);
    *stub.script.metrics.lock().unwrap() = json!({"error": "x"});
    assert!(
        engine.load().await.is_err(),
        "a malformed body is not a reading"
    );
}

// T49 T17 (ADR 0038 §7, §8): cancellation is never acknowledged; the
// adapter is restart-only; a `cache_salt` reaching the adapter's own
// forwarder is refused, never forwarded unchanged.
#[tokio::test]
async fn cancellation_is_uncertain_and_the_surface_restart_only() {
    let (stub, port) = stub_engine(|_| {}).await;
    let engine = adapter(port);
    for ack in [false, true] {
        assert_eq!(
            engine
                .cancel_work(&member(), &RequestRef { id: "r-1".into() }, ack)
                .await
                .unwrap(),
            CancellationOutcome::Uncertain
        );
    }
    assert!(matches!(
        engine.park(&member(), ParkLevel::One).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    assert!(matches!(
        engine.restore(&member()).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    let mut command = initialize_command(30_000);
    command.action = RuntimeAction::Park;
    assert_eq!(
        engine.execute_persisted(&command).await.unwrap_err(),
        RuntimeError::Unsupported
    );
    let request = json!({"model": SERVED, "messages": [], "cache_salt": "tenant-a"});
    assert!(matches!(
        engine.forward_chat(&request).await,
        Err(AdapterError::CacheSaltUnsupported)
    ));
    assert_eq!(stub.chats.load(SeqCst), 0);
}
