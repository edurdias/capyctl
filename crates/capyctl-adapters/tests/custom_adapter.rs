//! ADR 0031 contract for the custom engine adapter (T44): readiness is the
//! port, the manifest's health path and then the API's own probe (chat or
//! the fixed decision), never the model list; the key on every request when
//! the manifest uses `{api_key}`; the idle signal as Prometheus gauges or
//! JSON fields, unknown on anything missing or malformed; an engine without
//! one never quiescent; the loopback check of the owned listeners; the
//! restart-only surface and the `cache_salt` refusal. Exercised against a
//! local axum engine, a scripted process tool and a fake `/proc`; nothing
//! here qualifies a native engine recipe (only the live rows CE1–CE4 do).

use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

use capyctl_adapters::custom::{CustomAdapter, PlanInputCustom};
use capyctl_adapters::traits::{
    AdapterError, ChatForward, EngineAdapter, EngineWork, MemberRef, OwnedProcessLaunch, ParkLevel,
    RenderedCommand, RuntimeAction, RuntimeCommand, RuntimeError, WorkObservation,
};
use capyctl_config::custom::{CustomManifest, PlaceholderValues};
use capyctl_domain::completion::{
    ExecutionIdentities, Milestone, Presence, ProcessIdentity, StepExecutionContext,
    TransitionToken,
};

const KEY: &str = "5f0c1e2d3c4b5a69788796a5b4c3d2e1f00112233445566778899aabbccddeeff";

/// A process tool that records what it was asked to spawn and answers from a
/// script. It never signals or inspects a real process.
struct ScriptedTool {
    present: Mutex<Presence>,
    spawned: OnceLock<RenderedCommand>,
    terminated: AtomicBool,
}

impl ScriptedTool {
    fn new(present: Presence) -> Arc<Self> {
        Arc::new(Self {
            present: Mutex::new(present),
            spawned: OnceLock::new(),
            terminated: AtomicBool::new(false),
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
        *self.present.lock().unwrap()
    }

    fn observe_group(&self, _api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        Ok(vec![api_identity()])
    }

    fn terminate_owned(
        &self,
        _identities: &[ProcessIdentity],
        _grace: Duration,
    ) -> Result<(), RuntimeError> {
        self.terminated.store(true, SeqCst);
        Ok(())
    }
}

const PID: u32 = 4242;

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

/// What the stub does; tests change it while the stub runs.
#[derive(Default)]
struct Script {
    /// Health reads answered 503 before 200.
    health_loading: AtomicUsize,
    /// Health always answers 500.
    health_broken: AtomicBool,
    /// Chat requests answered 503 before an answer.
    chat_loading: AtomicUsize,
    /// What `/v1/systemone` answers; 404 when `None`.
    decision: Mutex<Option<Value>>,
    /// What `/v1/embeddings` answers; 422 when `None`.
    embedding: Mutex<Option<Value>>,
    /// The idle route's status and body.
    idle: Mutex<Option<(StatusCode, String)>>,
    /// The bearer key every request must carry, when set.
    key: Option<String>,
}

#[derive(Clone)]
struct Stub {
    script: Arc<Script>,
    health_reads: Arc<AtomicUsize>,
    chats: Arc<AtomicUsize>,
    decisions: Arc<AtomicUsize>,
    embeddings: Arc<AtomicUsize>,
    models: Arc<AtomicUsize>,
    idle_reads: Arc<AtomicUsize>,
    unauthorized: Arc<AtomicUsize>,
}

impl Stub {
    fn refused(&self, headers: &HeaderMap) -> Option<axum::response::Response> {
        let key = self.script.key.as_ref()?;
        let sent = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        if sent == Some(format!("Bearer {key}").as_str()) {
            return None;
        }
        self.unauthorized.fetch_add(1, SeqCst);
        Some(StatusCode::UNAUTHORIZED.into_response())
    }
}

fn take(counter: &AtomicUsize) -> bool {
    counter
        .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

async fn health(State(stub): State<Stub>, headers: HeaderMap) -> axum::response::Response {
    stub.health_reads.fetch_add(1, SeqCst);
    if let Some(refused) = stub.refused(&headers) {
        return refused;
    }
    if stub.script.health_broken.load(SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if take(&stub.script.health_loading) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // basal's `/health` answers a constant.
    Json(json!({"status": "ok"})).into_response()
}

async fn models(State(stub): State<Stub>) -> axum::response::Response {
    stub.models.fetch_add(1, SeqCst);
    Json(json!({"models": [{"name": "basal"}]})).into_response()
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({"id": "c-1", "object": "chat.completion.chunk", "created": 1, "model": "basal",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

async fn chat(
    State(stub): State<Stub>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    stub.chats.fetch_add(1, SeqCst);
    if let Some(refused) = stub.refused(&headers) {
        return refused;
    }
    if take(&stub.script.chat_loading) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "loading"})),
        )
            .into_response();
    }
    assert_eq!(body["model"], "basal", "the probe names the served model");
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant"}), Value::Null),
        chunk(json!({"content": "Ready."}), Value::Null),
        chunk(json!({}), json!("stop")),
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

async fn systemone(
    State(stub): State<Stub>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    stub.decisions.fetch_add(1, SeqCst);
    if let Some(refused) = stub.refused(&headers) {
        return refused;
    }
    assert_eq!(body["model"], "basal", "the probe names the served model");
    match stub.script.decision.lock().unwrap().clone() {
        Some(answer) => Json(answer).into_response(),
        // basal answers every failure 422.
        None => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "model loading"})),
        )
            .into_response(),
    }
}

async fn embeddings(
    State(stub): State<Stub>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    stub.embeddings.fetch_add(1, SeqCst);
    if let Some(refused) = stub.refused(&headers) {
        return refused;
    }
    assert_eq!(
        body,
        capyctl_adapters::embedding_probe::request("basal"),
        "the fixed probe names the served model"
    );
    match stub.script.embedding.lock().unwrap().clone() {
        Some(answer) => Json(answer).into_response(),
        None => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "model loading"})),
        )
            .into_response(),
    }
}

async fn idle(State(stub): State<Stub>, headers: HeaderMap) -> axum::response::Response {
    stub.idle_reads.fetch_add(1, SeqCst);
    if let Some(refused) = stub.refused(&headers) {
        return refused;
    }
    match stub.script.idle.lock().unwrap().clone() {
        Some((status, body)) => (status, body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn stub_engine(script: Script) -> (Stub, u16) {
    let stub = Stub {
        script: Arc::new(script),
        health_reads: Arc::default(),
        chats: Arc::default(),
        decisions: Arc::default(),
        embeddings: Arc::default(),
        models: Arc::default(),
        idle_reads: Arc::default(),
        unauthorized: Arc::default(),
    };
    let app = axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/systemone", post(systemone))
        .route("/v1/embeddings", post(embeddings))
        .route("/metrics", get(idle))
        .route("/stats", get(idle))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (stub, port)
}

fn decision(choice: &str, billing: f64, shipping: f64, technical: f64) -> Value {
    json!({"model": "basal", "answers": {"route": {"type": "choice", "choice": choice,
        "probabilities": {"billing": billing, "shipping": shipping, "technical": technical}}}})
}

fn manifest(text: &str) -> CustomManifest {
    CustomManifest::parse(text).unwrap()
}

const CHAT: &str = "kind: custom_engine\nschema_version: 1\napi: chat\n\
    args: [--host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\", --name, \"{served_name}\"]\n\
    health: /health\n";

const DECISIONS: &str =
    "kind: custom_engine\nschema_version: 1\napi: decisions\nversion: \"2.2.0\"\n\
    args: [--mode, eager, --host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\"]\n\
    health: /health\n";

const EMBEDDINGS: &str =
    "kind: custom_engine\nschema_version: 1\napi: embeddings\nversion: \"0.3.0\"\n\
    args: [--host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\"]\n\
    env: {SERVE_AUTH: \"{api_key}\"}\n\
    health: /health\n";

const KEYED: &str = "kind: custom_engine\nschema_version: 1\napi: chat\n\
    args: [--host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\"]\n\
    env: {SERVE_AUTH: \"{api_key}\"}\n\
    idle: {url: /metrics, gauges: [requests_running, requests_waiting]}\n";

/// A fake `/proc` in which the API process holds one listening socket on
/// `address` (an IPv4 address and the port).
fn fake_proc(address: [u8; 4], port: u16) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let process = root.path().join(PID.to_string());
    std::fs::create_dir_all(process.join("fd")).unwrap();
    std::fs::create_dir_all(process.join("net")).unwrap();
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
    root
}

fn adapter(port: u16, manifest: &CustomManifest, key: Option<&str>) -> CustomAdapter {
    CustomAdapter::new(
        format!("http://127.0.0.1:{port}").parse().unwrap(),
        manifest.build_fingerprint(),
        "basal".into(),
        key.map(str::to_owned),
    )
    .with_api(manifest.api)
    .with_health(manifest.health.clone())
    .with_idle(manifest.idle.clone())
}

fn plan(port: u16, manifest: &CustomManifest, key: Option<&str>) -> PlanInputCustom {
    PlanInputCustom {
        executable: "/home/u/basal-venv/bin/basal-serve".into(),
        manifest: manifest.clone(),
        values: PlaceholderValues {
            port,
            model_dir: "/srv/models/basal-omni".into(),
            served_name: "basal".into(),
            context_length: Some(4096),
            api_key: key.map(str::to_owned),
        },
        engine_env: capyctl_config::engine_env::ResolvedEnv {
            vars: manifest
                .env
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        (
                            value.clone(),
                            capyctl_config::engine_env::EnvSource::Manifest,
                        ),
                    )
                })
                .collect(),
        },
        engine_log: None,
        cuda_namespace: None,
    }
}

fn launching(
    port: u16,
    manifest: &CustomManifest,
    key: Option<&str>,
    tool: Arc<ScriptedTool>,
    proc_root: &std::path::Path,
) -> CustomAdapter {
    adapter(port, manifest, key)
        .with_launch(plan(port, manifest, key))
        .with_tools(tool)
        .with_proc_root(proc_root.to_path_buf())
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
            launch_settings: Some(capyctl_testkit::custom_launch_settings()),
        },
    }
}

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d-1".into(),
        member_id: "b-1".into(),
    }
}

// T44 (ADR 0031 §5, SPEC §6.1): a chat custom engine is ready only once the
// port accepts, the health path answers 200 and the chat probe is answered;
// a probe answered 503 keeps the launch waiting; the model list is never
// read; the command is the manifest's, one argument per entry.
#[tokio::test]
async fn a_chat_engine_is_ready_only_after_its_chat_probe() {
    let (stub, port) = stub_engine(Script {
        health_loading: AtomicUsize::new(2),
        chat_loading: AtomicUsize::new(2),
        ..Script::default()
    })
    .await;
    let proc_root = fake_proc([127, 0, 0, 1], port);
    let manifest = manifest(CHAT);
    let tool = ScriptedTool::new(Presence::Alive);
    let observation = launching(port, &manifest, None, tool.clone(), proc_root.path())
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert!(
        stub.health_reads.load(SeqCst) >= 3,
        "a 503 health is not ready"
    );
    assert_eq!(stub.chats.load(SeqCst), 3, "probed until answered");
    assert_eq!(stub.decisions.load(SeqCst), 0);
    assert_eq!(stub.models.load(SeqCst), 0, "/v1/models is never read");
    assert_eq!(observation.identities, vec![api_identity()]);
    assert!(observation.facts.contains(&Milestone::ModelUsable));
    assert!(observation.receipt.contains("listeners on loopback"));
    let spawned = tool.spawned.get().expect("one spawn");
    assert_eq!(
        spawned.argv,
        vec![
            "/home/u/basal-venv/bin/basal-serve",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--model",
            "/srv/models/basal-omni",
            "--name",
            "basal",
        ]
    );
    assert_eq!(spawned.env["HF_HUB_OFFLINE"], "1");
    assert_eq!(spawned.env["TRANSFORMERS_OFFLINE"], "1");
    // T21 (owner decision 2026-10-22): `engine_telemetry` is `off` unless a
    // role says otherwise.
    assert_eq!(spawned.env["DO_NOT_TRACK"], "1");
    assert!(spawned.env["PATH"].starts_with("/home/u/basal-venv/bin:"));
    assert!(!spawned
        .env
        .contains_key(capyctl_adapters::engine_log::OWNED_VALUES_VARIABLE));
}

// T44 T43 (ADR 0031 §5, ADR 0030 §3): a decisions custom engine is ready
// only on a well-formed answer to the fixed decision, never a chat probe;
// its wake check is that decision.
#[tokio::test]
async fn a_decisions_engine_is_ready_only_after_the_fixed_decision() {
    use capyctl_adapters::completion_probe::CanaryEvidence;
    let (stub, port) = stub_engine(Script::default()).await;
    let proc_root = fake_proc([127, 0, 0, 1], port);
    let manifest = manifest(DECISIONS);
    let engine = launching(
        port,
        &manifest,
        None,
        ScriptedTool::new(Presence::Alive),
        proc_root.path(),
    );
    let started = tokio::spawn({
        let command = initialize_command(30_000);
        async move { engine.execute_persisted(&command).await.map(|_| engine) }
    });
    // basal answers 422 until it can decide; the launch keeps waiting.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!started.is_finished(), "a 422 decision is not readiness");
    assert!(stub.decisions.load(SeqCst) >= 2);
    *stub.script.decision.lock().unwrap() = Some(decision("billing", 0.95, 0.02, 0.03));
    let engine = started.await.unwrap().unwrap();
    assert_eq!(stub.chats.load(SeqCst), 0, "never a chat probe");
    assert_eq!(stub.models.load(SeqCst), 0, "/v1/models is never read");
    let canary = engine
        .wake_canary(
            &initialize_command(30_000).context,
            8,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(&canary.answer, CanaryEvidence::Decision(d) if d.choice == "billing"));
}

// T47 T44 (ADR 0036 §1, §5, ADR 0031 §5): an embeddings custom engine is
// ready only on a well-formed answer to the fixed embedding, with its key,
// never a chat or decision probe; its wake check is that embedding.
#[tokio::test]
async fn an_embeddings_engine_is_ready_only_after_the_fixed_embedding() {
    use capyctl_adapters::completion_probe::CanaryEvidence;
    let entry = |vector: Value| json!({"object": "embedding", "index": 0, "embedding": vector});
    let answer =
        |vector: Value| json!({"object": "list", "model": "basal", "data": [entry(vector)]});
    let script = Script {
        key: Some("k3y".into()),
        // A zero vector is no answer: the launch keeps waiting.
        embedding: Mutex::new(Some(answer(json!([0.0, 0.0])))),
        ..Script::default()
    };
    let (stub, port) = stub_engine(script).await;
    let proc_root = fake_proc([127, 0, 0, 1], port);
    let manifest = manifest(EMBEDDINGS);
    let engine = launching(
        port,
        &manifest,
        Some("k3y"),
        ScriptedTool::new(Presence::Alive),
        proc_root.path(),
    );
    let started = tokio::spawn({
        let command = initialize_command(30_000);
        async move { engine.execute_persisted(&command).await.map(|_| engine) }
    });
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!started.is_finished(), "a zero vector is not readiness");
    assert!(stub.embeddings.load(SeqCst) >= 2);
    *stub.script.embedding.lock().unwrap() = Some(answer(json!([0.6, 0.8])));
    let engine = started.await.unwrap().unwrap();
    assert_eq!(stub.chats.load(SeqCst), 0, "never a chat probe");
    assert_eq!(stub.decisions.load(SeqCst), 0, "never a decision probe");
    assert_eq!(stub.unauthorized.load(SeqCst), 0, "the key on every probe");
    let canary = engine
        .wake_canary(
            &initialize_command(30_000).context,
            8,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(&canary.answer, CanaryEvidence::Embedding(e) if e.vector == [0.6, 0.8]));
}

// T44 (ADR 0031 §5): a health path that never answers 200 keeps the launch
// waiting; nothing is probed, and the deadline ends it uncertain.
#[tokio::test]
async fn a_health_path_that_is_not_200_keeps_waiting() {
    let script = Script::default();
    script.health_broken.store(true, SeqCst);
    let (stub, port) = stub_engine(script).await;
    let proc_root = fake_proc([127, 0, 0, 1], port);
    let manifest = manifest(CHAT);
    let error = launching(
        port,
        &manifest,
        None,
        ScriptedTool::new(Presence::Alive),
        proc_root.path(),
    )
    .execute_persisted(&initialize_command(3_500))
    .await
    .unwrap_err();
    assert!(
        matches!(&error, RuntimeError::Uncertain(text) if text.contains("deadline")),
        "{error}"
    );
    assert!(stub.health_reads.load(SeqCst) >= 2);
    assert_eq!(stub.chats.load(SeqCst), 0);
    assert_eq!(stub.models.load(SeqCst), 0);
}

// T44 (ADR 0031 §7): with `{api_key}` the launch's key is rendered where the
// manifest puts it, owned by the log writer by value, and sent as a bearer
// header on the health read, the probe, forwarded requests and idle reads.
#[tokio::test]
async fn the_key_is_rendered_and_sent_on_every_request() {
    let (stub, port) = stub_engine(Script {
        key: Some(KEY.into()),
        idle: Mutex::new(Some((
            StatusCode::OK,
            "requests_running 0\nrequests_waiting 0\n".into(),
        ))),
        ..Script::default()
    })
    .await;
    let proc_root = fake_proc([127, 0, 0, 1], port);
    let manifest = manifest(KEYED);
    let tool = ScriptedTool::new(Presence::Alive);
    let engine = launching(port, &manifest, Some(KEY), tool.clone(), proc_root.path());
    engine
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    let spawned = tool.spawned.get().unwrap();
    assert_eq!(spawned.env["SERVE_AUTH"], KEY);
    assert_eq!(
        capyctl_adapters::engine_log::decode_owned_values(
            &spawned.env[capyctl_adapters::engine_log::OWNED_VALUES_VARIABLE]
        ),
        vec![KEY]
    );
    assert!(!format!("{spawned:?}").contains(KEY));
    let answer = engine
        .forward_chat(&json!({"model": "basal", "messages": [{"role": "user", "content": "hi"}]}))
        .await
        .unwrap();
    assert_eq!(answer["choices"][0]["message"]["content"], "Ready.");
    assert_eq!(
        engine.idle_before_signal(&member()).await,
        Some(EngineWork::Idle)
    );
    assert_eq!(stub.unauthorized.load(SeqCst), 0, "every request was keyed");
    // The same engine refuses an unkeyed reader: its idle reading is unknown.
    let unkeyed = adapter(port, &manifest, None);
    assert_eq!(
        unkeyed.idle_before_signal(&member()).await,
        Some(EngineWork::Unanswered)
    );
    assert!(!unkeyed.engine_quiescent(&member(), 0).await);
}

// T44 (ADR 0031 §6): the idle signal: gauges summed over labels, JSON
// fields; missing, malformed, non-200 and unreachable are unknown, never
// idle; a busy reading is busy; nothing listening is not listening.
#[tokio::test]
async fn the_idle_signal_reads_idle_only_on_zero() {
    let gauges = manifest(
        "kind: custom_engine\nschema_version: 1\napi: chat\n\
         args: [\"{host}\", \"{port}\", \"{model_dir}\"]\n\
         idle: {url: /metrics, gauges: [requests_running, requests_waiting]}\n",
    );
    let fields = manifest(
        "kind: custom_engine\nschema_version: 1\napi: chat\n\
         args: [\"{host}\", \"{port}\", \"{model_dir}\"]\n\
         idle: {url: /stats, json: [queue.running, busy]}\n",
    );
    let (stub, port) = stub_engine(Script::default()).await;
    let cases: [(&CustomManifest, StatusCode, &str, EngineWork); 9] = [
        (
            &gauges,
            StatusCode::OK,
            "requests_running{slot=\"0\"} 0\nrequests_running{slot=\"1\"} 0\nrequests_waiting 0\n",
            EngineWork::Idle,
        ),
        (
            &gauges,
            StatusCode::OK,
            "requests_running{slot=\"0\"} 0\nrequests_running{slot=\"1\"} 1\nrequests_waiting 0\n",
            EngineWork::Busy,
        ),
        (
            &gauges,
            StatusCode::OK,
            "requests_running 0\n",
            EngineWork::Unanswered,
        ),
        (
            &gauges,
            StatusCode::OK,
            "requests_running 0\nrequests_waiting NaN\n",
            EngineWork::Unanswered,
        ),
        (
            &gauges,
            StatusCode::SERVICE_UNAVAILABLE,
            "requests_running 0\nrequests_waiting 0\n",
            EngineWork::Unanswered,
        ),
        (
            &fields,
            StatusCode::OK,
            r#"{"queue": {"running": 0}, "busy": false}"#,
            EngineWork::Idle,
        ),
        (
            &fields,
            StatusCode::OK,
            r#"{"queue": {"running": 0}, "busy": true}"#,
            EngineWork::Busy,
        ),
        (
            &fields,
            StatusCode::OK,
            r#"{"queue": {}}"#,
            EngineWork::Unanswered,
        ),
        (&fields, StatusCode::OK, "<html>", EngineWork::Unanswered),
    ];
    for (manifest, status, body, expected) in cases {
        *stub.script.idle.lock().unwrap() = Some((status, body.into()));
        let engine = adapter(port, manifest, None);
        assert_eq!(
            engine.idle_before_signal(&member()).await,
            Some(expected),
            "{status} {body}"
        );
        assert_eq!(
            engine.engine_quiescent(&member(), 0).await,
            expected == EngineWork::Idle,
            "{status} {body}"
        );
        assert_eq!(
            engine.observe_work(&member()).await.unwrap() == WorkObservation::Idle,
            expected == EngineWork::Idle
        );
    }
    // Nothing listening: the engine serves nothing, so the stop may proceed,
    // but that is not a quiescence acknowledgement.
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let gone = adapter(closed, &gauges, None);
    assert_eq!(
        gone.idle_before_signal(&member()).await,
        Some(EngineWork::NotListening)
    );
    assert!(!gone.engine_quiescent(&member(), 0).await);
}

// T44 (ADR 0031 §6): without an idle signal there is no account of the
// engine's work: the stop proceeds on CapyCTL's drain, the work is unknown
// and the engine is never quiescent while it runs.
#[tokio::test]
async fn an_engine_without_an_idle_signal_is_never_quiescent() {
    let (stub, port) = stub_engine(Script::default()).await;
    let engine = adapter(port, &manifest(DECISIONS), None);
    assert_eq!(engine.idle_before_signal(&member()).await, None);
    assert!(!engine.engine_quiescent(&member(), 0).await);
    assert_eq!(
        engine.observe_work(&member()).await.unwrap(),
        WorkObservation::Unknown
    );
    assert!(
        capyctl_adapters::tensorfold::wait_idle(
            &engine,
            &member(),
            Duration::from_secs(5),
            std::future::pending()
        )
        .await,
        "the stop proceeds at once"
    );
    assert_eq!(stub.idle_reads.load(SeqCst), 0);
}

// T44 (ADR 0031 §7, §10): an owned process listening off loopback fails the
// launch `custom_listener_exposed`, naming the address and port, after the
// probe passed, and the launch's processes are stopped.
#[tokio::test]
async fn an_exposed_listener_fails_the_launch_and_stops_it() {
    let (_stub, port) = stub_engine(Script::default()).await;
    let proc_root = fake_proc([0, 0, 0, 0], port);
    let tool = ScriptedTool::new(Presence::Alive);
    let error = launching(port, &manifest(CHAT), None, tool.clone(), proc_root.path())
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap_err();
    let RuntimeError::LaunchFailed(text) = &error else {
        panic!("{error}");
    };
    assert!(
        text.starts_with(&format!(
            "custom_listener_exposed: an owned process listens on 0.0.0.0:{port}"
        )),
        "{text}"
    );
    assert!(tool.terminated.load(SeqCst), "the launch was stopped");
}

// T44 T13 (ADR 0031 §4, SPEC §8.1): a command whose own process exits (one
// that daemonizes and returns) fails the launch, whatever answers the port.
#[tokio::test]
async fn an_engine_that_exits_fails_the_launch() {
    let (_stub, port) = stub_engine(Script::default()).await;
    let proc_root = fake_proc([127, 0, 0, 1], port);
    let error = launching(
        port,
        &manifest(CHAT),
        None,
        ScriptedTool::new(Presence::Gone),
        proc_root.path(),
    )
    .execute_persisted(&initialize_command(30_000))
    .await
    .unwrap_err();
    assert!(
        matches!(&error, RuntimeError::LaunchFailed(text) if text.contains("exited before readiness")),
        "{error}"
    );
}

// T44 (ADR 0031 §4): restart-only: no park, restore or reload, and every
// step but Initialize is unsupported.
#[tokio::test]
async fn a_custom_engine_never_parks() {
    let (_stub, port) = stub_engine(Script::default()).await;
    let engine = adapter(port, &manifest(CHAT), None);
    assert_eq!(
        engine.park(&member(), ParkLevel::One).await.unwrap_err(),
        AdapterError::UnsupportedCapability
    );
    assert_eq!(
        engine.restore(&member()).await.unwrap_err(),
        AdapterError::UnsupportedCapability
    );
    assert_eq!(
        engine.reload_weights(&member()).await.unwrap_err(),
        AdapterError::UnsupportedCapability
    );
    let mut park = initialize_command(30_000);
    park.action = RuntimeAction::Park;
    assert!(matches!(
        engine.execute_persisted(&park).await,
        Err(RuntimeError::Unsupported)
    ));
    // A chat custom engine has no canary; its check after a restart is the
    // startup chat probe.
    assert!(matches!(
        engine
            .wake_canary(&park.context, 8, Duration::from_secs(1))
            .await,
        Err(RuntimeError::Unsupported)
    ));
}

// T44 T37 (ADR 0031 §4, SPEC §10): `cache_salt` is refused; the engine
// receives nothing. An engine's 4xx is a rejection.
#[tokio::test]
async fn cache_salt_is_refused_and_a_4xx_is_rejected() {
    let (stub, port) = stub_engine(Script::default()).await;
    let engine = adapter(port, &manifest(DECISIONS), None);
    let request = json!({"model": "public", "messages": [{"role": "user", "content": "hi"}],
        "cache_salt": "tenant-a"});
    assert_eq!(
        engine.forward_chat(&request).await.unwrap_err(),
        AdapterError::CacheSaltUnsupported
    );
    assert_eq!(stub.chats.load(SeqCst), 0);
    // basal answers 422 for every failure: relayed as a rejection.
    let refused = engine
        .forward_decision(&capyctl_adapters::decision_probe::request("public"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, AdapterError::Rejected { status: 422, .. }),
        "{refused:?}"
    );
}
