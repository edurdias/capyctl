//! SPEC §13.2 (W13, G08): an owned engine that exits while Ready is detected
//! promptly, its dispatch closes, it is settled with verified cleanup (the rest
//! of its recorded group is terminated and proven gone before anything is
//! released), status reads `failed`, and the next request relaunches it on
//! demand. Driven through the real `mllm` binary, standalone and remote.
//!
//! The engine is a fake vLLM executable (a small Python HTTP server with one
//! forked worker in its process group). CPU and fake-engine tests are not
//! qualification: passing these never shows a native engine recipe works on
//! either Spark (SPEC §18).

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mllm_config::effective::{Engine, ModelSource};
use serde_json::{json, Value};

mod support;
use support::process::{free_port, free_ports, Guarded};

/// A vLLM stand-in: `serve <model> --port P --served-model-name R`, the key only
/// from `VLLM_API_KEY`, every `/v1` route keyed, one forked worker child in the
/// same process group, SSE chat. Each start appends its pid to `launches.log`
/// and its worker's pid to `workers.log`. A chat whose last message is `slower`
/// streams for about 12 s.
const FAKE_VLLM: &str = r#"
import json, os, sys, time, http.server
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
key = os.environ.get("VLLM_API_KEY", "")
with open(os.path.join(here, "launches.log"), "a") as f:
    f.write(str(os.getpid()) + "\n")
port = int(args[args.index("--port") + 1])
served = args[args.index("--served-model-name") + 1]
worker = os.fork()
if worker == 0:
    while True:
        time.sleep(60)
with open(os.path.join(here, "workers.log"), "a") as f:
    f.write(str(worker) + "\n")

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
            body = b"vllm:num_requests_running 0\nvllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 0.0\n"
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
        last = (body.get("messages") or [{}])[-1].get("content")
        count = {"slower": 120}.get(last, 1)
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

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

fn pids(file: &Path) -> Vec<i32> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

/// The fake engine, mllm's protected vLLM entry that runs it, and the guard.
fn engine_files(root: &Path, runtime_mode: u32) -> (PathBuf, PathBuf) {
    let bin = root.join("engine");
    std::fs::create_dir_all(&bin).unwrap();
    let engine = bin.join("vllm");
    std::fs::write(&engine, format!("#!{}\n{FAKE_VLLM}", python3().display())).unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(python3(), bin.join("python3")).unwrap();
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(runtime.join("mllm_vllm_guard.py"), "# test guard\n").unwrap();
    std::fs::write(
        runtime.join("vllm_entry.py"),
        format!(
            "import runpy\nrunpy.run_path({:?}, run_name=\"__main__\")\n",
            engine.display().to_string()
        ),
    )
    .unwrap();
    for module in ["vllm_entry.py", "mllm_vllm_guard.py"] {
        std::fs::set_permissions(runtime.join(module), std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(runtime_mode)).unwrap();
    std::fs::create_dir_all(root.join("models/toy")).unwrap();
    (engine, runtime)
}

/// A running role. Its guard is built before anything can fail, so a role
/// that never prints its ready line is still killed (with its process group)
/// when the wait panics.
struct Role {
    _child: Guarded,
    lines: mpsc::Receiver<String>,
}

impl Role {
    fn spawn(mut command: Command, ready: Option<&str>) -> Self {
        let mut child = Guarded::spawn(command.stdout(Stdio::piped()).stderr(Stdio::inherit()));
        let (lines, received) = mpsc::channel();
        let stdout = child.child().stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        let role = Self {
            _child: child,
            lines: received,
        };
        if let Some(ready) = ready {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match role.lines.recv_timeout(left) {
                    Ok(line) if line.starts_with(ready) => break,
                    Ok(_) => {}
                    Err(error) => panic!("the role never printed {ready:?}: {error}"),
                }
            }
        }
        role
    }
}

/// Read the whole remaining stream; whether the engine finished it.
async fn finished(mut streaming: reqwest::Response) -> bool {
    let mut rest = Vec::new();
    while let Ok(Some(chunk)) = streaming.chunk().await {
        rest.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&rest).contains("[DONE]")
}

/// Poll `status` until `want` holds, returning the status and how long it took.
fn status_until(
    status: impl Fn() -> Value,
    want: impl Fn(&str) -> bool,
    within: Duration,
) -> (Value, Duration) {
    let started = Instant::now();
    loop {
        let value = status();
        if value["observed_state"].as_str().is_some_and(&want) {
            return (value, started.elapsed());
        }
        assert!(
            started.elapsed() < within,
            "status never matched within {within:?}: {value}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_gone(pid: i32, within: Duration) {
    let deadline = Instant::now() + within;
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still alive");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------- standalone

struct Installation {
    root: tempfile::TempDir,
    inference: String,
    management: String,
    engines: String,
}

impl Installation {
    fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        engine_files(root.path(), 0o755);
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

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mllm"));
        command
            .env("MLLM_STATE_DIR", self.state())
            .env("MLLM_VLLM_BIN", self.root.path().join("engine/vllm"))
            .env_remove("MLLM_SGLANG_BIN")
            .env("MLLM_MODELS_ROOT", self.root.path().join("models"))
            .env("MLLM_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("MLLM_ENGINE_FINGERPRINT", "fake-vllm-w13")
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

    fn launches(&self) -> Vec<i32> {
        pids(&self.root.path().join("engine/launches.log"))
    }

    fn workers(&self) -> Vec<i32> {
        pids(&self.root.path().join("engine/workers.log"))
    }

    fn api_key(&self) -> String {
        std::fs::read_to_string(self.state().join("identity/credentials"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("api_key: "))
            .unwrap()
            .to_owned()
    }

    fn chat(&self, content: &str, stream: bool) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", self.inference))
            .bearer_auth(self.api_key())
            .json(&json!({
                "model": "w13-model",
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
            assert!(started.elapsed() < within, "not served within {within:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn status(&self, deployment: &str) -> Value {
        let out = self.cli(&["status", "deployment", deployment]);
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    fn deploy(&self) -> String {
        // Sized from an explicit capacity, not this machine's (see
        // `support::BINARY_TEST_CAPACITY_BYTES`).
        let capacity = support::BINARY_TEST_CAPACITY_BYTES;
        let document = mllm_cli::standalone_config::deployment_document(
            "w13-model",
            "w13-model",
            &ModelSource::Local {
                path: self
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
            "local",
        );
        let file = self.root.path().join("deployment.json");
        std::fs::write(&file, document.to_string()).unwrap();
        let out = self.cli(&[
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
        deployed["deployment"]["id"].as_str().unwrap().to_owned()
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        for pid in self.launches() {
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
        for pid in self.workers() {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// T20 T30 T32 T33 T38 (W13): SIGKILL of the embedded engine's API process
/// while a stream is in flight. The stream ends without `[DONE]` (no resumable
/// or exactly-once claim), dispatch closes within about a second, the worker the
/// dead engine left behind (a partial group) is terminated and proven gone
/// before the reservation is released, status reads `failed`, the exit is
/// journaled with its signal, no request lease is left charged, and the next
/// request relaunches the deployment on demand.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standalone_engine_exit_is_settled_and_relaunched_on_demand() {
    let installation = Installation::new();
    let _role = Role::spawn(
        {
            let mut command = installation.command();
            command.args(["start", "standalone"]);
            command
        },
        Some("standalone ready"),
    );
    let deployment = installation.deploy();
    installation.served_within(Duration::from_secs(20)).await;
    let engine = installation.launches();
    assert_eq!(engine.len(), 1);
    let (engine, worker) = (engine[0], installation.workers()[0]);

    // T38: a stream in flight when the engine dies.
    let mut streaming = installation.chat("slower", true).send().await.unwrap();
    assert_eq!(streaming.status(), 200);
    streaming.chunk().await.unwrap().unwrap();
    unsafe {
        libc::kill(engine, libc::SIGKILL);
    }
    let (_, closed) = status_until(
        || installation.status(&deployment),
        |state| state != "ready",
        Duration::from_secs(10),
    );
    assert!(
        closed < Duration::from_secs(2),
        "dispatch closed promptly: {closed:?}"
    );
    assert!(
        !finished(streaming).await,
        "the cut stream never claims completion"
    );

    // Settled with verified cleanup: the partial group is terminated first.
    let (status, _) = status_until(
        || installation.status(&deployment),
        |state| state == "failed",
        Duration::from_secs(30),
    );
    assert_eq!(status["suspended"], false, "not an operator stop: {status}");
    wait_gone(worker, Duration::from_secs(5));
    {
        let store =
            mllm_store::Store::open(&installation.state().join("server/srv.sqlite3")).unwrap();
        let evidence = store.journal_evidence_of(&deployment).unwrap();
        assert!(
            evidence
                .iter()
                .any(|entry| entry.contains("exited (signal 9)")),
            "{evidence:?}"
        );
        assert!(
            store.pending_dispatches(&deployment).unwrap().is_empty(),
            "request leases left charged"
        );
        assert!(store.resource_snapshot().unwrap().owners.is_empty());
    }

    // Q5: the next request relaunches it on demand, as a new launch.
    installation.served_within(Duration::from_secs(60)).await;
    let launches = installation.launches();
    assert_eq!(launches.len(), 2, "relaunched on demand: {launches:?}");
    assert!(alive(launches[1]));
    let (status, _) = status_until(
        || installation.status(&deployment),
        |state| state == "ready",
        Duration::from_secs(10),
    );
    assert_eq!(status["observed_state"], "ready");
}

// -------------------------------------------------------------------- remote

/// A server and one enrolled host on this machine.
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
            let out = cli(state, &["init", role, "--output", config.to_str().unwrap()]);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let mut server: Value =
            serde_json::from_slice(&std::fs::read(&roles.server_config).unwrap()).unwrap();
        for name in ["management", "inference", "bootstrap", "control"] {
            let address = format!("127.0.0.1:{}", free_port());
            server["listeners"][name]["bind"] = address.clone().into();
            if matches!(name, "bootstrap" | "control") {
                server["enrollment"][format!("{name}_address")] =
                    format!("https://{address}").into();
            }
            if name == "inference" {
                roles.inference = address;
            }
        }
        server["shutdown"] = json!({"drain_timeout": "10s"});
        std::fs::write(&roles.server_config, serde_json::to_vec(&server).unwrap()).unwrap();

        let (engine, runtime) = engine_files(&path, 0o700);
        let golden: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let template: Value =
            serde_json::from_slice(&std::fs::read(&roles.host_config).unwrap()).unwrap();
        let mut host = golden["input"]["host"].clone();
        host["name"] = "w13-host".into();
        host["state_dir"] = template["state_dir"].clone();
        host["identity_dir"] = template["identity_dir"].clone();
        host["runtime_dir"] = json!(runtime);
        host["model_store"]["path"] = json!(path.join("models"));
        let port = free_port();
        host["resource_policy"]["endpoint_port_range"] = json!({"start": port, "end": port});
        host["resource_policy"]["queue"]["request_deadline"] = json!("900s");
        host["resource_policy"]["domains"]["unified"] = json!({
            "free_reserve": "16MiB", "host_kv_limit": "64MiB", "managed_limit": "1GiB",
            "memory": "unified", "parked_limit": "256MiB"
        });
        let profile = &mut host["runtime_profiles"]["local"];
        profile["executable"] = json!(engine);
        profile["security"]["deep_park"] = json!("disabled");
        let ingress = format!("127.0.0.1:{}", free_port());
        host["ingress"] = json!({
            "transport": "trusted_private_link",
            "address": format!("http://{ingress}"),
            "bind": ingress,
        });
        host["shutdown"] = json!({"drain_timeout": "10s"});
        std::fs::write(&roles.host_config, serde_json::to_vec(&host).unwrap()).unwrap();
        std::fs::set_permissions(path.join("models"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        roles
    }

    fn manage(&self, args: &[&str]) -> std::process::Output {
        let mut all = args.to_vec();
        all.extend(["--config", self.server_config.to_str().unwrap()]);
        cli(&self.server_state, &all)
    }

    fn start(&self, role: &str) -> Role {
        let (state, config) = match role {
            "server" => (&self.server_state, &self.server_config),
            _ => (&self.host_state, &self.host_config),
        };
        let mut command = command(state);
        command.args(["start", role, "--config", config.to_str().unwrap()]);
        Role::spawn(command, None)
    }

    fn hosts(&self, expected: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let out = self.manage(&["list", "hosts", "--output", "json"]);
            if out.status.success() {
                let value: Value = serde_json::from_slice(&out.stdout).unwrap();
                if value["hosts"].as_array().is_some_and(|hosts| {
                    hosts.len() == expected
                        && hosts
                            .iter()
                            .all(|h| h["online"] == true && h["eligible"] == true)
                }) {
                    return value;
                }
            }
            assert!(Instant::now() < deadline, "the host never became eligible");
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
        pids(&self.root.path().join("engine/launches.log"))
    }

    fn workers(&self) -> Vec<i32> {
        pids(&self.root.path().join("engine/workers.log"))
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
            assert!(started.elapsed() < within, "not served within {within:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn status(&self, deployment: &str) -> Value {
        let out = self.manage(&["status", "deployment", deployment, "--output", "json"]);
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
}

impl Drop for TwoRoles {
    fn drop(&mut self) {
        for pid in self.launches() {
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
        for pid in self.workers() {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn command(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mllm"));
    command
        .env("MLLM_STATE_DIR", state)
        .env_remove("MLLM_VLLM_BIN")
        .env_remove("MLLM_SGLANG_BIN");
    command
}

fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    command(state).args(args).output().unwrap()
}

/// T20 T30 T32 T33 T38 (W13): SIGKILL of a remote engine's API process while a
/// stream is in flight. The host agent reports the exit on its control session
/// with the signal its launcher reaped; the server closes that instance's
/// dispatch within about a second, the stream ends without `[DONE]`, the host's
/// Terminate kills the worker the engine left behind and proves the whole
/// recorded group gone before anything is released, status reads `failed`, and
/// the next request relaunches the deployment on demand on the same host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_engine_exit_is_reported_settled_and_relaunched_on_demand() {
    let roles = TwoRoles::new();
    let _server = roles.start("server");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !roles
        .manage(&["list", "hosts", "--output", "json"])
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "the server never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
    let invitation = roles.root.path().join("host.join");
    let out = roles.manage(&[
        "invite",
        "host",
        "--name",
        "w13-host",
        "--output",
        invitation.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = cli(
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
    let _host = roles.start("host");
    let listed = roles.hosts(1);
    let host_id = listed["hosts"][0]["host_id"].as_str().unwrap().to_owned();

    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut deployment = golden["input"]["deployment"].clone();
    deployment["host"] = host_id.clone().into();
    deployment["runtime_profile_revision"] =
        golden["input"]["host"]["runtime_profiles"]["local"]["revision"].clone();
    deployment["model"] = json!({
        "source": {"type": "local", "path": roles.root.path().join("models/toy")},
        "content_fingerprint": "sha256:toy",
        "revision": "r1"
    });
    deployment["residency"] = json!("restart_only");
    deployment["engine_config"] = json!({"memory": {"kv_cache": "64MiB"}});
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
    let deployment = deployed["deployment"]["id"].as_str().unwrap().to_owned();
    roles.served_within(Duration::from_secs(30)).await;
    let engine = roles.launches();
    assert_eq!(engine.len(), 1);
    let (engine, worker) = (engine[0], roles.workers()[0]);

    // T38: a stream in flight through the host's ingress when the engine dies.
    let mut streaming = roles.chat("slower", true).send().await.unwrap();
    assert_eq!(streaming.status(), 200);
    streaming.chunk().await.unwrap().unwrap();
    unsafe {
        libc::kill(engine, libc::SIGKILL);
    }
    let (_, closed) = status_until(
        || roles.status(&deployment),
        |state| state != "ready",
        Duration::from_secs(10),
    );
    assert!(
        closed < Duration::from_secs(2),
        "dispatch closed promptly: {closed:?}"
    );
    assert!(
        !finished(streaming).await,
        "the cut stream never claims completion"
    );

    // T32: the host's Terminate kills the worker left behind and proves the
    // whole recorded group gone; only then is anything released.
    let (status, _) = status_until(
        || roles.status(&deployment),
        |state| state == "failed",
        Duration::from_secs(40),
    );
    assert_eq!(status["suspended"], false, "not an operator stop: {status}");
    wait_gone(worker, Duration::from_secs(5));
    {
        let store = mllm_store::Store::open(&roles.server_state.join("srv.sqlite3")).unwrap();
        let evidence = store.journal_evidence_of(&deployment).unwrap();
        assert!(
            evidence
                .iter()
                .any(|entry| entry.contains("exited (signal 9)")),
            "{evidence:?}"
        );
        assert!(
            store.pending_dispatches(&deployment).unwrap().is_empty(),
            "request leases left charged"
        );
        assert!(store.resource_snapshot().unwrap().owners.is_empty());
    }

    // Q5: the next request relaunches it on demand on the same host.
    roles.served_within(Duration::from_secs(60)).await;
    let launches = roles.launches();
    assert_eq!(launches.len(), 2, "relaunched on demand: {launches:?}");
    assert!(alive(launches[1]));
}
