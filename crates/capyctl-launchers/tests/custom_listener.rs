//! ADR 0031 §4, §7 end to end with real processes: a custom engine (a small
//! Python HTTP server) launched through the durable launcher in its own
//! process group, made ready by the custom adapter, and checked for
//! listeners off loopback. An engine that opens one fails the launch
//! `custom_listener_exposed` and is stopped; one that listens only on
//! 127.0.0.1 is ready; a command that daemonizes and exits fails (T13).
//! Every engine also exits on its own after a minute, so a failed test
//! leaves nothing running for long.
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capyctl_adapters::custom::{CustomAdapter, PlanInputCustom};
use capyctl_adapters::traits::{
    EngineAdapter, OwnedProcessLaunch, RuntimeAction, RuntimeCommand, RuntimeError,
};
use capyctl_config::custom::{CustomManifest, PlaceholderValues};
use capyctl_domain::completion::{
    ExecutionIdentities, Presence, ProcessIdentity, StepExecutionContext, TransitionToken,
};
use capyctl_launchers::{AssociationError, DurableProcessLaunch, LaunchAssociation};

const ENGINE: &str = r#"#!/usr/bin/env python3
import http.server, json, os, socket, sys, threading, time
args = sys.argv[1:]
host = args[args.index("--host") + 1]
port = int(args[args.index("--port") + 1])
mode = args[args.index("--mode") + 1]
if mode == "daemonize" and os.fork() > 0:
    os._exit(0)
threading.Thread(target=lambda: (time.sleep(60), os._exit(0)), daemon=True).start()
if mode == "expose":
    extra = socket.socket()
    extra.bind(("0.0.0.0", 0))
    extra.listen(1)
def chunk(delta, finish):
    return json.dumps({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "basal",
                       "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
class Engine(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        status = 200 if self.path == "/health" else 404
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        body = ("data: %s\n\ndata: %s\n\ndata: [DONE]\n\n" % (
            chunk({"role": "assistant", "content": "Ready."}, None), chunk({}, "stop"))).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
http.server.HTTPServer((host, port), Engine).serve_forever()
"#;

struct Accept;
impl LaunchAssociation for Accept {
    fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> {
        Ok(())
    }
}

/// Records the API identity the launcher reports, for cleanup and checks.
struct Recording {
    inner: DurableProcessLaunch,
    api: std::sync::Mutex<Option<ProcessIdentity>>,
}

impl OwnedProcessLaunch for Recording {
    fn spawn_durable(
        &self,
        incarnation: &str,
        cmd: &capyctl_adapters::traits::RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        let api = self.inner.spawn_durable(incarnation, cmd)?;
        *self.api.lock().unwrap() = Some(api.clone());
        Ok(api)
    }
    fn present(&self, identity: &ProcessIdentity) -> Presence {
        self.inner.present(identity)
    }
    fn observe_group(&self, api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        self.inner.observe_group(api)
    }
    fn terminate_owned(
        &self,
        identities: &[ProcessIdentity],
        grace: Duration,
    ) -> Result<(), RuntimeError> {
        self.inner.terminate_owned(identities, grace)
    }
}

impl Drop for Recording {
    /// Whatever a test proved, the group it started does not outlive it.
    fn drop(&mut self) {
        if let Some(api) = self.api.lock().unwrap().as_ref() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(api.pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn initialize() -> RuntimeCommand {
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
            incarnation: format!("custom-{}", now_ms()),
            issued_at_ms: now_ms(),
            deadline_ms: now_ms() + 30_000,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: Some("g-1".into()),
            launch_settings: Some(capyctl_domain::launch::LaunchSettings::Custom(
                capyctl_domain::launch::CustomLaunchSettings {
                    common: Default::default(),
                    memory: capyctl_domain::launch::MemoryRequest {
                        request_bytes: 1 << 30,
                        kv_cache_bytes: 0,
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
                    provenance: Default::default(),
                },
            )),
        },
    }
}

/// Launch the Python engine in `mode` through the durable launcher and the
/// custom adapter's Initialize.
async fn launch(
    mode: &str,
) -> (
    Result<capyctl_domain::completion::EffectObservation, RuntimeError>,
    Arc<Recording>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("serve-engine");
    std::fs::write(&executable, ENGINE).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let manifest = CustomManifest::parse(&format!(
        "kind: custom_engine\nschema_version: 1\napi: chat\n\
         args: [--host, \"{{host}}\", --port, \"{{port}}\", --model, \"{{model_dir}}\", --mode, {mode}]\n\
         health: /health\n"
    ))
    .unwrap();
    let port = free_port();
    let plan = PlanInputCustom {
        executable: executable.to_string_lossy().into_owned(),
        manifest: manifest.clone(),
        values: PlaceholderValues {
            port,
            model_dir: dir.path().to_string_lossy().into_owned(),
            served_name: "basal".into(),
            context_length: None,
            api_key: None,
        },
        engine_env: Default::default(),
        engine_log: Some(dir.path().join("engine.log").to_string_lossy().into_owned()),
        cuda_namespace: None,
    };
    let tools = Arc::new(Recording {
        inner: DurableProcessLaunch::new(Arc::new(Accept)),
        api: Default::default(),
    });
    let adapter = CustomAdapter::new(
        format!("http://127.0.0.1:{port}").parse().unwrap(),
        "unversioned".into(),
        "basal".into(),
        None,
    )
    .with_api(manifest.api)
    .with_health(manifest.health.clone())
    .with_launch(plan)
    .with_tools(tools.clone());
    let result = adapter.execute_persisted(&initialize()).await;
    (result, tools, dir)
}

fn python3() -> bool {
    std::process::Command::new("/usr/bin/env")
        .args(["python3", "-c", "pass"])
        .env_clear()
        .env("PATH", capyctl_config::toolchain::SYSTEM_PATH)
        .status()
        .is_ok_and(|status| status.success())
}

// T44 (ADR 0031 §7): an engine listening only on 127.0.0.1 passes the
// loopback check and is ready.
#[tokio::test]
async fn a_loopback_engine_is_ready() {
    assert!(python3(), "python3 is needed on the fixed system PATH");
    let (result, tools, _dir) = launch("loopback").await;
    let observation = result.unwrap();
    assert!(observation.receipt.contains("listeners on loopback"));
    tools
        .terminate_owned(&observation.identities, Duration::from_secs(2))
        .unwrap();
}

// T44 (ADR 0031 §7, §10): an owned process that opens a listener on
// 0.0.0.0 fails the launch `custom_listener_exposed`, naming it, and the
// launch's processes are stopped, proven gone.
#[tokio::test]
async fn an_engine_listening_off_loopback_fails_and_is_stopped() {
    assert!(python3(), "python3 is needed on the fixed system PATH");
    let (result, tools, _dir) = launch("expose").await;
    let error = result.unwrap_err();
    let RuntimeError::LaunchFailed(text) = &error else {
        panic!("{error}");
    };
    assert!(
        text.starts_with("custom_listener_exposed: an owned process listens on 0.0.0.0:"),
        "{text}"
    );
    assert!(text.ends_with("; the engine was stopped"), "{text}");
    let api = tools.api.lock().unwrap().clone().unwrap();
    assert_eq!(tools.present(&api), Presence::Gone);
    assert!(tools.observe_group(&api).unwrap().is_empty());
}

// T44 T13 (ADR 0031 §4, SPEC §8.1): a command that daemonizes and exits
// leaves no owned handle; the launch fails though its daemon answers.
#[tokio::test]
async fn a_daemonizing_command_fails_the_launch() {
    assert!(python3(), "python3 is needed on the fixed system PATH");
    let (result, _tools, _dir) = launch("daemonize").await;
    let error = result.unwrap_err();
    assert!(
        matches!(&error, RuntimeError::LaunchFailed(text) if text.contains("exited before readiness")),
        "{error}"
    );
}
