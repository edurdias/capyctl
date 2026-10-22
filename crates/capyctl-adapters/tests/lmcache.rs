//! ADR 0033: the LMCache server of a vLLM or SGLang launch, CapyCTL's side.
//!
//! The server's command, the two connector renderings, the start (a fake
//! `/status` server, a scripted process tool and a fake `/proc` root for the
//! listener check), and the vLLM Initialize around it, against a local stub
//! engine. Nothing here runs LMCache or an engine: it proves CapyCTL's
//! sequence, renderings and reports, not that LMCache works (the LC rows do).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

use capyctl_adapters::lmcache::{
    render_server, start_server, vllm_kv_transfer_config, write_sglang_config, LmcacheLaunch,
    LmcacheReport, LmcacheState, ServerPlan,
};
use capyctl_adapters::traits::{
    EngineAdapter, OwnedProcessLaunch, RenderedCommand, RuntimeAction, RuntimeCommand, RuntimeError,
};
use capyctl_adapters::vllm::{render_command, PlanInputVllm, VllmAdapter, USER_ARGS_MARKER};
use capyctl_adapters::ParkPolicy;
use capyctl_config::lmcache::LmcacheOffReason;
use capyctl_domain::completion::{
    ExecutionIdentities, Milestone, Presence, ProcessIdentity, StepExecutionContext,
    TransitionToken,
};

// ------------------------------------------------------------------- fixtures

fn identity(role: &str, pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: role.into(),
        pid,
        boot_id: "boot".into(),
        start_ticks: u64::from(pid) * 10,
    }
}

fn server_plan(http_port: u16, proc_root: &Path, required: bool) -> ServerPlan {
    ServerPlan {
        lmcache_bin: "/opt/venv/bin/lmcache".into(),
        rpc_port: 25001,
        http_port,
        pool_gib: 4,
        path: "/opt/venv/bin:/usr/bin:/bin".into(),
        home: Some("/var/lib/capyctl".into()),
        cuda_visible_devices: Some("GPU-1".into()),
        cuda_device_order: None,
        log: None,
        config_dir: proc_root.join("lmcache-dir"),
        start_bound: Duration::from_secs(5),
        proc_root: proc_root.to_path_buf(),
        required,
    }
}

/// A `/proc` root where each `(pid, inode)` holds one socket, and every
/// process's `net/tcp` lists a loopback listener (inode 1111) and one on all
/// addresses (inode 2222).
fn fake_proc(sockets: &[(u32, u64)]) -> tempfile::TempDir {
    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:61A9 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1111 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:61AA 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2222 1 0000000000000000 100 0 0 10 0\n";
    let root = tempfile::tempdir().unwrap();
    for (pid, inode) in sockets {
        let process = root.path().join(pid.to_string());
        std::fs::create_dir_all(process.join("fd")).unwrap();
        std::fs::create_dir_all(process.join("net")).unwrap();
        std::os::unix::fs::symlink(format!("socket:[{inode}]"), process.join("fd").join("3"))
            .unwrap();
        std::fs::write(process.join("net").join("tcp"), TCP).unwrap();
    }
    root
}

/// A loopback `/status` that answers 200.
async fn status_server() -> u16 {
    let app = axum::Router::new().route("/status", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

/// A port nobody listens on.
async fn closed_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// A stub vLLM: the model listed, a chat probe answered.
async fn stub_engine() -> u16 {
    async fn models() -> Json<Value> {
        Json(json!({"object": "list", "data": [{"id": "gate-m", "object": "model"}]}))
    }
    async fn chat(Json(body): Json<Value>) -> axum::response::Response {
        let model = body["model"].as_str().unwrap_or_default().to_owned();
        let chunks = [
            json!({"id": "p", "object": "chat.completion.chunk", "created": 1, "model": model,
                   "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ready"},
                                "finish_reason": Value::Null}]}),
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
    let app = axum::Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .fallback(|| async { StatusCode::NOT_FOUND });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

/// A process tool answering from a script; it never touches a real process.
struct Tool {
    server: ProcessIdentity,
    server_group: Vec<ProcessIdentity>,
    engine_group: Vec<ProcessIdentity>,
    server_presence: Mutex<Presence>,
    engine_presence: Mutex<Presence>,
    /// The server exits as soon as the engine is spawned.
    server_dies_with_engine: bool,
    /// The engine exits as soon as it is spawned.
    engine_dies: bool,
    spawned: Mutex<Vec<(String, RenderedCommand)>>,
    stopped: Mutex<Vec<Vec<ProcessIdentity>>>,
}

impl Tool {
    fn new() -> Self {
        let server = identity("lmcache", 3000);
        Self {
            server_group: vec![server.clone(), identity("lmcache-0", 3001)],
            server,
            engine_group: vec![identity("api", 4000), identity("worker-0", 4001)],
            server_presence: Mutex::new(Presence::Alive),
            engine_presence: Mutex::new(Presence::Alive),
            server_dies_with_engine: false,
            engine_dies: false,
            spawned: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
        }
    }

    fn spawned_kinds(&self) -> Vec<String> {
        self.spawned
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, _)| kind.clone())
            .collect()
    }

    fn engine_command(&self) -> RenderedCommand {
        self.spawned
            .lock()
            .unwrap()
            .iter()
            .find(|(kind, _)| kind != "lmcache")
            .map(|(_, cmd)| cmd.clone())
            .unwrap()
    }

    fn engine_spawned(&self) {
        if self.engine_dies {
            *self.engine_presence.lock().unwrap() = Presence::Gone;
        }
        if self.server_dies_with_engine {
            *self.server_presence.lock().unwrap() = Presence::Gone;
        }
    }
}

impl OwnedProcessLaunch for Tool {
    fn spawn_durable(
        &self,
        _: &str,
        cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawned
            .lock()
            .unwrap()
            .push(("engine".into(), cmd.clone()));
        self.engine_spawned();
        Ok(self.engine_group[0].clone())
    }
    fn spawn_lmcache(
        &self,
        _: &str,
        cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawned
            .lock()
            .unwrap()
            .push(("lmcache".into(), cmd.clone()));
        Ok(self.server.clone())
    }
    fn spawn_durable_wake(
        &self,
        _: &str,
        wake: &str,
        cmd: &RenderedCommand,
        _: Option<&capyctl_adapters::ProtectedLaunchDescriptors>,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawned
            .lock()
            .unwrap()
            .push((format!("wake:{wake}"), cmd.clone()));
        Ok(self.engine_group[0].clone())
    }
    fn present(&self, identity: &ProcessIdentity) -> Presence {
        if identity.is_lmcache() {
            *self.server_presence.lock().unwrap()
        } else {
            *self.engine_presence.lock().unwrap()
        }
    }
    fn observe_group(&self, _: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        Ok(self.engine_group.clone())
    }
    fn observe_lmcache_group(
        &self,
        _: &ProcessIdentity,
    ) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        Ok(self.server_group.clone())
    }
    fn stop_group(&self, identities: &[ProcessIdentity], _: Duration) -> Result<(), RuntimeError> {
        self.stopped.lock().unwrap().push(identities.to_vec());
        Ok(())
    }
    fn terminate_owned(&self, _: &[ProcessIdentity], _: Duration) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn context(
    identities: ExecutionIdentities,
    step: &str,
    deadline_in_ms: i64,
) -> StepExecutionContext {
    let owned = matches!(identities, ExecutionIdentities::OwnedLaunch);
    StepExecutionContext {
        token: TransitionToken {
            deployment_id: "d-1".into(),
            revision: 1,
            generation: 1,
            operation_id: "o-1".into(),
            step_id: step.into(),
        },
        binding_id: "b-1".into(),
        incarnation: "i-1".into(),
        issued_at_ms: now_ms(),
        deadline_ms: now_ms() + deadline_in_ms,
        identities,
        completion_target: None,
        grant_id: Some("g-1".into()),
        launch_settings: owned.then(capyctl_testkit::vllm_launch_settings),
    }
}

fn initialize(deadline_in_ms: i64) -> RuntimeCommand {
    RuntimeCommand {
        action: RuntimeAction::Initialize,
        context: context(ExecutionIdentities::OwnedLaunch, "s-1", deadline_in_ms),
    }
}

fn vllm_plan(port: u16, state: Option<Arc<LmcacheState>>, log: Option<&Path>) -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/venv/bin/vllm".into(),
        model_path: "/srv/models/gate-m".into(),
        port,
        served_model_name: "gate-m".into(),
        tensor_parallel_size: 1,
        pipeline_parallel_size: 1,
        engine_path_extra: Some("/opt/venv/bin".into()),
        runtime_dir: Some("/opt/capyctl/runtime".into()),
        engine_log: log.map(|log| log.to_string_lossy().into_owned()),
        lmcache: state,
        ..PlanInputVllm::default()
    }
}

fn vllm_adapter(port: u16, plan: PlanInputVllm, tool: Arc<Tool>) -> VllmAdapter {
    VllmAdapter::new(
        format!("http://127.0.0.1:{port}").parse().unwrap(),
        Some("k3y".into()),
        "fp".into(),
        ParkPolicy::Disabled,
        "gate-m".into(),
    )
    .with_residency(capyctl_config::effective::Residency::RestartOnly)
    .with_launch(plan)
    .with_tools(tool)
    .with_engine_key("k3y".into())
}

// ---------------------------------------------------------- the server command

// T45 T37 (ADR 0033 §3): the server's argument vector, exactly; LMCache 0.5.6
// spells these options so (`multiprocess/config.py`: --host, --port,
// --transport; --http-host, --http-port; `distributed/config.py`:
// --l1-size-gb, --eviction-policy, --l1-use-lazy as a BooleanOptionalAction
// that defaults to lazy, hence the negation).
#[test]
fn the_server_argv_is_exactly_capyctls() {
    let root = tempfile::tempdir().unwrap();
    let command = render_server(&server_plan(25002, root.path(), false));
    assert_eq!(
        command.argv,
        [
            "/opt/venv/bin/lmcache",
            "server",
            "--host",
            "127.0.0.1",
            "--port",
            "25001",
            "--transport",
            "zmq",
            "--http-host",
            "127.0.0.1",
            "--http-port",
            "25002",
            "--l1-size-gb",
            "4",
            "--eviction-policy",
            "LRU",
            "--no-l1-use-lazy",
        ]
    );
    // T37: no script API, no lazy pool, no tier beyond host RAM.
    for refused in [
        "--run-script-api-enabled",
        "--l1-use-lazy",
        "--shm-name",
        "--gds-l1-path",
        "--l1-devdax-path",
        "--l2-adapter",
        "--enable-p2p",
    ] {
        assert!(!command.argv.iter().any(|a| a == refused), "{refused}");
    }
    assert!(!command
        .argv
        .iter()
        .any(|a| a.contains("l2") || a.contains("p2p") || a.contains("gds") || a.contains("disk")));
}

// T45 T21 T37 (ADR 0033 §3): the server's environment is exactly PATH, HOME
// and the instance's CUDA_VISIBLE_DEVICES; only the launcher's log name rides
// beside them, and the launcher never passes it on (capyctl-launchers test
// `the_server_starts_with_exactly_its_three_variables_and_its_own_log`).
#[test]
fn the_server_env_is_exactly_three_names() {
    let root = tempfile::tempdir().unwrap();
    let mut plan = server_plan(25002, root.path(), false);
    let names = |plan: &ServerPlan| render_server(plan).env.keys().cloned().collect::<Vec<_>>();
    assert_eq!(names(&plan), ["CUDA_VISIBLE_DEVICES", "HOME", "PATH"]);
    assert_eq!(
        render_server(&plan).env["PATH"],
        "/opt/venv/bin:/usr/bin:/bin"
    );
    plan.home = None;
    plan.cuda_visible_devices = None;
    assert_eq!(names(&plan), ["PATH"]);
    plan.log = Some("/var/log/capyctl/d/i-1.lmcache.log".into());
    assert_eq!(names(&plan), ["CAPYCTL_ENGINE_LOG", "PATH"]);
}

// ----------------------------------------------------------------- connectors

// T45 (ADR 0033 §4): vLLM's connector, exactly: LMCache's MP connector by
// module path, both roles, the server's loopback request port, no autostart.
// LMCache 0.5.6 `lmcache_mp_connector.py` reads `lmcache.mp.host`,
// `lmcache.mp.port` (and adds `tcp://`) and `mp_server_launcher.py` reads
// `lmcache.mp.autostart`.
#[test]
fn the_vllm_connector_is_the_adrs_json() {
    let rendered = vllm_kv_transfer_config(25001);
    assert_eq!(
        rendered,
        "{\"kv_connector\":\"LMCacheMPConnector\",\
         \"kv_connector_module_path\":\"lmcache.integration.vllm.lmcache_mp_connector\",\
         \"kv_role\":\"kv_both\",\
         \"kv_connector_extra_config\":{\"lmcache.mp.host\":\"127.0.0.1\",\
         \"lmcache.mp.port\":25001,\"lmcache.mp.autostart\":false}}"
    );
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        parsed,
        json!({
            "kv_connector": "LMCacheMPConnector",
            "kv_connector_module_path": "lmcache.integration.vllm.lmcache_mp_connector",
            "kv_role": "kv_both",
            "kv_connector_extra_config": {
                "lmcache.mp.host": "127.0.0.1",
                "lmcache.mp.port": 25001,
                "lmcache.mp.autostart": false
            }
        })
    );
}

// T45 T14 (ADR 0033 §§4, 11): the owned connector renders in the reserved block,
// before the user-arguments marker, beside extras that validation accepts.
#[test]
fn the_vllm_connector_renders_before_the_marker() {
    let mut plan = vllm_plan(8100, None, None);
    plan.extra_args = vec!["--max-num-seqs".into(), "4".into()];
    plan.kv_transfer_config = Some(vllm_kv_transfer_config(25001));
    let argv = render_command(&plan).unwrap().argv;
    let flag = argv
        .iter()
        .position(|a| a == "--kv-transfer-config")
        .unwrap();
    let marker = argv.iter().position(|a| a == USER_ARGS_MARKER).unwrap();
    assert!(flag < marker, "{argv:?}");
    assert_eq!(argv[flag + 1], vllm_kv_transfer_config(25001));
    assert_eq!(
        argv.iter().filter(|a| *a == "--kv-transfer-config").count(),
        1
    );
    // Without a server, nothing is rendered.
    plan.kv_transfer_config = None;
    assert!(!render_command(&plan)
        .unwrap()
        .argv
        .iter()
        .any(|a| a == "--kv-transfer-config"));
}

// T45 T37 (ADR 0033 §4): SGLang's connector file holds exactly two keys, is
// 0600 inside a 0700 directory, and is never written through a symlink.
#[test]
fn the_sglang_config_file_is_two_keys_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("lmcache").join("i-1");
    let path = write_sglang_config(&dir, 25001).unwrap();
    assert_eq!(path, dir.join("config.yaml"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "mp_host: 127.0.0.1\nmp_port: 25001\n"
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&dir), 0o700);
    // Written again (a restart-parked launch's wake), still exactly two keys.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    write_sglang_config(&dir, 25009).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "mp_host: 127.0.0.1\nmp_port: 25009\n"
    );
    assert_eq!(mode(&path), 0o600);
    // A symlink in the file's place is refused, its target untouched.
    let other = root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let target = root.path().join("target");
    std::fs::write(&target, "kept").unwrap();
    std::os::unix::fs::symlink(&target, other.join("config.yaml")).unwrap();
    assert!(write_sglang_config(&other, 25001).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
}

// ---------------------------------------------------------------- server start

// T45 T37 (ADR 0033 §3): a server that answers `/status` and listens on
// loopback only is the launch's server group.
#[tokio::test]
async fn a_server_on_loopback_that_answers_status_starts() {
    let tool = Arc::new(Tool::new());
    let proc = fake_proc(&[(3000, 1111), (3001, 1111)]);
    let port = status_server().await;
    let group = start_server(tool.clone(), "i-1", &server_plan(port, proc.path(), false))
        .await
        .unwrap();
    assert_eq!(group, tool.server_group);
    assert_eq!(tool.spawned_kinds(), ["lmcache"]);
    assert!(tool.stopped.lock().unwrap().is_empty());
}

// T45 (ADR 0033 §3): no `/status` within the start bound: the server is
// stopped (verified) and the reason is `server_start_failed`.
#[tokio::test]
async fn a_server_silent_past_its_bound_is_stopped_and_fails_to_start() {
    let tool = Arc::new(Tool::new());
    let proc = fake_proc(&[]);
    let mut plan = server_plan(closed_port().await, proc.path(), false);
    plan.start_bound = Duration::from_millis(600);
    let started = std::time::Instant::now();
    let (reason, detail) = start_server(tool.clone(), "i-1", &plan).await.unwrap_err();
    assert_eq!(reason, LmcacheOffReason::ServerStartFailed);
    assert!(detail.contains("GET /status"), "{detail}");
    assert!(detail.ends_with("the server was stopped"), "{detail}");
    assert_eq!(*tool.stopped.lock().unwrap(), [vec![tool.server.clone()]]);
    assert!(started.elapsed() < Duration::from_secs(5));
}

// T45 T37 (ADR 0033 §3, ADR 0031 §7): a server group listening off loopback is
// stopped with its whole group and the reason is `listener_exposed`.
#[tokio::test]
async fn a_server_listening_off_loopback_is_stopped_as_exposed() {
    let tool = Arc::new(Tool::new());
    let proc = fake_proc(&[(3000, 1111), (3001, 2222)]);
    let port = status_server().await;
    let (reason, detail) =
        start_server(tool.clone(), "i-1", &server_plan(port, proc.path(), false))
            .await
            .unwrap_err();
    assert_eq!(reason, LmcacheOffReason::ListenerExposed);
    assert!(detail.contains("0.0.0.0:25002"), "{detail}");
    assert_eq!(
        *tool.stopped.lock().unwrap(),
        std::slice::from_ref(&tool.server_group)
    );
}

// T45 (ADR 0033 §3): a server that exits before answering is stopped and
// fails to start, without waiting out its bound.
#[tokio::test]
async fn a_server_that_exits_before_answering_fails_at_once() {
    let tool = Arc::new(Tool::new());
    *tool.server_presence.lock().unwrap() = Presence::Gone;
    let proc = fake_proc(&[]);
    let mut plan = server_plan(closed_port().await, proc.path(), false);
    plan.start_bound = Duration::from_secs(30);
    let started = std::time::Instant::now();
    let (reason, _) = start_server(tool.clone(), "i-1", &plan).await.unwrap_err();
    assert_eq!(reason, LmcacheOffReason::ServerStartFailed);
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ---------------------------------------------------------- vLLM Initialize

/// A launch with a server that starts on `status`'s port.
async fn started_state(proc: &Path, required: bool) -> Arc<LmcacheState> {
    LmcacheState::new(LmcacheLaunch::Start(server_plan(
        status_server().await,
        proc,
        required,
    )))
}

// T45 (ADR 0033 §§2.2, 3): server first, then the engine with the connector;
// Ready reports both groups and LMCache on.
#[tokio::test]
async fn a_vllm_launch_starts_its_server_first_and_reports_both_groups() {
    let proc = fake_proc(&[(3000, 1111), (3001, 1111)]);
    let state = started_state(proc.path(), false).await;
    let tool = Arc::new(Tool::new());
    let port = stub_engine().await;
    let adapter = vllm_adapter(
        port,
        vllm_plan(port, Some(state.clone()), None),
        tool.clone(),
    );
    let observation = adapter
        .execute_persisted(&initialize(30_000))
        .await
        .unwrap();
    assert_eq!(tool.spawned_kinds(), ["lmcache", "engine"]);
    let argv = tool.engine_command().argv;
    let flag = argv
        .iter()
        .position(|a| a == "--kv-transfer-config")
        .unwrap();
    assert!(flag < argv.iter().position(|a| a == USER_ARGS_MARKER).unwrap());
    assert_eq!(argv[flag + 1], vllm_kv_transfer_config(25001));
    let roles: BTreeSet<&str> = observation
        .identities
        .iter()
        .map(|i| i.role.as_str())
        .collect();
    assert_eq!(
        roles,
        BTreeSet::from(["api", "worker-0", "lmcache", "lmcache-0"])
    );
    assert_eq!(
        state.report(),
        Some(LmcacheReport::On {
            server: tool.server_group.clone(),
            rpc_port: 25001,
            http_port: match state.launch() {
                Some(LmcacheLaunch::Attached { plan, .. }) => plan.http_port,
                other => panic!("{other:?}"),
            },
            pool_bytes: 4 << 30,
        })
    );
}

// T45 (ADR 0033 §9): `required` and a server that did not start: the launch
// fails `lmcache_unavailable:server_start_failed` and no engine is spawned.
#[tokio::test]
async fn a_required_launch_fails_without_spawning_an_engine() {
    let proc = fake_proc(&[]);
    let mut plan = server_plan(closed_port().await, proc.path(), true);
    plan.start_bound = Duration::from_millis(400);
    let state = LmcacheState::new(LmcacheLaunch::Start(plan));
    let tool = Arc::new(Tool::new());
    let port = stub_engine().await;
    let adapter = vllm_adapter(
        port,
        vllm_plan(port, Some(state.clone()), None),
        tool.clone(),
    );
    let error = adapter
        .execute_persisted(&initialize(30_000))
        .await
        .unwrap_err();
    let RuntimeError::LaunchFailed(message) = error else {
        panic!("{error:?}");
    };
    assert!(
        message.starts_with("lmcache_unavailable:server_start_failed; "),
        "{message}"
    );
    assert_eq!(tool.spawned_kinds(), ["lmcache"], "no engine was spawned");
    assert!(matches!(
        state.report(),
        Some(LmcacheReport::Off {
            reason: LmcacheOffReason::ServerStartFailed,
            connector_failed: false,
            ..
        })
    ));
}

// T45 (ADR 0033 §§3, 9): under `auto` a server that did not start leaves the
// launch to run without the connector, LMCache off with the reason.
#[tokio::test]
async fn an_auto_launch_runs_without_the_connector_when_the_server_fails() {
    let proc = fake_proc(&[(3000, 2222), (3001, 1111)]);
    let state = started_state(proc.path(), false).await;
    let tool = Arc::new(Tool::new());
    let port = stub_engine().await;
    let adapter = vllm_adapter(
        port,
        vllm_plan(port, Some(state.clone()), None),
        tool.clone(),
    );
    let observation = adapter
        .execute_persisted(&initialize(30_000))
        .await
        .unwrap();
    assert_eq!(tool.spawned_kinds(), ["lmcache", "engine"]);
    assert!(!tool
        .engine_command()
        .argv
        .iter()
        .any(|a| a == "--kv-transfer-config"));
    assert!(observation
        .identities
        .iter()
        .all(ProcessIdentity::is_engine));
    assert!(matches!(
        state.report(),
        Some(LmcacheReport::Off {
            reason: LmcacheOffReason::ListenerExposed,
            connector_failed: false,
            ..
        })
    ));
}

// T45 (ADR 0033 §§3, 8): the server exiting while its engine starts is an
// engine start failure with the connector.
#[tokio::test]
async fn a_server_exiting_while_the_engine_starts_is_an_engine_start_failure() {
    let proc = fake_proc(&[(3000, 1111), (3001, 1111)]);
    let state = started_state(proc.path(), false).await;
    let mut tool = Tool::new();
    tool.server_dies_with_engine = true;
    let tool = Arc::new(tool);
    // An engine that never lists its model, so readiness keeps polling.
    let port = closed_port().await;
    let adapter = vllm_adapter(
        port,
        vllm_plan(port, Some(state.clone()), None),
        tool.clone(),
    );
    let error = adapter
        .execute_persisted(&initialize(30_000))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, RuntimeError::LaunchFailed(m) if m.starts_with("the LMCache server exited")),
        "{error:?}"
    );
    assert!(matches!(
        state.report(),
        Some(LmcacheReport::Off {
            reason: LmcacheOffReason::EngineStartFailed,
            connector_failed: true,
            ..
        })
    ));
}

// T45 T21 (ADR 0033 §8): an engine started with the connector that exits
// before Ready is reported with the engine log's redacted tail; the step's
// own error is unchanged.
#[tokio::test]
async fn an_engine_failing_with_the_connector_reports_the_redacted_tail() {
    let proc = fake_proc(&[(3000, 1111), (3001, 1111)]);
    let state = started_state(proc.path(), false).await;
    let logs = tempfile::tempdir().unwrap();
    let log = logs.path().join("i-1.log");
    std::fs::write(
        &log,
        "loading https://hub.example/m?token=hf-secret-value-123\nLMCache connector failed\n",
    )
    .unwrap();
    let mut tool = Tool::new();
    tool.engine_dies = true;
    let tool = Arc::new(tool);
    let port = closed_port().await;
    let adapter = vllm_adapter(
        port,
        vllm_plan(port, Some(state.clone()), Some(&log)),
        tool.clone(),
    );
    let error = adapter
        .execute_persisted(&initialize(30_000))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, RuntimeError::LaunchFailed(m) if m.starts_with("the engine exited before readiness")),
        "{error:?}"
    );
    let Some(LmcacheReport::Off {
        reason: LmcacheOffReason::EngineStartFailed,
        detail,
        connector_failed: true,
    }) = state.report()
    else {
        panic!("{:?}", state.report());
    };
    assert!(detail.contains("LMCache connector failed"), "{detail}");
    assert!(!detail.contains("hf-secret-value-123"), "{detail}");
    assert!(detail.len() <= capyctl_adapters::lmcache::MAX_DETAIL_BYTES);
}

// T45 (ADR 0033 §7): a restart-parked launch parks by stopping its engine group
// alone (the server kept), and its Restore starts the engine again on the same
// server under the wake key, both groups reported.
#[tokio::test]
async fn a_restart_park_stops_the_engine_and_a_restore_starts_it_on_the_server() {
    let proc = fake_proc(&[(3000, 1111), (3001, 1111)]);
    let state = started_state(proc.path(), false).await;
    let tool = Arc::new(Tool::new());
    let port = stub_engine().await;
    let adapter = vllm_adapter(
        port,
        vllm_plan(port, Some(state.clone()), None),
        tool.clone(),
    );
    let ready = adapter
        .execute_persisted(&initialize(30_000))
        .await
        .unwrap();

    let park = RuntimeCommand {
        action: RuntimeAction::Park,
        context: context(
            ExecutionIdentities::Retained(ready.identities.clone()),
            "park-1",
            30_000,
        ),
    };
    let parked = adapter.execute_persisted(&park).await.unwrap();
    assert_eq!(parked.identities, tool.server_group);
    assert!(parked.facts.contains(&Milestone::MemoryReleased));
    assert_eq!(
        *tool.stopped.lock().unwrap(),
        std::slice::from_ref(&tool.engine_group)
    );

    let restore = RuntimeCommand {
        action: RuntimeAction::Restore,
        context: context(
            ExecutionIdentities::Retained(parked.identities.clone()),
            "restore-1",
            30_000,
        ),
    };
    let restored = adapter.execute_persisted(&restore).await.unwrap();
    assert_eq!(
        tool.spawned_kinds(),
        ["lmcache", "engine", "wake:restore-1"]
    );
    assert_eq!(restored.identities.len(), 4);
    assert!(state.is_on());
}
