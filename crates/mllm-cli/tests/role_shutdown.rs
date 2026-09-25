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
use std::sync::mpsc;
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

/// A per-installation engine port range (`MLLM_STANDALONE_ENGINE_PORTS`):
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
            .env("MLLM_STANDALONE_INFERENCE_ADDR", &self.inference)
            .env("MLLM_STANDALONE_MANAGEMENT_ADDR", &self.management)
            .env("MLLM_STANDALONE_ENGINE_PORTS", &self.engines);
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
        let mut command = self.command();
        command
            .args(["start", "standalone"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(seconds) = drain_secs {
            self.drain_timeout(&format!("{seconds}s"));
        }
        // Guard first: a role that never prints its ready line is killed
        // with its process group when the wait below panics.
        let mut child = Guarded::spawn(&mut command);
        let (lines, received) = mpsc::channel();
        let stdout = child.child().stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        let role = Role {
            child,
            lines: received,
        };
        role.expect_line(
            |line| line.starts_with("standalone ready"),
            Duration::from_secs(60),
        );
        role
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
}

impl Role {
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
    // Sized from an explicit capacity, not this machine's (see
    // `support::BINARY_TEST_CAPACITY_BYTES`).
    let capacity = support::BINARY_TEST_CAPACITY_BYTES;
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
        capacity,
        mllm_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        true,
    );
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
    let status = installation.cli(&["status", "deployment", &deployment]);
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
    let status = installation.cli(&["status", "deployment", &deployment]);
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

/// T03: a standalone listener override must stay on loopback (SPEC §16.5).
#[test]
fn a_non_loopback_standalone_listener_is_refused() {
    let installation = Installation::new();
    // A port free now: a regressed refusal would otherwise bind a fixed
    // public port, and serve until the bound below fails the test.
    let out = output_within(
        installation.command().args(["start", "standalone"]).env(
            "MLLM_STANDALONE_INFERENCE_ADDR",
            format!("0.0.0.0:{}", free_port()),
        ),
        REFUSAL_BOUND,
    );
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("MLLM_STANDALONE_INFERENCE_ADDR"), "{said}");
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
        let mut child = Guarded::spawn(
            self.command(state)
                .args(["start", role, "--config", config.to_str().unwrap()])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit()),
        );
        let (lines, received) = mpsc::channel();
        let stdout = child.child().stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        Role {
            child,
            lines: received,
        }
    }

    fn hosts(&self, online: bool, expected: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let out = self.manage(&["list", "hosts", "--output", "json"]);
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
