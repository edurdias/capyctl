//! SPEC §4.3 (owner decision P3, 2026-09-22): stopping or signalling a role is a
//! service restart, and draining a host is a separate explicit action.
//!
//! These drive the real `mllm` binary. The standalone role runs a fake vLLM
//! executable (a small Python HTTP server speaking the vLLM surface this project
//! uses), so the launch, the recorded process identities, the signal, the
//! re-attachment and the drain all go through the product's own paths.
//!
//! CPU and fake-engine tests are not qualification: passing these never shows
//! that a native engine recipe works (SPEC §18). The live steps that prove the
//! same behaviour on a real engine are in the W11 hand-off.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

mod support;
use support::process::{free_port, free_ports, output_within, Guarded};

use mllm_config::effective::{Engine, ModelSource};
use serde_json::{json, Value};

/// A vLLM stand-in: `serve <model> --port P --served-model-name R`, the key only
/// from `VLLM_API_KEY`, every `/v1` route keyed, one forked worker child in the
/// same process group, SSE chat. Each start appends its pid to `launches.log`,
/// which is how the tests tell a re-attached engine from a relaunched one. A chat
/// whose last message is `slow` or `slower` streams for about 3 s or 12 s.
const FAKE_VLLM: &str = r#"
import json, os, sys, time, threading, http.server
args = sys.argv[1:]
active = [0]
active_lock = threading.Lock()
here = os.path.dirname(os.path.abspath(__file__))
key = os.environ.get("VLLM_API_KEY", "")
with open(os.path.join(here, "launches.log"), "a") as f:
    f.write(str(os.getpid()) + "\n")
port = int(args[args.index("--port") + 1])
served = args[args.index("--served-model-name") + 1]
if os.fork() == 0:
    while True:
        time.sleep(60)

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    def log_message(self, *a):
        pass
    def keyed(self):
        if not key or self.headers.get("Authorization") != "Bearer " + key:
            self.send_response(401); self.end_headers(); return False
        return True
    def send_json(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if self.path == "/health":
            self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers(); return
        if not self.keyed(): return
        if self.path == "/v1/models":
            self.send_json({"object": "list", "data": [{"id": served, "object": "model"}]})
        elif self.path == "/is_sleeping":
            self.send_json({"is_sleeping": False})
        elif self.path == "/metrics":
            # The vLLM gauges quiescence evidence reads (SPEC §10, W12).
            with active_lock:
                running = active[0]
            body = ("vllm:num_requests_running %d\nvllm:num_requests_waiting 0\n"
                    "vllm:kv_cache_usage_perc 0.0\n" % running).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body)
        else:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers()
    def do_POST(self):
        if not self.keyed(): return
        if self.path != "/v1/chat/completions":
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers(); return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        if body.get("model") != served:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers(); return
        with active_lock:
            active[0] += 1
        try:
            self.chat(body)
        finally:
            with active_lock:
                active[0] -= 1
    def chat(self, body):
        last = (body.get("messages") or [{}])[-1].get("content")
        count = {"slow": 30, "slower": 120}.get(last, 1)
        def chunk(delta, finish):
            return {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": served,
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(("data: " + json.dumps(chunk({"role": "assistant", "content": ""}, None)) + "\n\n").encode())
            self.wfile.flush()
            for i in range(count):
                if count > 1:
                    time.sleep(0.1)
                self.wfile.write(("data: " + json.dumps(chunk({"content": "fake-vllm-answer " + str(i) + " "}, None)) + "\n\n").encode())
                self.wfile.flush()
            self.wfile.write(("data: " + json.dumps(chunk({}, "stop")) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            self.send_json({"id": "c1", "object": "chat.completion", "created": 1, "model": served,
                            "choices": [{"index": 0, "finish_reason": "stop",
                                         "message": {"role": "assistant", "content": "fake-vllm-answer"}}]})

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

fn python3() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("python3"))
                .find(|candidate| candidate.is_file())
        })
        .expect("python3 on PATH for the fake engine")
}

/// A per-installation engine port range (`MLLM_ENGINE_PORTS`):
/// four consecutive loopback ports free when chosen, below the ephemeral
/// range (`support::process::free_ports`). The 8100 default would make every
/// standalone fake engine in parallel tests bind the same port.
fn engine_ports() -> String {
    let ports = free_ports(4, true);
    format!("{}-{}", ports[0], ports[3])
}

/// Owner decision Q11: a vLLM launch runs the installation's interpreter on
/// mllm's protected entry. The stand-ins: `python3` beside the fake engine, and
/// an entry that runs the fake engine in process (its `__file__` stays the
/// engine's, so its records land beside it).
fn protected_entry(bin: &Path, engine: &Path, runtime: &Path) {
    std::os::unix::fs::symlink(python3(), bin.join("python3")).unwrap();
    std::fs::write(
        runtime.join("vllm_entry.py"),
        format!(
            "import runpy\nrunpy.run_path({:?}, run_name=\"__main__\")\n",
            engine.display().to_string()
        ),
    )
    .unwrap();
    // SPEC §9.1 / T21: mllm's runtime modules are this user's and not group-
    // or other-writable, whatever the umask that wrote them.
    for module in ["vllm_entry.py", "mllm_vllm_guard.py"] {
        let path = runtime.join(module);
        if path.exists() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
    }
}

/// One standalone installation: state, a fake engine, a model directory and the
/// loopback addresses this run's listeners use.
struct Installation {
    root: tempfile::TempDir,
    inference: String,
    management: String,
    engines: String,
}

impl Installation {
    fn new() -> Self {
        // The controller lock refuses a state path under a group- or
        // other-writable ancestor, so the root lives under HOME, owner-only.
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = root.path().join("engine");
        std::fs::create_dir_all(&bin).unwrap();
        let engine = bin.join("vllm");
        std::fs::write(&engine, format!("#!{}\n{FAKE_VLLM}", python3().display())).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("mllm_vllm_guard.py"), "# test guard\n").unwrap();
        protected_entry(&bin, &engine, &runtime);
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(root.path().join("models/toy")).unwrap();
        Self {
            inference: format!("127.0.0.1:{}", free_port()),
            management: format!("127.0.0.1:{}", free_port()),
            engines: engine_ports(),
            root,
        }
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// SPEC §4.3, §15.3 (T17): the drain bound is `shutdown.drain_timeout` in
    /// the standalone role document, generated first when it does not exist yet.
    fn drain_timeout(&self, value: &str) {
        let path = self.state().join("config/standalone.yaml");
        if !path.exists() {
            mllm_config::generate_default(mllm_config::ConfigKind::Standalone, &self.state())
                .unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: String = text
            .lines()
            .take_while(|line| *line != "shutdown:")
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(
            &path,
            format!("{kept}shutdown:\n  drain_timeout: {value}\n"),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mllm"));
        command
            .env("MLLM_STATE_DIR", self.state())
            .env("MLLM_VLLM_BIN", self.root.path().join("engine/vllm"))
            .env_remove("MLLM_SGLANG_BIN")
            .env("MLLM_MODELS_ROOT", self.root.path().join("models"))
            .env("MLLM_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("MLLM_ENGINE_FINGERPRINT", "fake-vllm-w11")
            .env("MLLM_KV_CACHE_BYTES", "64MiB")
            .env("MLLM_DEEP_PARK", "off")
            .env("MLLM_INFERENCE_ADDR", &self.inference)
            .env_remove("MLLM_STANDALONE_INFERENCE_ADDR")
            .env("MLLM_MANAGEMENT_ADDR", &self.management)
            .env("MLLM_ENGINE_PORTS", &self.engines);
        command
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    /// Every engine start so far, oldest first.
    fn launches(&self) -> Vec<i32> {
        std::fs::read_to_string(self.root.path().join("engine/launches.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }

    fn api_key(&self) -> String {
        std::fs::read_to_string(self.state().join("identity/credentials"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("api_key: "))
            .unwrap()
            .to_owned()
    }

    fn start(&self, drain_secs: Option<u64>) -> Role {
        self.start_with(drain_secs, &[]).0
    }

    /// As [`Self::start`], with extra `start standalone` arguments; also
    /// returns the ready line.
    fn start_with(&self, drain_secs: Option<u64>, extra: &[&str]) -> (Role, String) {
        let mut command = self.command();
        command.args(["start", "standalone"]).args(extra);
        self.start_command(drain_secs, &mut command)
    }

    /// As [`Self::start_with`], running `command` as the role.
    fn start_command(&self, drain_secs: Option<u64>, command: &mut Command) -> (Role, String) {
        if let Some(seconds) = drain_secs {
            self.drain_timeout(&format!("{seconds}s"));
        }
        let role = Role::spawn(command);
        let ready = role.expect_line(
            |line| line.starts_with("standalone ready"),
            Duration::from_secs(60),
        );
        (role, ready)
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        // The engines deliberately outlive the role; the test must not leave them.
        for pid in self.launches() {
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

struct Role {
    child: Guarded,
    lines: mpsc::Receiver<String>,
    /// Everything the role wrote to stderr so far (it is also forwarded to the
    /// test's own stderr), and whether the stream has ended.
    stderr: Arc<Mutex<(String, bool)>>,
}

impl Role {
    /// Run `command` as a role, reading its stdout lines and its stderr.
    fn spawn(command: &mut Command) -> Self {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        // Guard first: a role that never prints its ready line is killed
        // with its process group when a wait on it panics.
        let mut child = Guarded::spawn(command);
        let (lines, received) = mpsc::channel();
        let stdout = child.child().stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        let stderr = Arc::new(Mutex::new((String::new(), false)));
        let pipe = child.child().stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                eprintln!("{line}");
                let mut text = sink.lock().unwrap();
                text.0.push_str(&line);
                text.0.push('\n');
            }
            sink.lock().unwrap().1 = true;
        });
        Self {
            child,
            lines: received,
            stderr,
        }
    }

    fn expect_line(&self, want: impl Fn(&str) -> bool, within: Duration) -> String {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if want(&line) => return line,
                Ok(_) => {}
                Err(error) => panic!("the role never printed the expected line: {error}"),
            }
        }
    }

    fn signal(&self) {
        self.child.signal(libc::SIGTERM);
    }

    /// Wait for the role to exit, returning its status and its final report.
    fn exit(mut self, within: Duration) -> (std::process::ExitStatus, Value) {
        let deadline = Instant::now() + within;
        let status = loop {
            if let Some(status) = self.child.child().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "the role did not exit within {within:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        let report = self
            .lines
            .try_iter()
            .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
            .find(|value| value["role"] == "standalone")
            .unwrap_or(Value::Null);
        (status, report)
    }
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

fn chat_request(
    installation: &Installation,
    content: &str,
    stream: bool,
) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!(
            "http://{}/v1/chat/completions",
            installation.inference
        ))
        .bearer_auth(installation.api_key())
        .json(&json!({
            "model": "w11-model",
            "stream": stream,
            "messages": [{"role": "user", "content": content}]
        }))
}

/// Poll until a plain chat is served, returning how long it took.
async fn served_within(installation: &Installation, within: Duration) -> Duration {
    let started = Instant::now();
    loop {
        if let Ok(response) = chat_request(installation, "hello", false).send().await {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status == 200 {
                assert!(body.contains("fake-vllm-answer"), "{body}");
                return started.elapsed();
            }
        }
        assert!(
            started.elapsed() < within,
            "inference was not served within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn deploy(installation: &Installation) -> Value {
    deploy_with_deadline(
        installation,
        mllm_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
    )
}

fn deploy_with_deadline(installation: &Installation, request_deadline: &str) -> Value {
    // Sized for the shape the binary publishes on this machine, from an
    // explicit capacity rather than this machine's (see
    // `support::binary_template_memory`).
    let document = mllm_cli::standalone_config::deployment_document(
        "w11-model",
        "w11-model",
        &ModelSource::Local {
            path: installation
                .root
                .path()
                .join("models/toy")
                .to_string_lossy()
                .into_owned(),
        },
        Engine::Vllm,
        &support::binary_template_memory(),
        request_deadline,
        // The host runs with MLLM_DEEP_PARK=off (ADR 0012 opt-out), so
        // its generated deployment is restart_only.
        false,
        "local",
    )
    .expect("the template fits the stated card");
    let file = installation.root.path().join("deployment.json");
    std::fs::write(&file, document.to_string()).unwrap();
    let out = installation.cli(&[
        "deploy",
        "model",
        "--file",
        file.to_str().unwrap(),
        "--activate",
        "--wait",
    ]);
    assert!(
        out.status.success(),
        "deploy: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// T01, T10, T17, T18, T33, T38: a standalone SIGTERM is a restart. Admission closes
/// with a retryable 503, the admitted stream finishes, the process exits 0 with
/// the engine still running, and the next start re-attaches that same engine —
/// no relaunch — serving again only after it is re-proven. A stream that outlives
/// the drain bound is cut at the bound. `mllm drain standalone` then stops the
/// engine with verified cleanup, and the deployment stays eligible for
/// on-demand activation.
/// T03 T21 (owner rule 2026-09-25: every setting three ways): a standalone
/// whose engine installation, runtime directory and engine port range are
/// stated only in its document (`host.local_engine`, `host.runtime_dir`,
/// `host.resource_policy.endpoint_port_range`), started with `--state-dir`
/// instead of `MLLM_STATE_DIR`, boots and serves; the environment then wins
/// over the document, and a flag over both (the published fingerprint).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standalone_engine_settings_come_from_the_document_env_or_flags() {
    let installation = Installation::new();
    let state = installation.state();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, &state).unwrap();
    let mut document: Value =
        mllm_config::parse_document(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let (start, end) = installation.engines.split_once('-').unwrap();
    document["host"]["local_engine"] = json!({
        "vllm": installation.root.path().join("engine/vllm"),
        "build_fingerprint": "yaml-fp",
        "kv_cache": "64MiB",
        "deep_park": "off",
    });
    document["host"]["runtime_dir"] = json!(installation.root.path().join("runtime"));
    document["host"]["resource_policy"]["endpoint_port_range"] =
        json!({"start": start.parse::<u16>().unwrap(), "end": end.parse::<u16>().unwrap()});
    std::fs::write(&path, document.to_string()).unwrap();
    let bare = |command: &mut Command| {
        for name in [
            "MLLM_STATE_DIR",
            "MLLM_VLLM_BIN",
            "MLLM_RUNTIME_DIR",
            "MLLM_ENGINE_FINGERPRINT",
            "MLLM_KV_CACHE_BYTES",
            "MLLM_DEEP_PARK",
            "MLLM_ENGINE_PORTS",
        ] {
            command.env_remove(name);
        }
    };
    let engines = |extra_env: &[(&str, &str)]| -> Value {
        let mut command = installation.command();
        bare(&mut command);
        command.envs(extra_env.iter().copied()).args([
            "--state-dir",
            state.to_str().unwrap(),
            "engine",
            "list",
            "--format",
            "json",
        ]);
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let version = |listed: &Value| {
        let rows = listed["engines"].as_array().cloned().unwrap_or_default();
        let local = rows
            .iter()
            .find(|row| row["profile"] == "local")
            .unwrap_or_else(|| panic!("no local profile: {listed}"));
        local["version"].as_str().unwrap().to_owned()
    };
    let start_with = |env: &[(&str, &str)], flags: &[&str]| {
        let mut command = installation.command();
        bare(&mut command);
        command
            .envs(env.iter().copied())
            .args([
                "--state-dir",
                state.to_str().unwrap(),
                "start",
                "standalone",
            ])
            .args(flags);
        installation.start_command(None, &mut command).0
    };

    // The document alone declares the engine; it serves.
    let role = start_with(&[], &[]);
    assert_eq!(version(&engines(&[])), "yaml-fp");
    let deployed = deploy(&installation);
    assert_eq!(deployed["deployment"]["observed_state"], "ready");
    served_within(&installation, Duration::from_secs(10)).await;
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(25));
    assert!(status.success(), "{status:?}");

    // The environment wins over the document, a flag over both.
    let role = start_with(&[("MLLM_ENGINE_FINGERPRINT", "env-fp")], &[]);
    assert_eq!(version(&engines(&[])), "env-fp");
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(25));
    assert!(status.success(), "{status:?}");
    let role = start_with(
        &[("MLLM_ENGINE_FINGERPRINT", "env-fp")],
        &["--engine-fingerprint", "flag-fp"],
    );
    assert_eq!(version(&engines(&[])), "flag-fp");
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(25));
    assert!(status.success(), "{status:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standalone_signal_restarts_and_drain_stops_with_cleanup() {
    let installation = Installation::new();
    let role = installation.start(Some(20));
    let deployed = deploy(&installation);
    assert_eq!(deployed["deployment"]["observed_state"], "ready");
    let deployment = deployed["deployment"]["id"].as_str().unwrap().to_owned();
    served_within(&installation, Duration::from_secs(10)).await;
    let engine = installation.launches();
    assert_eq!(engine.len(), 1, "one engine launched");
    let engine = engine[0];

    // An admitted stream in flight when the signal arrives.
    let mut streaming = chat_request(&installation, "slow", true)
        .send()
        .await
        .unwrap();
    assert_eq!(streaming.status(), 200);
    let first = streaming.chunk().await.unwrap().unwrap();
    assert!(!first.is_empty());
    role.signal();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // T18: late work is refused with a retryable answer once admission closed.
    let refused = chat_request(&installation, "hello", false)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 503);
    assert_eq!(refused.headers()["retry-after"], "5");
    let refusal: Value = refused.json().await.unwrap();
    assert_eq!(refusal["error"]["code"], "shutting_down");

    // T17: the admitted stream completes on its own inside the bound.
    let mut rest = Vec::new();
    while let Some(chunk) = streaming.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    let rest = String::from_utf8_lossy(&rest);
    assert!(
        rest.contains("[DONE]"),
        "the admitted stream finished: {rest}"
    );

    let (status, report) = role.exit(Duration::from_secs(25));
    assert!(status.success(), "a signalled role exits 0: {status:?}");
    assert_eq!(report["engines"], "retained", "{report}");
    assert_eq!(report["drain"]["in_flight_at_close"], 1, "{report}");
    assert_eq!(report["drain"]["drained"], true, "{report}");
    // T33: the restart did not delete the deployment or stop the engine.
    assert!(alive(engine), "the engine outlives the role restart");

    // T33: the next start re-attaches the same engine and serves only after
    // re-proving it; nothing was relaunched.
    let role = installation.start(Some(2));
    let status = installation.cli(&["status", "deployment", &deployment, "--format", "json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    // Phase B follow-up: until the adopted engine is re-proven its dispatch is
    // closed, and status says so rather than claiming it is serving.
    assert!(
        matches!(
            status["observed_state"].as_str(),
            Some("ready" | "reconciling")
        ),
        "{status}"
    );
    served_within(&installation, Duration::from_secs(20)).await;
    let status = installation.cli(&["status", "deployment", &deployment, "--format", "json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["observed_state"], "ready", "{status}");
    assert_eq!(
        installation.launches(),
        vec![engine],
        "re-attached, not relaunched"
    );
    assert!(alive(engine));

    // T17, T38: the drain is bounded; a stream still running at the bound is
    // cut, honestly, rather than held open or resumed.
    let mut streaming = chat_request(&installation, "slower", true)
        .send()
        .await
        .unwrap();
    assert_eq!(streaming.status(), 200);
    streaming.chunk().await.unwrap().unwrap();
    let signalled = Instant::now();
    role.signal();
    let mut rest = Vec::new();
    while let Ok(Some(chunk)) = streaming.chunk().await {
        rest.extend_from_slice(&chunk);
    }
    assert!(
        !String::from_utf8_lossy(&rest).contains("[DONE]"),
        "the stream outliving the bound was cut"
    );
    let (status, report) = role.exit(Duration::from_secs(15));
    assert!(status.success(), "{status:?}");
    assert!(
        signalled.elapsed() < Duration::from_secs(10),
        "the drain honoured its 2 s bound"
    );
    assert_eq!(report["drain"]["cancelled"], 1, "{report}");
    assert_eq!(report["drain"]["drained"], false, "{report}");
    assert!(alive(engine), "a cut stream never stops the engine");

    // T33: the re-attachment is journaled as evidence: the restarted role adopted
    // the launch with dispatch closed, and reopened it only on local proof.
    {
        let store =
            mllm_store::Store::open(&installation.state().join("server/srv.sqlite3")).unwrap();
        let evidence = store.journal_evidence_of(&deployment).unwrap();
        assert!(
            evidence
                .iter()
                .any(|entry| entry.contains("adopted its ready embedded launch")),
            "{evidence:?}"
        );
        assert!(
            evidence.iter().any(|entry| entry
                .contains("authenticated model list naming w11-model at")
                && entry.contains("dispatch reopened")),
            "{evidence:?}"
        );
    }

    // SPEC §4.3: the explicit drain stops the engine with verified cleanup.
    let role = installation.start(None);
    served_within(&installation, Duration::from_secs(20)).await;
    assert_eq!(installation.launches(), vec![engine]);
    let drained = installation.cli(&["drain", "standalone"]);
    assert!(
        drained.status.success(),
        "drain: {}",
        String::from_utf8_lossy(&drained.stderr)
    );
    let drained: Value = serde_json::from_slice(&drained.stdout).unwrap();
    assert_eq!(drained["drained"], true, "{drained}");
    let stopped = &drained["deployments"][0];
    assert_eq!(stopped["deployment_id"], deployment.as_str(), "{drained}");
    assert_eq!(stopped["state"], "succeeded", "{drained}");
    assert_eq!(stopped["cleanup"], "verified", "{drained}");
    assert_eq!(stopped["observed_state"], "stopped", "{drained}");
    // T10: a drain is not an operator stop; activation on demand stays allowed.
    assert_eq!(stopped["suspended"], false, "{drained}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(engine) {
        assert!(Instant::now() < deadline, "the drained engine is gone");
        std::thread::sleep(Duration::from_millis(50));
    }

    // T10: the next request activates the deployment again, as a new launch.
    served_within(&installation, Duration::from_secs(60)).await;
    let launches = installation.launches();
    assert_eq!(
        launches.len(),
        2,
        "on-demand activation relaunched: {launches:?}"
    );
    assert_ne!(launches[1], engine);

    role.signal();
    let (status, report) = role.exit(Duration::from_secs(40));
    assert!(status.success(), "{status:?}");
    assert_eq!(report["engines"], "retained", "{report}");
    assert!(alive(launches[1]));
}

/// SPEC §4.3, §6: `mllm drain standalone` bounds its drain at 900 s, and a
/// Stop's deadline may not lie beyond its launch's request deadline. A
/// deployment whose request deadline is shorter than the drain window is still
/// drained with verified cleanup, and a retry under the same `--request-id`
/// is not refused (an in-flight Stop's exact replay is covered in
/// `mllm-management/tests/drain.rs`).
// T10 T13 T32
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_stops_a_deployment_whose_request_deadline_is_shorter_than_the_drain() {
    let installation = Installation::new();
    let role = installation.start(Some(20));
    let deployed = deploy_with_deadline(&installation, "600s");
    assert_eq!(deployed["deployment"]["observed_state"], "ready");
    let deployment = deployed["deployment"]["id"].as_str().unwrap().to_owned();
    served_within(&installation, Duration::from_secs(10)).await;
    let engine = installation.launches()[0];

    let request_id = ulid::Ulid::new().to_string();
    let request_id = request_id.as_str();
    let drain = || {
        let out = installation.cli(&["drain", "standalone", "--request-id", request_id]);
        assert!(
            out.status.success(),
            "drain: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let drained = drain();
    assert_eq!(drained["drained"], true, "{drained}");
    assert_eq!(drained["refused"], json!([]), "{drained}");
    let stopped = &drained["deployments"][0];
    assert_eq!(stopped["deployment_id"], deployment.as_str(), "{drained}");
    assert_eq!(stopped["state"], "succeeded", "{drained}");
    assert_eq!(stopped["cleanup"], "verified", "{drained}");
    // T10: a drain is not an operator stop.
    assert_eq!(stopped["suspended"], false, "{drained}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(engine) {
        assert!(Instant::now() < deadline, "the drained engine is gone");
        std::thread::sleep(Duration::from_millis(50));
    }
    // T13: the same request identity is sent again with the same journaled
    // deadline and is not refused. The Stop already settled and released the
    // runtime, so nothing is left on the host to stop and nothing relaunches.
    let replayed = drain();
    assert_eq!(replayed["drained"], true, "{replayed}");
    assert_eq!(replayed["refused"], json!([]), "{replayed}");
    assert_eq!(installation.launches(), vec![engine]);

    role.signal();
    let (status, _) = role.exit(Duration::from_secs(40));
    assert!(status.success(), "{status:?}");
}

/// How long a role start that must refuse is given to exit. A refusal that
/// regressed into a serving role fails the test here instead of hanging it.
const REFUSAL_BOUND: Duration = Duration::from_secs(60);

/// T03 T17: a drain bound outside 0 s to 600 s in the role document refuses to
/// start the role, before any listener or engine exists. The environment
/// variable that used to set it is gone and has no effect.
#[test]
fn a_malformed_drain_bound_refuses_startup() {
    let installation = Installation::new();
    installation.drain_timeout("601s");
    let out = output_within(
        installation
            .command()
            .args(["start", "standalone"])
            .env("MLLM_SHUTDOWN_DRAIN_SECS", "5"),
        REFUSAL_BOUND,
    );
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("shutdown.drain_timeout"), "{said}");
    assert!(installation.launches().is_empty());
}

/// SPEC §9.1, §13.3: standalone requires the same runtime directory integrity
/// the host agent does. A guard module writable by other refuses startup with
/// the named reason before any engine starts. (Group write through the owner's
/// private group is trusted since the 2026-09-22 owner decision.)
// T21 T37
#[test]
fn a_writable_runtime_module_refuses_standalone_startup() {
    let installation = Installation::new();
    std::fs::set_permissions(
        installation.root.path().join("runtime/mllm_vllm_guard.py"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let out = output_within(
        installation.command().args(["start", "standalone"]),
        REFUSAL_BOUND,
    );
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("runtime_integrity"), "{said}");
    assert!(said.contains("mllm_vllm_guard.py"), "{said}");
    assert!(installation.launches().is_empty());
}

/// T03: the management listener override must stay on loopback (SPEC §16.5);
/// the inference override may name any unicast address (design §9) but never
/// a multicast one.
#[test]
fn a_non_loopback_standalone_listener_is_refused() {
    let installation = Installation::new();
    // A port free now: a regressed refusal would otherwise bind a fixed
    // public port, and serve until the bound below fails the test.
    // Owner decision 2026-09-25: MLLM_MANAGEMENT_ADDR, and the deprecated
    // MLLM_STANDALONE_MANAGEMENT_ADDR when it is the one set.
    for variable in ["MLLM_MANAGEMENT_ADDR", "MLLM_STANDALONE_MANAGEMENT_ADDR"] {
        let out = output_within(
            installation
                .command()
                .args(["start", "standalone"])
                .env_remove("MLLM_MANAGEMENT_ADDR")
                .env(variable, format!("0.0.0.0:{}", free_port())),
            REFUSAL_BOUND,
        );
        assert!(!out.status.success());
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains(variable), "{said}");
    }
    for variable in ["MLLM_INFERENCE_ADDR", "MLLM_STANDALONE_INFERENCE_ADDR"] {
        let out = output_within(
            installation
                .command()
                .args(["start", "standalone"])
                .env_remove("MLLM_INFERENCE_ADDR")
                .env(variable, format!("224.0.0.1:{}", free_port())),
            REFUSAL_BOUND,
        );
        assert!(!out.status.success());
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains(variable), "{said}");
    }
}

/// Everything `role` wrote to stderr, once the stream has ended (the role
/// has exited).
fn stderr_of(stderr: &Arc<Mutex<(String, bool)>>) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = stderr.lock().unwrap();
        if text.1 {
            return text.0.clone();
        }
        drop(text);
        assert!(Instant::now() < deadline, "the role's stderr never ended");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Run `command` as a role until it prints `ready`, stop it, and return the
/// ready line and everything it wrote to stderr.
fn run_until_ready(command: &mut Command, ready: impl Fn(&str) -> bool) -> (String, String) {
    let role = Role::spawn(command);
    let line = role.expect_line(ready, Duration::from_secs(60));
    let stderr = role.stderr.clone();
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(30));
    assert!(status.success(), "{status:?}");
    (line, stderr_of(&stderr))
}

/// T02 (ADR 0019, design §9): a standalone installation whose document the
/// previous generator wrote (inference on `127.0.0.1:8443`) is migrated on its
/// first start: the notice is printed once on stderr, the document states
/// `0.0.0.0:8443` with the original kept beside it, and the next start prints
/// nothing. The runs here listen on a loopback port through
/// `MLLM_INFERENCE_ADDR`, so the test never serves on every interface; the
/// document's effective bind is checked in `standalone_start.rs`.
#[test]
fn the_old_loopback_standalone_document_is_migrated_once() {
    let installation = Installation::new();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, &installation.state())
            .unwrap();
    let previous = std::fs::read_to_string(&path)
        .unwrap()
        .replace("0.0.0.0:8443", "127.0.0.1:8443");
    std::fs::write(&path, &previous).unwrap();

    let standalone = |line: &str| line.starts_with("standalone ready");
    let mut command = installation.command();
    command.args(["start", "standalone"]);
    let (_, said) = run_until_ready(&mut command, standalone);
    assert_eq!(
        said.matches("NOTICE: mllm 0.1.0 serves inference on all interfaces")
            .count(),
        1,
        "{said}"
    );
    assert!(
        said.contains(&format!("Configuration updated: {}", path.display())),
        "{said}"
    );
    assert!(!said.contains("config_migration_failed"), "{said}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        previous.replace("127.0.0.1:8443", "0.0.0.0:8443")
    );
    let backup = path.with_file_name("standalone.yaml.pre-0.1.0");
    assert_eq!(std::fs::read_to_string(backup).unwrap(), previous);
    assert!(installation
        .state()
        .join("migrations/inference-bind-v1")
        .exists());

    let mut command = installation.command();
    command.args(["start", "standalone"]);
    let (_, said) = run_until_ready(&mut command, standalone);
    assert!(!said.contains("NOTICE"), "{said}");
}

/// T03 (design §9, owner rule): the standalone inference address is set three
/// ways, `--listen` > `MLLM_INFERENCE_ADDR` > the document's
/// `server.listeners.inference.bind`; the deprecated
/// `MLLM_STANDALONE_INFERENCE_ADDR` still works, after the new variable, and
/// says it is deprecated.
#[test]
fn the_standalone_inference_address_follows_flag_then_environment_then_document() {
    let installation = Installation::new();
    let (path, _) =
        mllm_config::generate_default(mllm_config::ConfigKind::Standalone, &installation.state())
            .unwrap();
    let document = format!("127.0.0.1:{}", free_port());
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("0.0.0.0:8443", &document)).unwrap();
    let environment = format!("127.0.0.1:{}", free_port());
    let flag = format!("127.0.0.1:{}", free_port());
    let alias = format!("127.0.0.1:{}", free_port());
    let standalone = |line: &str| line.starts_with("standalone ready");
    let bound = |line: &str| {
        line.rsplit("inference listener ")
            .next()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim_end_matches(')')
            .to_owned()
    };

    let mut command = installation.command();
    command
        .args(["start", "standalone", "--listen", &flag])
        .env("MLLM_INFERENCE_ADDR", &environment);
    assert_eq!(bound(&run_until_ready(&mut command, standalone).0), flag);

    let mut command = installation.command();
    command
        .args(["start", "standalone"])
        .env("MLLM_INFERENCE_ADDR", &environment);
    assert_eq!(
        bound(&run_until_ready(&mut command, standalone).0),
        environment
    );

    let mut command = installation.command();
    command
        .args(["start", "standalone"])
        .env_remove("MLLM_INFERENCE_ADDR");
    let (line, said) = run_until_ready(&mut command, standalone);
    assert_eq!(bound(&line), document);
    assert!(!said.contains("deprecated"), "{said}");

    let mut command = installation.command();
    command
        .args(["start", "standalone"])
        .env_remove("MLLM_INFERENCE_ADDR")
        .env("MLLM_STANDALONE_INFERENCE_ADDR", &alias);
    let (line, said) = run_until_ready(&mut command, standalone);
    assert_eq!(bound(&line), alias);
    assert!(
        said.contains("MLLM_STANDALONE_INFERENCE_ADDR is deprecated; use MLLM_INFERENCE_ADDR"),
        "{said}"
    );

    let mut command = installation.command();
    command
        .args(["start", "standalone"])
        .env("MLLM_INFERENCE_ADDR", &environment)
        .env("MLLM_STANDALONE_INFERENCE_ADDR", &alias);
    let (line, said) = run_until_ready(&mut command, standalone);
    assert_eq!(bound(&line), environment);
    assert!(said.contains("deprecated"), "{said}");
}

/// A server initialised by the binary, every listener on a free loopback port
/// except inference, which states `inference`.
fn server_installation(inference: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = root.path().join("server");
    let config = root.path().join("server.yaml");
    let out = server_command(&state)
        .args(["init", "server", "--output", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut server: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    for name in ["management", "bootstrap", "control"] {
        let address = format!("127.0.0.1:{}", free_port());
        server["listeners"][name]["bind"] = address.clone().into();
        if name != "management" {
            server["enrollment"][format!("{name}_address")] = format!("https://{address}").into();
        }
    }
    server["listeners"]["inference"]["bind"] = inference.into();
    std::fs::write(&config, serde_json::to_vec_pretty(&server).unwrap()).unwrap();
    (root, state, config)
}

fn server_command(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mllm"));
    command
        .env("MLLM_STATE_DIR", state)
        .env_remove("MLLM_INFERENCE_ADDR")
        .env_remove("MLLM_STANDALONE_INFERENCE_ADDR")
        .env_remove("MLLM_VLLM_BIN")
        .env_remove("MLLM_SGLANG_BIN");
    command
}

/// The inference address in a server's start banner.
fn server_inference(banner: &str) -> String {
    serde_json::from_str::<Value>(banner).unwrap()["inference"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// T01 T02 T03 (ADR 0019, design §9, owner rule): `mllm start server --listen`
/// moves the server's inference listener for one run; the address follows
/// `--listen` > `MLLM_INFERENCE_ADDR` > the document, as for standalone. A
/// server document stating the old loopback default is migrated once, with
/// the notice; loopback set back afterwards is kept. Every run listens on a
/// loopback port.
#[test]
fn start_server_listen_environment_and_migration() {
    let (_root, state, config) = server_installation("127.0.0.1:8443");
    let original = std::fs::read_to_string(&config).unwrap();
    let banner = |line: &str| line.contains("\"role\":\"server\"");
    let start = |state: &Path| {
        let mut command = server_command(state);
        command.args(["start", "server", "--config", config.to_str().unwrap()]);
        command
    };
    let flag = format!("127.0.0.1:{}", free_port());
    let environment = format!("127.0.0.1:{}", free_port());

    // First start: --listen wins over the environment; the document migrates.
    let mut command = start(&state);
    command
        .args(["--listen", &flag])
        .env("MLLM_INFERENCE_ADDR", &environment);
    let (line, said) = run_until_ready(&mut command, banner);
    assert_eq!(server_inference(&line), flag);
    assert_eq!(
        said.matches("NOTICE: mllm 0.1.0 serves inference on all interfaces")
            .count(),
        1,
        "{said}"
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        original.replace("127.0.0.1:8443", "0.0.0.0:8443")
    );
    assert_eq!(
        std::fs::read_to_string(config.with_file_name("server.yaml.pre-0.1.0")).unwrap(),
        original
    );
    assert!(state.join("migrations/inference-bind-v1").exists());

    // The environment wins over the document; no second notice.
    let mut command = start(&state);
    command.env("MLLM_INFERENCE_ADDR", &environment);
    let (line, said) = run_until_ready(&mut command, banner);
    assert_eq!(server_inference(&line), environment);
    assert!(!said.contains("NOTICE"), "{said}");

    // The operator narrows the document back to loopback: it is kept and bound.
    let document = format!("127.0.0.1:{}", free_port());
    std::fs::write(&config, original.replace("127.0.0.1:8443", &document)).unwrap();
    let (line, said) = run_until_ready(&mut start(&state), banner);
    assert_eq!(server_inference(&line), document);
    assert!(!said.contains("NOTICE"), "{said}");
    std::fs::write(&config, &original).unwrap();
    let mut command = start(&state);
    command.env("MLLM_INFERENCE_ADDR", &environment);
    let (_, said) = run_until_ready(&mut command, banner);
    assert!(!said.contains("NOTICE"), "{said}");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        original,
        "never migrated twice"
    );

    // The deprecated standalone name also moves the server, with a warning.
    let alias = format!("127.0.0.1:{}", free_port());
    let mut command = start(&state);
    command.env("MLLM_STANDALONE_INFERENCE_ADDR", &alias);
    let (line, said) = run_until_ready(&mut command, banner);
    assert_eq!(server_inference(&line), alias);
    assert!(
        said.contains("MLLM_STANDALONE_INFERENCE_ADDR is deprecated"),
        "{said}"
    );
}

/// T37 (design §9): the server's inference authentication follows the same
/// rule: `--no-inference-auth` on a non-loopback bind prints the warning
/// once; the document's `authentication: none` on loopback says nothing.
#[test]
fn start_server_warns_only_for_an_exposed_unauthenticated_listener() {
    let (_root, state, config) = server_installation("127.0.0.1:8443");
    std::fs::create_dir_all(state.join("migrations")).unwrap();
    std::fs::write(state.join("migrations/inference-bind-v1"), "").unwrap();
    let banner = |line: &str| line.contains("\"role\":\"server\"");
    let open = format!("0.0.0.0:{}", free_port());
    let mut command = server_command(&state);
    command.args([
        "start",
        "server",
        "--config",
        config.to_str().unwrap(),
        "--listen",
        &open,
        "--no-inference-auth",
    ]);
    let (line, said) = run_until_ready(&mut command, banner);
    assert_eq!(server_inference(&line), open);
    assert_eq!(
        said.matches(&format!(
            "WARNING: the inference endpoint on {open} accepts requests without an API key."
        ))
        .count(),
        1,
        "{said}"
    );
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("\"api_key\"", "\"none\"")).unwrap();
    let loopback = format!("127.0.0.1:{}", free_port());
    let mut command = server_command(&state);
    command.args([
        "start",
        "server",
        "--config",
        config.to_str().unwrap(),
        "--listen",
        &loopback,
    ]);
    let (_, said) = run_until_ready(&mut command, banner);
    assert!(!said.contains("WARNING"), "{said}");
}

/// T01 T03 (design §9): `--listen` replaces the inference bind for one run and
/// wins over `MLLM_INFERENCE_ADDR`; the ready line names the address
/// bound.
#[test]
fn listen_moves_the_standalone_inference_listener() {
    let installation = Installation::new();
    let listen = format!("127.0.0.1:{}", free_port());
    let (role, ready) = installation.start_with(None, &["--listen", &listen]);
    assert!(
        ready.contains(&format!("inference listener {listen}")),
        "{ready}"
    );
    std::net::TcpStream::connect(&listen).expect("the --listen address is served");
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(30));
    assert!(status.success(), "{status:?}");
}

/// T37 (design §9): `--no-inference-auth` on a non-loopback bind prints the
/// warning once, before the listener serves; requests then need no key; and
/// `mllm status` repeats it as `inference: unauthenticated on <addr>`.
#[test]
fn an_unauthenticated_exposed_listener_is_announced_and_shown_in_status() {
    let installation = Installation::new();
    let port = free_port();
    let open = format!("0.0.0.0:{port}");
    let mut command = installation.command();
    command.args([
        "start",
        "standalone",
        "--listen",
        &open,
        "--no-inference-auth",
    ]);
    let (role, _) = installation.start_command(None, &mut command);
    let stderr = role.stderr.clone();
    // No key is needed on this run.
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::io::Write::write_all(
        &mut stream,
        b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut reply = String::new();
    std::io::Read::read_to_string(&mut stream, &mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    deploy(&installation);
    let out = installation.cli(&["status", "deployment", "w11-model"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let shown = String::from_utf8_lossy(&out.stderr);
    assert!(
        shown.contains(&format!("inference: unauthenticated on {open}")),
        "{shown}"
    );
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(30));
    assert!(status.success(), "{status:?}");
    let said = stderr_of(&stderr);
    assert_eq!(
        said.matches(&format!(
            "WARNING: the inference endpoint on {open} accepts requests without an API key."
        ))
        .count(),
        1,
        "{said}"
    );
    assert!(
        said.contains("Anyone who can reach this address can use your models and GPU."),
        "{said}"
    );
}

/// T37 (design §9): without a key on loopback, and with the key anywhere,
/// the role says nothing; `MLLM_INFERENCE_AUTH` turns the key off as the
/// flag does, and a malformed value refuses the start.
#[test]
fn a_loopback_or_keyed_listener_is_not_announced() {
    let installation = Installation::new();
    let standalone = |line: &str| line.starts_with("standalone ready");
    let mut command = installation.command();
    command.args(["start", "standalone", "--no-inference-auth"]);
    let (_, said) = run_until_ready(&mut command, standalone);
    assert!(!said.contains("WARNING"), "{said}");
    let mut command = installation.command();
    command
        .args(["start", "standalone"])
        .env("MLLM_INFERENCE_AUTH", "none");
    let (_, said) = run_until_ready(&mut command, standalone);
    assert!(!said.contains("WARNING"), "{said}");
    let out = output_within(
        installation
            .command()
            .args(["start", "standalone"])
            .env("MLLM_INFERENCE_AUTH", "off"),
        REFUSAL_BOUND,
    );
    assert!(!out.status.success());
    let refused = String::from_utf8_lossy(&out.stderr);
    assert!(refused.contains("MLLM_INFERENCE_AUTH"), "{refused}");
}

/// T01: drain is action-first and names its resource; stopping a role
/// has no verb (SPEC §14), so `stop standalone` is not a command.
#[test]
fn drain_grammar_is_action_first() {
    use mllm_cli::grammar::{parse, Command as Parsed};
    assert_eq!(
        parse(["mllm", "drain", "host", "host-a"]).unwrap(),
        Parsed::Drain {
            host: Some("host-a".into()),
            wait: false,
        }
    );
    // Owner decision 4: `--wait` waits for an offline host's pending Stops.
    assert_eq!(
        parse(["mllm", "drain", "host", "host-a", "--wait"]).unwrap(),
        Parsed::Drain {
            host: Some("host-a".into()),
            wait: true,
        }
    );
    assert_eq!(
        parse(["mllm", "drain", "standalone"]).unwrap(),
        Parsed::Drain {
            host: None,
            wait: false,
        }
    );
    assert!(parse(["mllm", "stop", "standalone"]).is_err());
    assert!(parse(["mllm", "stop", "server"]).is_err());
    assert!(parse(["mllm", "drain", "host"]).is_err());
    assert!(mllm_cli::grammar::parse_invocation([
        "mllm",
        "drain",
        "standalone",
        "--request-id",
        "01K00000000000000000000000"
    ])
    .is_ok());
}

/// A server and one enrolled host on this machine, the host running the fake
/// engine behind its private ingress on loopback.
struct TwoRoles {
    root: tempfile::TempDir,
    server_state: PathBuf,
    host_state: PathBuf,
    server_config: PathBuf,
    host_config: PathBuf,
    inference: String,
}

impl TwoRoles {
    fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().to_path_buf();
        let mut roles = Self {
            server_state: path.join("server"),
            host_state: path.join("host"),
            server_config: path.join("server.yaml"),
            host_config: path.join("host.yaml"),
            inference: String::new(),
            root,
        };
        for (state, role, config) in [
            (&roles.server_state, "server", &roles.server_config),
            (&roles.host_state, "host", &roles.host_config),
        ] {
            let out = roles.cli(state, &["init", role, "--output", config.to_str().unwrap()]);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let mut server: Value =
            serde_json::from_slice(&std::fs::read(&roles.server_config).unwrap()).unwrap();
        let mut inference = String::new();
        for name in ["management", "inference", "bootstrap", "control"] {
            let address = format!("127.0.0.1:{}", free_port());
            server["listeners"][name]["bind"] = address.clone().into();
            if matches!(name, "bootstrap" | "control") {
                server["enrollment"][format!("{name}_address")] =
                    format!("https://{address}").into();
            }
            if name == "inference" {
                inference = address;
            }
        }
        // T17: the drain bound is role configuration (`shutdown.drain_timeout`).
        server["shutdown"] = json!({"drain_timeout": "20s"});
        std::fs::write(&roles.server_config, serde_json::to_vec(&server).unwrap()).unwrap();

        // The host document: the golden vLLM host shape, pointed at the fake
        // engine, one leased port, a loopback private ingress and a budget any
        // test machine can satisfy.
        let engine = path.join("engine");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(
            engine.join("vllm"),
            format!("#!{}\n{FAKE_VLLM}", python3().display()),
        )
        .unwrap();
        std::fs::set_permissions(engine.join("vllm"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let runtime = path.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(runtime.join("mllm_vllm_guard.py"), "# test guard\n").unwrap();
        protected_entry(&engine, &engine.join("vllm"), &runtime);
        let models = path.join("models");
        std::fs::create_dir_all(models.join("toy")).unwrap();
        std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o700)).unwrap();
        let golden: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let template: Value =
            serde_json::from_slice(&std::fs::read(&roles.host_config).unwrap()).unwrap();
        let mut host = golden["input"]["host"].clone();
        host["name"] = "w11-host".into();
        host["state_dir"] = template["state_dir"].clone();
        host["identity_dir"] = template["identity_dir"].clone();
        host["runtime_dir"] = json!(runtime);
        host["model_store"]["path"] = json!(models);
        let port = free_port();
        host["resource_policy"]["endpoint_port_range"] = json!({"start": port, "end": port});
        host["resource_policy"]["queue"]["request_deadline"] = json!("900s");
        host["resource_policy"]["domains"]["unified"] = json!({
            "free_reserve": "16MiB", "host_kv_limit": "64MiB", "managed_limit": "1GiB",
            "memory": "unified", "parked_limit": "256MiB"
        });
        let profile = &mut host["runtime_profiles"]["local"];
        profile["executable"] = json!(engine.join("vllm"));
        profile["security"]["deep_park"] = json!("disabled");
        let ingress = format!("127.0.0.1:{}", free_port());
        host["ingress"] = json!({
            "transport": "trusted_private_link",
            "address": format!("http://{ingress}"),
            "bind": ingress,
        });
        host["shutdown"] = json!({"drain_timeout": "20s"});
        std::fs::write(&roles.host_config, serde_json::to_vec(&host).unwrap()).unwrap();
        roles.inference = inference;
        roles
    }

    fn command(&self, state: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mllm"));
        command
            .env("MLLM_STATE_DIR", state)
            .env_remove("MLLM_VLLM_BIN")
            .env_remove("MLLM_SGLANG_BIN");
        command
    }

    fn cli(&self, state: &Path, args: &[&str]) -> std::process::Output {
        self.command(state).args(args).output().unwrap()
    }

    /// A management command against the server context.
    fn manage(&self, args: &[&str]) -> std::process::Output {
        let mut all = args.to_vec();
        all.extend(["--config", self.server_config.to_str().unwrap()]);
        self.cli(&self.server_state, &all)
    }

    fn start(&self, role: &str) -> Role {
        let (state, config) = match role {
            "server" => (&self.server_state, &self.server_config),
            _ => (&self.host_state, &self.host_config),
        };
        Role::spawn(
            self.command(state)
                .args(["start", role, "--config", config.to_str().unwrap()]),
        )
    }

    fn hosts(&self, online: bool, expected: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let out = self.manage(&["list", "hosts", "--format", "json"]);
            if out.status.success() {
                let value: Value = serde_json::from_slice(&out.stdout).unwrap();
                if value["hosts"].as_array().is_some_and(|hosts| {
                    hosts.len() == expected && hosts.iter().all(|h| h["online"] == online)
                }) {
                    return value;
                }
            }
            assert!(
                Instant::now() < deadline,
                "host connectivity never became {online}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn api_key(&self) -> String {
        let credentials: Value = serde_json::from_slice(
            &std::fs::read(self.server_state.join("identity/server-credentials.json")).unwrap(),
        )
        .unwrap();
        credentials["api_key"].as_str().unwrap().to_owned()
    }

    fn launches(&self) -> Vec<i32> {
        std::fs::read_to_string(self.root.path().join("engine/launches.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }

    fn chat(&self, content: &str, stream: bool) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", self.inference))
            .bearer_auth(self.api_key())
            .json(&json!({
                "model": "toy",
                "stream": stream,
                "messages": [{"role": "user", "content": content}]
            }))
    }

    async fn served_within(&self, within: Duration) {
        let started = Instant::now();
        loop {
            if let Ok(response) = self.chat("hello", false).send().await {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if status == 200 {
                    assert!(body.contains("fake-vllm-answer"), "{body}");
                    return;
                }
            }
            assert!(
                started.elapsed() < within,
                "inference was not served within {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for TwoRoles {
    fn drop(&mut self) {
        for pid in self.launches() {
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

fn final_report(role: Role, name: &str, within: Duration) -> Value {
    let deadline = Instant::now() + within;
    let mut role = role;
    let status = loop {
        if let Some(status) = role.child.child().try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the {name} role did not exit within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success(), "a signalled {name} exits 0: {status:?}");
    role.lines
        .try_iter()
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find(|value| value["role"] == name && value["stopped"] == true)
        .unwrap_or_else(|| panic!("the {name} printed no shutdown report"))
}

/// T01, T10, T17, T18, T33, T38: the two-host control plane on one machine.
/// SIGTERM to the host drains its ingress and leaves the engine running; the
/// restarted host re-proves it and serves again with no relaunch. SIGTERM to the
/// server drains inference (a new request gets 503 while an admitted stream
/// finishes) and the restarted server adopts and re-proves the remote engine.
/// `mllm drain host` then stops the engine with verified cleanup through the
/// server, leaving the deployment eligible for on-demand activation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_signals_restart_and_drain_host_stops_with_cleanup() {
    let roles = TwoRoles::new();
    let server = roles.start("server");
    roles.hosts(false, 0);
    let invitation = roles.root.path().join("host.join");
    let out = roles.manage(&[
        "invite",
        "host",
        "--name",
        "w11-host",
        "--output",
        invitation.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = roles.cli(
        &roles.host_state,
        &[
            "join",
            "host",
            "--join-file",
            invitation.to_str().unwrap(),
            "--config",
            roles.host_config.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let host = roles.start("host");
    let listed = roles.hosts(true, 1);
    let host_id = listed["hosts"][0]["host_id"].as_str().unwrap().to_owned();

    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut deployment = golden["input"]["deployment"].clone();
    deployment["host"] = host_id.clone().into();
    deployment["runtime_profile_revision"] =
        golden["input"]["host"]["runtime_profiles"]["local"]["revision"].clone();
    // The shape the U5 live run deployed with: a local model source under the
    // host's model store.
    deployment["model"] = json!({
        "source": {"type": "local", "path": roles.root.path().join("models/toy")},
        "content_fingerprint": "sha256:toy",
        "revision": "r1"
    });
    deployment["residency"] = json!("restart_only");
    // ADR 0014 §2: the KV cache is the deployment's, inside its Ready phase.
    deployment["engine_config"] = json!({"memory": {"kv_cache": "64MiB"}});
    // The CLI schedules its start up to 900 s ahead, so the deployment's own
    // bound, and the host ceiling above it, must allow that (U5 shape).
    deployment["request_deadline"] = json!("900s");
    for (phase, bytes) in [
        ("cold", "256MiB"),
        ("ready", "128MiB"),
        ("parking", "128MiB"),
        ("parked", "32MiB"),
        ("wake", "256MiB"),
    ] {
        deployment["resources"][phase]["allocations"][0]["bytes"] = json!(bytes);
        deployment["resources"][phase]["allocations"][0]["host_kv_bytes"] = json!("0B");
    }
    let file = roles.root.path().join("deployment.json");
    std::fs::write(&file, deployment.to_string()).unwrap();
    let out = roles.manage(&[
        "deploy",
        "model",
        "--file",
        file.to_str().unwrap(),
        "--activate",
        "--wait",
    ]);
    assert!(
        out.status.success(),
        "deploy: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deployed: Value = serde_json::from_slice(&out.stdout).unwrap();
    let deployment_id = deployed["deployment"]["id"].as_str().unwrap().to_owned();
    roles.served_within(Duration::from_secs(20)).await;
    let engine = roles.launches();
    assert_eq!(engine.len(), 1);
    let engine = engine[0];

    // Host restart: an admitted stream finishes through the draining ingress,
    // and the engine outlives the host role.
    let mut streaming = roles.chat("slow", true).send().await.unwrap();
    assert_eq!(streaming.status(), 200);
    streaming.chunk().await.unwrap().unwrap();
    host.signal();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Phase B follow-up (T17 T18 T38): the host told the controller first, so a
    // new request while the host drains is a retryable 503, never a 500.
    let refused = roles.chat("hello", false).send().await.unwrap();
    let refused_status = refused.status();
    let refusal: Value = refused.json().await.unwrap_or(Value::Null);
    let mut rest = Vec::new();
    while let Some(chunk) = streaming.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert!(
        String::from_utf8_lossy(&rest).contains("[DONE]"),
        "the host drained the stream"
    );
    let report = final_report(host, "host", Duration::from_secs(25));
    assert_eq!(report["engines"], "retained", "{report}");
    assert_eq!(report["drain"]["drained"], true, "{report}");
    assert_eq!(report["dispatch_suspension"], "acknowledged", "{report}");
    assert_eq!(refused_status, 503, "{refusal} {report}");
    assert_eq!(refusal["retryable"], true, "{refusal}");
    assert!(alive(engine), "the engine outlives the host restart");
    roles.hosts(false, 1);
    let host = roles.start("host");
    roles.hosts(true, 1);
    roles.served_within(Duration::from_secs(30)).await;
    assert_eq!(
        roles.launches(),
        vec![engine],
        "re-attached, not relaunched"
    );

    // Server restart: admission closes with a retryable 503 while an admitted
    // stream finishes; the restarted server adopts and re-proves the engine.
    let mut streaming = roles.chat("slow", true).send().await.unwrap();
    assert_eq!(streaming.status(), 200);
    streaming.chunk().await.unwrap().unwrap();
    server.signal();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = roles.chat("hello", false).send().await.unwrap();
    assert_eq!(refused.status(), 503);
    let refusal: Value = refused.json().await.unwrap();
    assert_eq!(refusal["error"]["code"], "shutting_down");
    let mut rest = Vec::new();
    while let Some(chunk) = streaming.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert!(
        String::from_utf8_lossy(&rest).contains("[DONE]"),
        "the server drained the stream"
    );
    let report = final_report(server, "server", Duration::from_secs(25));
    assert_eq!(report["engines"], "retained", "{report}");
    assert_eq!(report["drain"]["in_flight_at_close"], 1, "{report}");
    assert_eq!(report["drain"]["drained"], true, "{report}");
    assert!(alive(engine), "the engine outlives the server restart");
    let server = roles.start("server");
    roles.hosts(true, 1);
    roles.served_within(Duration::from_secs(30)).await;
    assert_eq!(roles.launches(), vec![engine], "adopted, not relaunched");

    // SPEC §4.3: the explicit drain of the host, by name, through the server.
    let out = roles.manage(&["drain", "host", "w11-host"]);
    assert!(
        out.status.success(),
        "drain: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let drained: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(drained["drained"], true, "{drained}");
    assert_eq!(drained["host"], host_id.as_str(), "{drained}");
    let stopped = &drained["deployments"][0];
    assert_eq!(
        stopped["deployment_id"],
        deployment_id.as_str(),
        "{drained}"
    );
    assert_eq!(stopped["cleanup"], "verified", "{drained}");
    assert_eq!(stopped["observed_state"], "stopped", "{drained}");
    // T10: a drain is not an operator stop.
    assert_eq!(stopped["suspended"], false, "{drained}");
    assert!(!alive(engine), "the drained engine is gone");
    let unknown = roles.manage(&["drain", "host", "no-such-host"]);
    assert!(!unknown.status.success());

    host.signal();
    final_report(host, "host", Duration::from_secs(25));
    server.signal();
    final_report(server, "server", Duration::from_secs(25));
    // SPEC §10 (T17 T19): every dispatch wrote a durable lease and every one was
    // closed on evidence (completion, refusal before forwarding, or the drain's
    // verified cleanup); none is left charged.
    let store = mllm_store::Store::open(&roles.server_state.join("srv.sqlite3")).unwrap();
    assert!(
        store.pending_dispatches(&deployment_id).unwrap().is_empty(),
        "request leases left behind"
    );
}

/// T03 (owner decision 2026-09-25): a standalone start honours the generic
/// overrides of a document-only setting, `--set` over `MLLM_SET__…` over the
/// document: the drain bound it reports at shutdown is the overridden one.
/// Fake engine only; not qualification (SPEC §18).
#[test]
fn a_standalone_start_takes_its_drain_bound_from_set_then_env_then_document() {
    let installation = Installation::new();
    let standalone = |line: &str| line.starts_with("standalone ready");
    let bound_after = |command: &mut Command| {
        let role = Role::spawn(command);
        role.expect_line(standalone, Duration::from_secs(60));
        role.signal();
        let (status, report) = role.exit(Duration::from_secs(30));
        assert!(status.success(), "{status:?}");
        report["drain_bound_secs"].clone()
    };
    let mut command = installation.command();
    command.args(["start", "standalone"]);
    assert_eq!(bound_after(&mut command), json!(30));
    let mut command = installation.command();
    command
        .args(["start", "standalone"])
        .env("MLLM_SET__SHUTDOWN__DRAIN_TIMEOUT", "6s");
    assert_eq!(bound_after(&mut command), json!(6));
    let mut command = installation.command();
    command
        .args(["start", "standalone", "--set", "shutdown.drain_timeout=7s"])
        .env("MLLM_SET__SHUTDOWN__DRAIN_TIMEOUT", "6s");
    assert_eq!(bound_after(&mut command), json!(7));
}

/// T37 (design §9 "Where the key is", final review I11): each role's ready
/// line names the owner-only credentials file that holds the API key, and
/// never carries the key itself.
#[test]
fn the_ready_lines_name_the_credentials_file_never_the_key() {
    let installation = Installation::new();
    let mut command = installation.command();
    command.args(["start", "standalone"]);
    let (ready, _) = run_until_ready(&mut command, |line| line.starts_with("standalone ready"));
    let credentials = installation.state().join("identity/credentials");
    assert!(
        ready.contains(&format!("credentials {}", credentials.display())),
        "{ready}"
    );
    assert!(!ready.contains(&installation.api_key()), "{ready}");

    let (_root, state, config) = server_installation(&format!("127.0.0.1:{}", free_port()));
    let mut command = server_command(&state);
    command.args(["start", "server", "--config", config.to_str().unwrap()]);
    let (banner, _) = run_until_ready(&mut command, |line| line.contains("\"role\":\"server\""));
    let banner: Value = serde_json::from_str(&banner).unwrap();
    let path = banner["credentials"]
        .as_str()
        .expect("the credentials path");
    assert!(path.ends_with("server-credentials.json"), "{banner}");
    let stored: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let key = stored["api_key"].as_str().unwrap();
    assert!(!banner.to_string().contains(key), "{banner}");
}

/// T03 (final review I8, owner rule: every setting three ways, standalone is
/// a server and a host in one process): the server's management address is
/// `--management-listen` > `MLLM_MANAGEMENT_ADDR` > the document, as for
/// standalone; the role records the address it serves on, so a client
/// command finds a role started with the flag without the variable; and a
/// named document whose state_dir disagrees with a named state root is
/// refused instead of moving the role's state.
#[test]
fn the_server_management_address_and_state_root_follow_the_shared_rule() {
    let (_root, state, config) = server_installation(&format!("127.0.0.1:{}", free_port()));
    let banner = |line: &str| line.contains("\"role\":\"server\"");
    let flag = format!("127.0.0.1:{}", free_port());
    let environment = format!("127.0.0.1:{}", free_port());
    let start = || {
        let mut command = server_command(&state);
        command.args(["start", "server", "--config", config.to_str().unwrap()]);
        command
    };
    let mut command = start();
    command
        .args(["--management-listen", &flag])
        .env("MLLM_MANAGEMENT_ADDR", &environment);
    let role = Role::spawn(&mut command);
    let line = role.expect_line(banner, Duration::from_secs(60));
    let banner_value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(banner_value["management"], flag.as_str(), "{line}");
    assert_eq!(
        std::fs::read_to_string(state.join("run/management-address"))
            .unwrap()
            .trim(),
        flag
    );
    // A client with no variable finds it through the recorded address.
    let listed = server_command(&state)
        .env_remove("MLLM_MANAGEMENT_ADDR")
        .args([
            "list",
            "hosts",
            "--config",
            config.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(30));
    assert!(status.success(), "{status:?}");

    // The variable over the document.
    let mut command = start();
    command.env("MLLM_MANAGEMENT_ADDR", &environment);
    let (line, _) = run_until_ready(&mut command, banner);
    let banner_value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(banner_value["management"], environment.as_str(), "{line}");

    // A state root that disagrees with the named document is refused.
    let elsewhere = state.parent().unwrap().join("elsewhere");
    let refused = server_command(&elsewhere)
        .args(["start", "server", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("disagrees with the state root"), "{said}");
}

/// T03 (final review I8): a standalone role started with
/// `--management-listen` is found by a client command that names neither
/// the flag nor the variable, through the address the role recorded.
#[test]
fn a_client_finds_a_standalone_started_with_management_listen() {
    let installation = Installation::new();
    let other = format!("127.0.0.1:{}", free_port());
    let mut command = installation.command();
    command.args(["start", "standalone", "--management-listen", &other]);
    let role = Role::spawn(&mut command);
    role.expect_line(
        |line| line.starts_with("standalone ready"),
        Duration::from_secs(60),
    );
    let listed = installation
        .command()
        .env_remove("MLLM_MANAGEMENT_ADDR")
        .args(["list", "deployments", "--json"])
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    role.signal();
    let (status, _) = role.exit(Duration::from_secs(30));
    assert!(status.success(), "{status:?}");
}
