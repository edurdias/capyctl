//! ADR 0029 contract for the llama.cpp adapter (T42): readiness from
//! `/health`, `/v1/models` and `/props` with the rendered-value check, the
//! `/metrics` idle gate and quiescence, the restart-only surface, the
//! `cache_salt` refusal, SSE comments, and the launch-time `config.ini`
//! refusal. Exercised against a local axum engine with llama-server 0.6.0's
//! shapes (`tools/server/server-context.cpp`, `server-http.cpp`,
//! `server-task.cpp`) and a scripted process tool; nothing here qualifies a
//! native engine recipe (only the live rows LC1–LC6 do).

use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

use capyctl_adapters::llamacpp::{
    LlamacppAdapter, PlanInputLlamacpp, EFFECTIVE_ARGS_MISMATCH, ENGINE_CONFIG_FILE,
};
use capyctl_adapters::traits::{
    AdapterError, ChatForward, ChatSink, DeliveryFailed, EngineAdapter, EngineWork, MemberRef,
    OwnedProcessLaunch, ParkLevel, RenderedCommand, RuntimeAction, RuntimeCommand, RuntimeError,
    StreamEnded,
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
    present: Presence,
    spawned: OnceLock<RenderedCommand>,
}

impl ScriptedTool {
    fn new(present: Presence) -> Arc<Self> {
        Arc::new(Self {
            present,
            spawned: OnceLock::new(),
        })
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

/// What the stub reports; tests change it while the stub runs.
#[derive(Default)]
struct Reported {
    /// Requests answered 503 `Loading model` before the model is loaded.
    loading: AtomicUsize,
    /// `/v1/models` reads, after loading, that do not list the model yet.
    unlisted: AtomicUsize,
    total_slots: AtomicU64,
    n_ctx: AtomicU64,
    n_ctx_train: AtomicU64,
    endpoint_metrics: AtomicBool,
    processing: AtomicU64,
    deferred: AtomicU64,
    /// `/metrics` without the work gauges.
    no_gauges: AtomicBool,
    /// `/metrics` never answers.
    hung_metrics: AtomicBool,
    reasoning_only: AtomicBool,
}

/// Take one from `counter` while it is positive.
fn take(counter: &AtomicUsize) -> bool {
    counter
        .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

#[derive(Clone)]
struct Stub {
    model: String,
    reported: Arc<Reported>,
    health_reads: Arc<AtomicUsize>,
    chats: Arc<AtomicUsize>,
}

impl Stub {
    /// llama-server answers every route 503 until its model is loaded.
    fn loading(&self) -> Option<axum::response::Response> {
        take(&self.reported.loading).then(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": {"message": "Loading model", "type": "unavailable_error", "code": 503}})),
            )
                .into_response()
        })
    }
}

async fn health(State(stub): State<Stub>) -> axum::response::Response {
    stub.health_reads.fetch_add(1, Ordering::SeqCst);
    if let Some(loading) = stub.loading() {
        return loading;
    }
    Json(json!({"status": "ok"})).into_response()
}

async fn models(State(stub): State<Stub>) -> axum::response::Response {
    if let Some(loading) = stub.loading() {
        return loading;
    }
    let reported = &stub.reported;
    if take(&reported.unlisted) {
        return Json(json!({"object": "list", "data": []})).into_response();
    }
    Json(json!({"models": [{"name": stub.model, "model": stub.model}], "object": "list",
        "data": [{"id": stub.model, "aliases": [stub.model], "object": "model", "owned_by": "llamacpp",
            "meta": {"vocab_type": 2, "n_vocab": 151936, "n_ctx": reported.n_ctx.load(SeqCst),
                "n_ctx_train": reported.n_ctx_train.load(SeqCst), "n_embd": 1024,
                "n_params": 1, "size": 1}}]}))
    .into_response()
}

async fn props(State(stub): State<Stub>) -> axum::response::Response {
    if let Some(loading) = stub.loading() {
        return loading;
    }
    let reported = &stub.reported;
    Json(
        json!({"default_generation_settings": {"n_ctx": reported.n_ctx.load(SeqCst)},
        "total_slots": reported.total_slots.load(SeqCst), "model_alias": stub.model,
        "model_path": "/srv/models/qwen/qwen-Q4_K_M.gguf", "endpoint_slots": true,
        "endpoint_props": false, "endpoint_metrics": reported.endpoint_metrics.load(SeqCst),
        "ui": false, "chat_template": "{{ messages }}", "build_info": "b1-d812350",
        "is_sleeping": false}),
    )
    .into_response()
}

async fn metrics(State(stub): State<Stub>) -> axum::response::Response {
    if let Some(loading) = stub.loading() {
        return loading;
    }
    let reported = &stub.reported;
    if reported.hung_metrics.load(SeqCst) {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
    let mut text = String::from(
        "# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed\n\
         # TYPE llamacpp:prompt_tokens_total counter\n\
         llamacpp:prompt_tokens_total 12\n",
    );
    if !reported.no_gauges.load(SeqCst) {
        text.push_str(&format!(
            "# HELP llamacpp:requests_processing Number of requests processing\n\
             # TYPE llamacpp:requests_processing gauge\n\
             llamacpp:requests_processing {}\n\
             # HELP llamacpp:requests_deferred Number of requests deferred\n\
             # TYPE llamacpp:requests_deferred gauge\n\
             llamacpp:requests_deferred {}\n",
            reported.processing.load(SeqCst),
            reported.deferred.load(SeqCst),
        ));
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        text,
    )
        .into_response()
}

fn chunk(model: &str, delta: Value, finish: Value) -> Value {
    json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
        "model": model, "system_fingerprint": "b1-d812350",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

async fn chat(State(stub): State<Stub>, Json(body): Json<Value>) -> axum::response::Response {
    stub.chats.fetch_add(1, Ordering::SeqCst);
    if let Some(loading) = stub.loading() {
        return loading;
    }
    assert_eq!(
        body["model"], stub.model,
        "the forwarded request keeps the served name"
    );
    let delta = if stub.reported.reasoning_only.load(SeqCst) {
        json!({"reasoning_content": "The user wants"})
    } else {
        json!({"content": "Ready."})
    };
    let mut end = chunk(&stub.model, json!({}), json!("length"));
    end["timings"] = json!({"cache_n": 0, "prompt_n": 3, "prompt_ms": 4.2,
        "predicted_n": 8, "predicted_ms": 80.0, "predicted_per_second": 100.0});
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            &stub.model,
            json!({"role": "assistant", "content": null}),
            Value::Null
        ),
        chunk(&stub.model, delta, Value::Null),
        end
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

fn reported() -> Reported {
    let reported = Reported::default();
    reported.total_slots.store(4, SeqCst);
    // pad256(8000) = 8192, the slot window `plan` renders.
    reported.n_ctx.store(8192, SeqCst);
    reported.n_ctx_train.store(32768, SeqCst);
    reported.endpoint_metrics.store(true, SeqCst);
    reported
}

async fn serve(app: axum::Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

async fn stub_engine(change: impl FnOnce(&Reported)) -> (Stub, u16) {
    let state = reported();
    change(&state);
    let stub = Stub {
        model: "qwen".into(),
        reported: Arc::new(state),
        health_reads: Arc::default(),
        chats: Arc::default(),
    };
    let app = axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/props", get(props))
        .route("/metrics", get(metrics))
        .route("/v1/chat/completions", post(chat))
        .with_state(stub.clone());
    let port = serve(app).await;
    (stub, port)
}

fn plan(port: u16) -> PlanInputLlamacpp {
    PlanInputLlamacpp {
        engine_bin: "/opt/llama.cpp/bin/llama-server".into(),
        engine_path_extra: Some("/opt/llama.cpp/bin".into()),
        model_file: "/srv/models/qwen/qwen-Q4_K_M.gguf".into(),
        served_model_name: "qwen".into(),
        port,
        context_length: 8000,
        slots: 4,
        cache_type: "f16".into(),
        config_dir: "/var/lib/capyctl/engines/llamacpp/config".into(),
        cache_dir: "/var/lib/capyctl/engines/llamacpp/cache".into(),
        ..PlanInputLlamacpp::default()
    }
}

/// An adapter for `port` that looks for `etc/llama.cpp/config.ini` under
/// `root`, never under the machine's own `/`.
fn adapter(port: u16, root: &std::path::Path) -> LlamacppAdapter {
    LlamacppAdapter::new(url(port), "0.6.0+d812350".into(), "qwen".into())
        .with_system_root(root.to_path_buf())
}

fn launching(port: u16, root: &std::path::Path, tool: Arc<ScriptedTool>) -> LlamacppAdapter {
    adapter(port, root).with_launch(plan(port)).with_tools(tool)
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
            launch_settings: Some(capyctl_testkit::llamacpp_launch_settings()),
        },
    }
}

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d-1".into(),
        member_id: "b-1".into(),
    }
}

// T42 (ADR 0029 §10, SPEC §6.1): spawn the rendered command, wait through
// `/health` 503 `Loading model` and a model list that does not name the
// served model yet, verify the slot settings, probe, and report the one
// process; no key reaches the engine.
#[tokio::test]
async fn initialize_waits_for_health_and_the_served_name() {
    let (stub, port) = stub_engine(|r| {
        r.loading.store(3, SeqCst);
        r.unlisted.store(2, SeqCst);
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let tool = ScriptedTool::new(Presence::Alive);
    let observation = launching(port, root.path(), tool.clone())
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert!(
        stub.health_reads.load(Ordering::SeqCst) >= 4,
        "a listening port is not readiness"
    );
    assert_eq!(observation.identities, vec![api_identity()]);
    assert!(observation.facts.contains(&Milestone::ModelUsable));
    assert!(observation.receipt.contains("slots verified"));
    let spawned = tool.spawned.get().expect("one spawn");
    let argv = &spawned.argv;
    assert_eq!(argv[0], "/opt/llama.cpp/bin/llama-server");
    assert!(argv.windows(2).any(|w| w == ["--ctx-size", "32768"]));
    assert!(argv.windows(2).any(|w| w == ["--parallel", "4"]));
    assert!(!spawned.env.keys().any(|k| k.contains("KEY")));
    assert_eq!(
        spawned.env.get("XDG_CONFIG_HOME").map(String::as_str),
        Some("/var/lib/capyctl/engines/llamacpp/config")
    );
}

/// One way the stub reports other settings than rendered.
type Change = fn(&Reported);

// T42 T14 (ADR 0029 §10, SPEC §8.2): a `/props` slot count, a `/v1/models`
// window or an endpoint switch other than rendered fails Initialize with
// `effective_args_mismatch`, before the probe.
#[tokio::test]
async fn other_slot_settings_than_rendered_fail_effective_args_mismatch() {
    let cases: [(&str, Change); 4] = [
        ("total_slots", |r| r.total_slots.store(2, SeqCst)),
        ("n_ctx", |r| r.n_ctx.store(2048, SeqCst)),
        ("n_ctx", |r| r.n_ctx.store(32768, SeqCst)),
        ("endpoint_metrics", |r| {
            r.endpoint_metrics.store(false, SeqCst)
        }),
    ];
    for (what, change) in cases {
        let (stub, port) = stub_engine(change).await;
        let root = tempfile::tempdir().unwrap();
        let error = launching(port, root.path(), ScriptedTool::new(Presence::Alive))
            .execute_persisted(&initialize_command(30_000))
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains(EFFECTIVE_ARGS_MISMATCH) && text.contains(what),
            "{what}: {text}"
        );
        assert_eq!(stub.chats.load(Ordering::SeqCst), 0, "{what}: no probe");
    }
}

// T42 (ADR 0029 §10): llama-server caps each slot's window at the training
// context while it keeps the cache; that capped window is the rendered one.
#[tokio::test]
async fn a_window_capped_at_the_training_context_is_the_rendered_one() {
    let (_stub, port) = stub_engine(|r| {
        r.n_ctx.store(8000, SeqCst);
        r.n_ctx_train.store(8000, SeqCst);
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    launching(port, root.path(), ScriptedTool::new(Presence::Alive))
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
}

// T42 (ADR 0029 §10): reasoning tokens alone are an answer.
#[tokio::test]
async fn a_reasoning_only_probe_answer_is_an_answer() {
    let (_stub, port) = stub_engine(|r| r.reasoning_only.store(true, SeqCst)).await;
    let root = tempfile::tempdir().unwrap();
    launching(port, root.path(), ScriptedTool::new(Presence::Alive))
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
}

// T42 (SPEC §6.4): an engine that exits before readiness fails the launch
// with its summary, naming the option llama-server's parser refused.
#[tokio::test]
async fn an_engine_that_exits_reports_the_rejected_argument() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("engine.log");
    std::fs::write(&log, "error: invalid argument: --bogus-flag\n").unwrap();
    let port = free_port().await;
    let mut launch = plan(port);
    launch.engine_log = Some(log.to_string_lossy().into_owned());
    let error = adapter(port, root.path())
        .with_launch(launch)
        .with_tools(ScriptedTool::new(Presence::Gone))
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();
    let RuntimeError::LaunchFailed(text) = error else {
        panic!("a launch failure, not {error:?}");
    };
    assert!(text.contains("rejected argument --bogus-flag"), "{text}");
}

// T42 T37 (ADR 0029 §6): a launch on a root holding `etc/llama.cpp/config.ini`
// is refused `engine_config_file` before any process starts.
#[tokio::test]
async fn a_system_config_file_refuses_the_launch_before_any_process() {
    let (stub, port) = stub_engine(|_| {}).await;
    let root = tempfile::tempdir().unwrap();
    let file = capyctl_config::llamacpp::system_config_file(root.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "[server]\napi-key = x\n").unwrap();
    let tool = ScriptedTool::new(Presence::Alive);
    let error = launching(port, root.path(), tool.clone())
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();
    assert_eq!(error, RuntimeError::Refused(ENGINE_CONFIG_FILE.into()));
    assert!(error.to_string().contains("config.ini"), "{error}");
    assert!(tool.spawned.get().is_none(), "nothing started");
    assert_eq!(stub.health_reads.load(Ordering::SeqCst), 0);
}

// T42 (ADR 0029 §10, ADR 0023 §6): idle needs `requests_processing` and
// `requests_deferred` both 0; either one reporting work is busy; an engine
// that does not listen or answers 503 serves nothing; `/metrics` without the
// gauges proves nothing.
#[tokio::test]
async fn idle_before_signal_reads_the_metrics_gauges() {
    let (stub, port) = stub_engine(|_| {}).await;
    let root = tempfile::tempdir().unwrap();
    let engine = adapter(port, root.path());
    let one = member();
    let work = || engine.idle_before_signal(&one);
    assert_eq!(work().await, Some(EngineWork::Idle));
    let reported = &stub.reported;
    reported.processing.store(1, SeqCst);
    assert_eq!(work().await, Some(EngineWork::Busy));
    reported.processing.store(0, SeqCst);
    reported.deferred.store(2, SeqCst);
    assert_eq!(work().await, Some(EngineWork::Busy));
    assert!(!engine.prepare_park(&member()).await.unwrap().quiescent);
    reported.deferred.store(0, SeqCst);
    reported.no_gauges.store(true, SeqCst);
    assert_eq!(work().await, Some(EngineWork::Unanswered));
    reported.no_gauges.store(false, SeqCst);
    reported.loading.store(1, SeqCst);
    assert_eq!(work().await, Some(EngineWork::NotListening));
    let gone = adapter(free_port().await, root.path());
    assert_eq!(
        gone.idle_before_signal(&member()).await,
        Some(EngineWork::NotListening)
    );
}

// T42 (ADR 0029 §10, ADR 0023 §6): the wait before a stop signal: a hung
// `/metrics` is signalled once the bound passes; an engine still busy at the
// bound is not, and the cleanup stays uncertain.
#[tokio::test]
async fn the_wait_signals_a_hung_engine_at_the_bound_and_never_a_busy_one() {
    use capyctl_adapters::tensorfold::wait_idle;
    let root = tempfile::tempdir().unwrap();
    let (_hung, hung_port) = stub_engine(|r| r.hung_metrics.store(true, SeqCst)).await;
    let started = Instant::now();
    assert!(
        wait_idle(
            &adapter(hung_port, root.path()),
            &member(),
            Duration::from_millis(500),
            std::future::pending()
        )
        .await
    );
    assert!(started.elapsed() >= Duration::from_millis(500));
    let (_busy, busy_port) = stub_engine(|r| r.processing.store(1, SeqCst)).await;
    assert!(
        !wait_idle(
            &adapter(busy_port, root.path()),
            &member(),
            Duration::from_millis(500),
            std::future::pending()
        )
        .await
    );
    let (_idle, idle_port) = stub_engine(|_| {}).await;
    let started = Instant::now();
    assert!(
        wait_idle(
            &adapter(idle_port, root.path()),
            &member(),
            Duration::from_secs(20),
            std::future::pending()
        )
        .await
    );
    assert!(started.elapsed() < Duration::from_secs(5), "idle at once");
}

// T42 T21 (ADR 0029 §10): restart-only; no park, restore or reload path.
#[tokio::test]
async fn there_is_no_park_path() {
    let root = tempfile::tempdir().unwrap();
    let engine = adapter(1, root.path());
    assert!(matches!(
        engine.park(&member(), ParkLevel::Two).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    assert!(matches!(
        engine.restore(&member()).await,
        Err(AdapterError::UnsupportedCapability)
    ));
    assert!(matches!(
        engine.reload_weights(&member()).await,
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
            engine.execute_persisted(&command).await.unwrap_err(),
            RuntimeError::Unsupported
        );
    }
}

// T42 T37 (ADR 0029 §11, SPEC §10, review focus 6): a request with
// `cache_salt` is refused `cache_salt_unsupported`; the engine receives
// nothing.
#[tokio::test]
async fn cache_salt_is_refused_for_llamacpp() {
    let (stub, port) = stub_engine(|_| {}).await;
    let root = tempfile::tempdir().unwrap();
    let engine = adapter(port, root.path());
    let request = json!({"model": "public", "messages": [{"role": "user", "content": "hi"}],
        "cache_salt": "tenant-a"});
    assert_eq!(
        engine.forward_chat(&request).await.unwrap_err(),
        AdapterError::CacheSaltUnsupported
    );
    let mut sink = Collect::default();
    assert!(matches!(
        engine.forward_chat_stream_async(&request, &mut sink).await,
        Err(AdapterError::CacheSaltUnsupported)
    ));
    assert_eq!(stub.chats.load(Ordering::SeqCst), 0);
    // Without it the request reaches the engine under the served name.
    let mut plain = request.clone();
    plain.as_object_mut().unwrap().remove("cache_salt");
    let answer = engine.forward_chat(&plain).await.unwrap();
    assert_eq!(answer["choices"][0]["message"]["content"], "Ready.");
    assert_eq!(stub.chats.load(Ordering::SeqCst), 1);
}

/// A sink that keeps every chunk and counts progress; once `hang_up` is set
/// it fails delivery, as a client that left does.
#[derive(Default)]
struct Collect {
    chunks: Vec<String>,
    progressed: usize,
    hang_up: Option<Arc<AtomicBool>>,
}

#[async_trait::async_trait]
impl ChatSink for Collect {
    async fn send(&mut self, chunk: String) -> Result<(), DeliveryFailed> {
        if self.hang_up.as_ref().is_some_and(|gone| gone.load(SeqCst)) {
            return Err(DeliveryFailed);
        }
        self.chunks.push(chunk);
        Ok(())
    }
    fn progressed(&mut self) {
        self.progressed += 1;
    }
}

/// A stream that sends `pings` SSE comments, as llama-server's writer does
/// while nothing else is ready (`--sse-ping-interval`), then its reply.
async fn pinging_engine(pings: usize, pause: Duration) -> u16 {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            let events = futures::stream::iter(0..pings + 1).then(move |at| async move {
                tokio::time::sleep(pause).await;
                let text = if at < pings {
                    ": ping\n\n".to_owned()
                } else {
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        chunk(
                            "qwen",
                            json!({"role": "assistant", "content": "Hi"}),
                            Value::Null
                        ),
                        chunk("qwen", json!({}), json!("stop"))
                    )
                };
                Ok::<_, std::convert::Infallible>(text)
            });
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(events),
            )
        }),
    );
    serve(app).await
}

use futures::StreamExt;

// T42 (ADR 0029 §4, SPEC §10): an SSE `:` comment is not backend progress, so
// a stream that only pings before its first output is bounded by the request
// deadline, never cut by the idle bound measured from progress.
#[tokio::test]
async fn sse_pings_are_not_progress() {
    let port = pinging_engine(3, Duration::from_millis(50)).await;
    let root = tempfile::tempdir().unwrap();
    let engine = adapter(port, root.path());
    let mut sink = Collect::default();
    let ended = engine
        .forward_chat_stream_async(
            &json!({"model": "public", "messages": [{"role": "user", "content": "hi"}]}),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(ended, StreamEnded::Completed);
    assert_eq!(sink.chunks.len(), 2, "{:?}", sink.chunks);
    assert!(
        sink.chunks.iter().all(|chunk| !chunk.contains("ping")),
        "comments are not relayed"
    );
    // The two data events and the terminator; none of the three pings.
    assert_eq!(sink.progressed, 3);
}

/// An engine whose stream keeps running until the client hangs up; the
/// gauges count it as processing while its connection is open, as
/// llama-server cancels a task when its connection closes.
async fn cancellable_engine() -> (Arc<AtomicU64>, u16) {
    let processing = Arc::new(AtomicU64::new(0));
    struct Running(Arc<AtomicU64>);
    impl Drop for Running {
        fn drop(&mut self) {
            self.0.fetch_sub(1, SeqCst);
        }
    }
    let counted = processing.clone();
    let metrics_view = processing.clone();
    let app = axum::Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, SeqCst);
                    let running = Running(counted.clone());
                    let events = futures::stream::unfold(running, |running| async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let event = format!(
                            "data: {}\n\n",
                            chunk("qwen", json!({"content": "x"}), Value::Null)
                        );
                        Some((Ok::<_, std::convert::Infallible>(event), running))
                    });
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(events),
                    )
                }
            }),
        )
        .route(
            "/metrics",
            get(move || {
                let view = metrics_view.clone();
                async move {
                    let processing = view.load(SeqCst);
                    format!(
                        "llamacpp:requests_processing {processing}\nllamacpp:requests_deferred 0\n"
                    )
                }
            }),
        );
    (processing, serve(app).await)
}

// T42 T17 (SPEC §10 amended 2026-10-01, ADR 0029 §10): after the client hangs
// up the forwarder closes the engine connection; the request is not reported
// quiescent while the gauges still count it, and is once they read 0.
#[tokio::test]
async fn a_hang_up_is_quiescent_once_the_gauges_read_zero() {
    let (processing, port) = cancellable_engine().await;
    let root = tempfile::tempdir().unwrap();
    let engine = adapter(port, root.path());
    let hang_up = Arc::new(AtomicBool::new(false));
    let mut sink = Collect {
        hang_up: Some(hang_up.clone()),
        ..Collect::default()
    };
    let request = json!({"model": "public", "messages": [{"role": "user", "content": "hi"}]});
    // While the stream runs, the engine is busy; then the client hangs up.
    let watcher = async {
        while processing.load(SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!engine.engine_quiescent(&member(), now_ms()).await);
        hang_up.store(true, SeqCst);
    };
    let (ended, ()) = tokio::join!(
        engine.forward_chat_stream_async(&request, &mut sink),
        watcher
    );
    assert_eq!(ended.unwrap(), StreamEnded::Cancelled);
    let until = Instant::now() + Duration::from_secs(5);
    while !engine.engine_quiescent(&member(), now_ms()).await {
        assert!(
            Instant::now() < until,
            "the closed connection ends the task"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(processing.load(SeqCst), 0);
}
