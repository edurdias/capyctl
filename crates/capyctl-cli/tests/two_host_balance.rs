//! ADR 0013 §10 (unit I3, owner decision D9): one deployment with two
//! instances on two enrolled hosts, driven through the real `capyctl` binary. The
//! server's router balances concurrent requests across both, shifts new work
//! away from an instance whose host agent reports a long engine queue (W8),
//! fails new requests over when a host is lost without replaying the request
//! that host had accepted, and routes to the instance again once its host
//! re-joins and re-proves readiness.
//!
//! CPU and fake-engine tests are not qualification: passing this never shows a
//! native engine recipe works on either Spark (SPEC §18). The live steps that
//! prove the same on real engines are in the I3 hand-off (matrix M54–M60).

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod support;
use support::process::{free_port, Guarded};

/// A vLLM stand-in: `serve <model> --port P --served-model-name R`, the key only
/// from `VLLM_API_KEY`, every `/v1` route keyed, one forked worker child in the
/// same process group. Each start appends its pid to
/// `launches.log` beside it and names its own host in every answer.
const FAKE_VLLM: &str = r#"
import json, os, sys, time, http.server
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
key = os.environ.get("VLLM_API_KEY", "")
with open(os.path.join(here, "launches.log"), "a") as f:
    f.write(str(os.getpid()) + "\n")
with open(os.path.join(here, "host"), "r") as f:
    host = f.read().strip()
port = int(args[args.index("--port") + 1])
def read(name, default):
    try:
        with open(os.path.join(here, name)) as f:
            return f.read().strip() or default
    except OSError:
        return default
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
            running = read("running", "0")
            body = ("vllm:num_requests_running " + running + "\nvllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 0.0\n").encode()
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
        answer = "fake-vllm-answer from " + host
        with open(os.path.join(here, "accepted.log"), "a") as f:
            f.write("1\n")
        time.sleep(float(read("delay", "0")))
        with open(os.path.join(here, "served.log"), "a") as f:
            f.write("1\n")
        if body.get("stream"):
            def chunk(delta, finish):
                return {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": served,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for part in (chunk({"role": "assistant", "content": ""}, None), chunk({"content": answer}, None), chunk({}, "stop")):
                self.wfile.write(("data: " + json.dumps(part) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        self.send_json({"id": "c1", "object": "chat.completion", "created": 1, "model": served,
                        "choices": [{"index": 0, "finish_reason": "stop",
                                     "message": {"role": "assistant", "content": answer}}]})

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

fn private_file(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

/// One host role: its state, its document and its own fake engine.
struct HostRole {
    name: String,
    state: PathBuf,
    config: PathBuf,
    engine: PathBuf,
}

impl HostRole {
    fn count(&self, log: &str) -> usize {
        std::fs::read_to_string(self.engine.join(log))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn served(&self) -> usize {
        self.count("served.log")
    }
    fn accepted(&self) -> usize {
        self.count("accepted.log")
    }
    /// Steer the fake engine: its answer delay or the queue its metrics report.
    fn set(&self, knob: &str, value: &str) {
        std::fs::write(self.engine.join(knob), value).unwrap();
    }
    fn launches(&self) -> Vec<i32> {
        std::fs::read_to_string(self.engine.join("launches.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }
}

/// A server and two enrolled hosts on this machine.
struct Cluster {
    root: tempfile::TempDir,
    server_state: PathBuf,
    server_config: PathBuf,
    inference: String,
    hosts: Vec<HostRole>,
}

impl Cluster {
    fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().to_path_buf();
        let server_state = path.join("server");
        let server_config = path.join("server.yaml");
        let out = cli(
            &server_state,
            &[
                "init",
                "server",
                "--output",
                server_config.to_str().unwrap(),
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut server: Value =
            serde_json::from_slice(&std::fs::read(&server_config).unwrap()).unwrap();
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
        server["shutdown"] = json!({"drain_timeout": "10s"});
        // Owner decision 2026-09-23: suspend a silent host after 5 s; lose its
        // session after 12 s, short enough to test the agent's own lost bound.
        server["control"] = json!({"heartbeat_suspend_after": "5s", "heartbeat_lost_after": "12s"});
        std::fs::write(&server_config, serde_json::to_vec(&server).unwrap()).unwrap();
        let models = path.join("models");
        std::fs::create_dir_all(models.join("toy")).unwrap();
        std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o700)).unwrap();
        let golden: Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let hosts = [("i3-spark-a", "a"), ("i3-spark-b", "b")]
            .into_iter()
            .map(|(name, zone)| {
                let state = path.join(name);
                let config = path.join(format!("{name}.yaml"));
                let out = cli(
                    &state,
                    &["init", "host", "--output", config.to_str().unwrap()],
                );
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let engine = path.join(format!("{name}-engine"));
                std::fs::create_dir_all(&engine).unwrap();
                capyctl_config::test_support::write_executable(
                    &engine.join("vllm"),
                    format!("#!{}\n{FAKE_VLLM}", python3().display()),
                    0o755,
                )
                .unwrap();
                std::fs::write(engine.join("host"), name).unwrap();
                std::os::unix::fs::symlink(python3(), engine.join("python3")).unwrap();
                // SPEC §13.3: the runtime directory the host trusts is its own
                // and writable by it alone.
                let runtime = path.join(format!("{name}-runtime"));
                std::fs::create_dir_all(&runtime).unwrap();
                std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
                private_file(&runtime.join("capyctl_vllm_guard.py"), "# test guard\n");
                private_file(
                    &runtime.join("vllm_entry.py"),
                    &format!(
                        "import runpy\nrunpy.run_path({:?}, run_name=\"__main__\")\n",
                        engine.join("vllm").display().to_string()
                    ),
                );
                let template: Value =
                    serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
                let mut host = golden["input"]["host"].clone();
                host["name"] = name.into();
                host["state_dir"] = template["state_dir"].clone();
                host["identity_dir"] = template["identity_dir"].clone();
                host["runtime_dir"] = json!(runtime);
                host["model_store"]["path"] = json!(models);
                let port = free_port();
                host["resource_policy"]["endpoint_port_range"] =
                    json!({"start": port, "end": port});
                host["resource_policy"]["queue"]["request_deadline"] = json!("900s");
                host["resource_policy"]["domains"]["unified"] = json!({
                    "free_reserve": "16MiB", "host_kv_limit": "64MiB", "managed_limit": "1GiB",
                    "memory": "unified", "parked_limit": "256MiB"
                });
                // ADR 0013 §2: labels are host policy the selector matches.
                host["resource_policy"]["labels"] = json!({"gpu": "gb10", "zone": zone});
                let profile = &mut host["runtime_profiles"]["local"];
                profile["executable"] = json!(engine.join("vllm"));
                profile["security"]["deep_park"] = json!("disabled");
                let ingress = format!("127.0.0.1:{}", free_port());
                host["ingress"] = json!({
                    "transport": "trusted_private_link",
                    "address": format!("http://{ingress}"),
                    "bind": ingress,
                });
                host["shutdown"] = json!({"drain_timeout": "10s"});
                std::fs::write(&config, serde_json::to_vec(&host).unwrap()).unwrap();
                HostRole {
                    name: name.into(),
                    state,
                    config,
                    engine,
                }
            })
            .collect();
        Self {
            root,
            server_state,
            server_config,
            inference,
            hosts,
        }
    }

    fn manage(&self, args: &[&str]) -> std::process::Output {
        let mut all = args.to_vec();
        all.extend(["--config", self.server_config.to_str().unwrap()]);
        cli(&self.server_state, &all)
    }

    fn manage_json(&self, args: &[&str]) -> Value {
        let out = self.manage(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn start(&self, role: &str, state: &Path, config: &Path) -> Role {
        // Each role's log stays beside its state for a failure to be read.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state.with_extension("log"))
            .unwrap();
        // Guard first, in its own process group (support::process).
        let mut child = Guarded::spawn(
            command(state)
                .args(["start", role, "--config", config.to_str().unwrap()])
                .stdout(Stdio::piped())
                .stderr(Stdio::from(log)),
        );
        let stdout = child.child().stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                drop(line);
            }
        });
        Role(child)
    }

    /// Wait until `expected` hosts are online and eligible (W12).
    fn eligible_hosts(&self, expected: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let out = self.manage(&["list", "hosts", "--format", "json"]);
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
            assert!(Instant::now() < deadline, "hosts never became eligible");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn credential(&self, field: &str) -> String {
        let credentials: Value = serde_json::from_slice(
            &std::fs::read(self.server_state.join("identity/server-credentials.json")).unwrap(),
        )
        .unwrap();
        credentials[field].as_str().unwrap().to_owned()
    }

    fn api_key(&self) -> String {
        self.credential("api_key")
    }

    async fn chat(&self) -> (u16, String) {
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", self.inference))
            .bearer_auth(self.api_key())
            .json(&json!({"model": "toy", "messages": [{"role": "user", "content": "hello"}]}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap_or_default())
    }

    async fn served(&self, within: Duration) -> String {
        let started = Instant::now();
        loop {
            let (status, body) = self.chat().await;
            if status == 200 {
                assert!(body.contains("fake-vllm-answer"), "{body}");
                return body;
            }
            assert!(
                started.elapsed() < within,
                "not served within {within:?}: {status} {body}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Poll deployment status until `ready` instances are Ready.
    fn ready_instances(&self, deployment: &str, ready: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status =
                self.manage_json(&["status", "deployment", deployment, "--format", "json"]);
            if status["ready_instances"] == json!(ready) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "never {ready} ready instances: {status}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for host in &self.hosts {
            for pid in host.launches() {
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
        }
        let _ = &self.root;
    }
}

/// A running role; dropping it kills its process group (support::process).
struct Role(Guarded);
impl Role {
    fn stop(mut self) {
        self.0.signal(libc::SIGTERM);
        self.0.exit_within(Duration::from_secs(30), "the role");
    }
}

fn command(state: &Path) -> Command {
    let mut command = support::capyctl();
    command
        .env("CAPYCTL_STATE_DIR", state)
        .env_remove("CAPYCTL_VLLM_BIN")
        .env_remove("CAPYCTL_SGLANG_BIN");
    command
}

fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    let mut command = command(state);
    command.args(args);
    // `--format` and `--json` conflict; a caller that names a format keeps it.
    if !args.contains(&"--format") {
        command.arg("--json");
    }
    command.output().unwrap()
}

/// One chat completion through the server's router.
async fn chat_once(inference: &str, key: &str, content: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("http://{inference}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&json!({"model": "toy", "messages": [{"role": "user", "content": content}]}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

/// Which host answered, from the fake engine's answer.
fn answered_by(body: &str) -> &'static str {
    if body.contains("i3-spark-a") {
        "a"
    } else if body.contains("i3-spark-b") {
        "b"
    } else {
        "none"
    }
}

impl Cluster {
    /// Chat until `host` answers `times` in a row, within `within`.
    async fn steered_to(&self, host: &str, times: usize, within: Duration) {
        let started = Instant::now();
        let mut run = 0;
        while run < times {
            let (status, body) = self.chat().await;
            run = if status == 200 && answered_by(&body) == host {
                run + 1
            } else {
                0
            };
            assert!(
                started.elapsed() < within,
                "never steered to {host}: last {status} {body}"
            );
            if run < times {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

impl Cluster {
    /// Start the server, enroll and start both hosts, and wait until both are
    /// eligible. Returns the server role and the host roles.
    fn boot(&self) -> (Role, Vec<Option<Role>>) {
        let server = self.start("server", &self.server_state, &self.server_config);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self
            .manage(&["list", "hosts", "--format", "json"])
            .status
            .success()
        {
            assert!(Instant::now() < deadline, "the server never answered");
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut roles = Vec::new();
        for host in &self.hosts {
            let invitation = self.root.path().join(format!("{}.join", host.name));
            let out = self.manage(&[
                "invite",
                "host",
                "--name",
                &host.name,
                "--output",
                invitation.to_str().unwrap(),
            ]);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let out = cli(
                &host.state,
                &[
                    "join",
                    "host",
                    "--join-file",
                    invitation.to_str().unwrap(),
                    "--config",
                    host.config.to_str().unwrap(),
                ],
            );
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            roles.push(Some(self.start("host", &host.state, &host.config)));
        }
        self.eligible_hosts(2);
        (server, roles)
    }

    /// Deploy one model with two instances spread over both hosts and return
    /// its deployment id.
    fn deploy_two(&self) -> String {
        let golden: Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut deployment = golden["input"]["deployment"].clone();
        deployment["instances"] = json!(2);
        deployment["placement"] = json!({
            "hosts": ["i3-spark-a", "i3-spark-b"],
            "strategy": "spread",
            "max_per_host": 1
        });
        deployment["runtime_profile_revision"] =
            golden["input"]["host"]["runtime_profiles"]["local"]["revision"].clone();
        deployment["model"] = json!({
            "source": {"type": "local", "path": self.root.path().join("models/toy")},
            "content_fingerprint": "sha256:toy",
            "revision": "r1"
        });
        deployment["residency"] = json!("restart_only");
        deployment["engine_config"] = json!({"memory": {"kv_cache": "64MiB"}});
        deployment["request_deadline"] = json!("900s");
        deployment["devices"] = json!([{"sharing": "shared"}]);
        for (phase, bytes) in [
            ("cold", "256MiB"),
            ("ready", "128MiB"),
            ("parking", "128MiB"),
            ("parked", "32MiB"),
            ("wake", "256MiB"),
        ] {
            deployment["resources"][phase]["allocations"][0]["bytes"] = json!(bytes);
            deployment["resources"][phase]["allocations"][0]["host_kv_bytes"] = json!("0B");
            if phase != "parked" {
                deployment["resources"][phase]["devices"] = json!([{"sharing": "shared"}]);
            }
        }
        let file = self.root.path().join("deployment.json");
        std::fs::write(&file, deployment.to_string()).unwrap();
        let deployed = self.manage_json(&[
            "deploy",
            "model",
            "--file",
            file.to_str().unwrap(),
            "--activate",
            "--wait",
        ]);
        deployed["deployment"]["id"].as_str().unwrap().to_owned()
    }
}

/// T17 T18 T29 T38 T33 (ADR 0013 §10, D9): two instances on two hosts behind
/// one route. Concurrent requests spread across both; a long queue reported by
/// one host's agent shifts new work to the other; losing a host fails new
/// requests over while the request it had accepted is not replayed; and the
/// re-joined host serves again from the same, adopted engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_router_balances_two_instances_and_fails_over_on_host_loss() {
    let cluster = Cluster::new();
    let (server, mut roles) = cluster.boot();
    let id = cluster.deploy_two();
    cluster.ready_instances(&id, 2);
    cluster.served(Duration::from_secs(30)).await;
    let (a, b) = (&cluster.hosts[0], &cluster.hosts[1]);

    // T17 (D9): a concurrent burst spreads across both instances.
    a.set("delay", "0.4");
    b.set("delay", "0.4");
    let (served_a, served_b) = (a.served(), b.served());
    let burst: Vec<_> = (0..16)
        .map(|_| {
            let inference = cluster.inference.clone();
            let key = cluster.api_key();
            tokio::spawn(async move { chat_once(&inference, &key, "hello").await })
        })
        .collect();
    let mut answers = Vec::new();
    for request in burst {
        answers.push(request.await.unwrap());
    }
    for (status, body) in &answers {
        assert_eq!(*status, 200, "{body}");
    }
    let by_a = answers
        .iter()
        .filter(|(_, body)| answered_by(body) == "a")
        .count();
    let by_b = answers
        .iter()
        .filter(|(_, body)| answered_by(body) == "b")
        .count();
    assert_eq!(by_a + by_b, 16);
    assert!(by_a >= 5 && by_b >= 5, "a {by_a}, b {by_b}");
    assert_eq!(
        (a.served() - served_a, b.served() - served_b),
        (by_a, by_b),
        "each request ran on exactly one engine"
    );

    // T17 T29 (W8 -> router): host a's agent reports a long engine queue on
    // its next scrape; new requests then go to host b.
    a.set("delay", "0");
    b.set("delay", "0");
    a.set("running", "40");
    cluster.steered_to("b", 8, Duration::from_secs(20)).await;
    a.set("running", "0");

    // T38 (SPEC §10): host a is preferred and accepts a slow request; its
    // host agent is then lost.
    b.set("running", "40");
    cluster.steered_to("a", 3, Duration::from_secs(20)).await;
    a.set("delay", "5");
    let accepted_a = a.accepted();
    let accepted_b = b.accepted();
    let in_flight = {
        let inference = cluster.inference.clone();
        let key = cluster.api_key();
        tokio::spawn(async move { chat_once(&inference, &key, "slow").await })
    };
    let started = Instant::now();
    while a.accepted() == accepted_a {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "host a never accepted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(roles[0].take());
    // SPEC §13.2: losing the agent is not losing the engine it owns.
    assert!(alive(a.launches()[0]), "the engine outlives its agent");
    let (status, body) = in_flight.await.unwrap();
    assert_ne!(status, 200, "the accepted request cannot complete: {body}");
    assert_eq!(
        b.accepted(),
        accepted_b,
        "the accepted request was not replayed on b"
    );
    {
        let store = capyctl_store::Store::open(&cluster.server_state.join("srv.sqlite3")).unwrap();
        assert!(
            store
                .pending_dispatches(&id)
                .unwrap()
                .iter()
                .any(|d| d.uncertain),
            "the accepted request's lease stays charged, uncertain"
        );
    }
    // New requests fail over to host b at once, with no client-visible error.
    b.set("running", "0");
    for _ in 0..6 {
        let (status, body) = cluster.chat().await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(answered_by(&body), "b", "{body}");
    }

    // T33: host a re-joins; the server re-proves its adopted engine and the
    // router offers it work again. The engine was never relaunched.
    a.set("delay", "0");
    roles[0] = Some(cluster.start("host", &a.state, &a.config));
    cluster.eligible_hosts(2);
    b.set("running", "40");
    cluster.steered_to("a", 3, Duration::from_secs(60)).await;
    assert_eq!(a.launches().len(), 1, "adopted, not relaunched");

    // Every selection is logged with its inputs.
    let log = std::fs::read_to_string(cluster.server_state.with_extension("log")).unwrap();
    assert!(
        log.lines().any(|line| line.contains("\"router_selection\"")
            && line.contains("\"load_source\":\"engine\"")),
        "no logged selection with engine load"
    );
    // Host a was either skipped (its session lost or its gate closed) or
    // offered and refused before forwarding.
    assert!(
        [
            "\"router_failover\"",
            "host_session_lost",
            "dispatch_closed"
        ]
        .iter()
        .any(|evidence| log.contains(evidence)),
        "no logged exclusion or failover of the lost host"
    );

    for role in roles.into_iter().flatten() {
        role.stop();
    }
    server.stop();
}

/// Lines of a role's log that contain `needle`.
fn logged(path: &Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

fn signal(role: &Role, signal: i32) {
    role.0.signal(signal);
}

/// Owner decision 2026-09-23 — T33 T38 T17 T29: a frozen (SIGSTOPped) host
/// agent keeps its TCP connections, so before heartbeats the router kept
/// sending it requests until its session was declared lost. Now, within about
/// the 5 s suspend bound, the server suspends that host's dispatch and the
/// router stops selecting it (new requests go to the other instance); the
/// request it had in flight settles on its own and is never replayed; nothing
/// is released. On SIGCONT the same session is heard again, a fresh probe
/// re-proves the engine and the host rejoins. A frozen server, in turn, makes
/// each agent drop its session after the lost bound and reconnect, leaving its
/// engine running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frozen_host_agent_is_suspended_within_seconds_and_rejoins_after_a_probe() {
    let cluster = Cluster::new();
    let (server, mut roles) = cluster.boot();
    let id = cluster.deploy_two();
    cluster.ready_instances(&id, 2);
    cluster.served(Duration::from_secs(30)).await;
    let (a, b) = (&cluster.hosts[0], &cluster.hosts[1]);
    let server_log = cluster.server_state.with_extension("log");

    // Host a is preferred and accepts a slow request.
    b.set("running", "40");
    cluster.steered_to("a", 3, Duration::from_secs(20)).await;
    a.set("delay", "3");
    let accepted_a = a.accepted();
    let in_flight = {
        let inference = cluster.inference.clone();
        let key = cluster.api_key();
        tokio::spawn(async move { chat_once(&inference, &key, "slow").await })
    };
    let started = Instant::now();
    while a.accepted() == accepted_a {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "host a never accepted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Freeze host a's agent. Its engine keeps running in its own group.
    let silent_before = logged(&server_log, "no heartbeat for");
    let frozen = roles[0].as_ref().unwrap();
    signal(frozen, libc::SIGSTOP);
    let froze = Instant::now();
    while logged(&server_log, "no heartbeat for") == silent_before {
        assert!(
            froze.elapsed() < Duration::from_secs(9),
            "the frozen host was never suspended"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let suspended_after = froze.elapsed();
    assert!(
        suspended_after >= Duration::from_secs(4) && suspended_after < Duration::from_secs(8),
        "suspended after {suspended_after:?}"
    );
    eprintln!("frozen host agent suspended after {suspended_after:?}");
    assert!(alive(a.launches()[0]), "the engine is not touched");

    // New requests go to host b, promptly and without error.
    b.set("running", "0");
    let accepted_b = b.accepted();
    for _ in 0..6 {
        let answer = tokio::time::timeout(Duration::from_secs(10), cluster.chat())
            .await
            .expect("a new request is not held by the frozen host");
        assert_eq!(answer.0, 200, "{}", answer.1);
        assert_eq!(answered_by(&answer.1), "b", "{}", answer.1);
    }
    assert_eq!(b.accepted() - accepted_b, 6);
    let log = std::fs::read_to_string(&server_log).unwrap();
    assert!(
        log.lines().any(|line| line.contains("\"router_selection\"")
            && line.contains("host_unresponsive")),
        "no selection skipped the frozen host as unresponsive"
    );
    // Nothing is released while the host is silent.
    {
        let store = capyctl_store::Store::open(&cluster.server_state.join("srv.sqlite3")).unwrap();
        assert!(
            !store.pending_dispatches(&id).unwrap().is_empty(),
            "the in-flight request's lease stays charged"
        );
    }

    // Thaw inside the lost bound: the in-flight request settles on its own
    // engine, never replayed on b.
    let resumed_before = logged(&server_log, "heartbeats resumed");
    signal(frozen, libc::SIGCONT);
    let (status, body) = tokio::time::timeout(Duration::from_secs(60), in_flight)
        .await
        .expect("the in-flight request settles")
        .unwrap();
    if status == 200 {
        assert_eq!(answered_by(&body), "a", "{body}");
    }
    assert_eq!(
        b.accepted() - accepted_b,
        6,
        "the in-flight request was not replayed on b"
    );
    assert_eq!(
        a.accepted(),
        accepted_a + 1,
        "the in-flight request ran once"
    );
    let thawed = Instant::now();
    while logged(&server_log, "heartbeats resumed") == resumed_before {
        assert!(
            thawed.elapsed() < Duration::from_secs(10),
            "host a was never heard again"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Re-proven by a fresh probe on the same session, host a serves again from
    // the same engine.
    a.set("delay", "0");
    b.set("running", "40");
    cluster.steered_to("a", 3, Duration::from_secs(60)).await;
    assert_eq!(a.launches().len(), 1, "adopted, not relaunched");

    // A frozen server: each agent stops hearing it and, after the 12 s lost
    // bound, drops its session and reconnects. Engines keep running.
    let hosts_log: Vec<PathBuf> = cluster
        .hosts
        .iter()
        .map(|host| host.state.with_extension("log"))
        .collect();
    let dropped_before: Vec<usize> = hosts_log
        .iter()
        .map(|log| logged(log, "controller heartbeats stopped"))
        .collect();
    signal(&server, libc::SIGSTOP);
    let froze = Instant::now();
    loop {
        let dropped = hosts_log
            .iter()
            .zip(&dropped_before)
            .all(|(log, before)| logged(log, "controller heartbeats stopped") > *before);
        if dropped {
            break;
        }
        assert!(
            froze.elapsed() < Duration::from_secs(20),
            "the agents never dropped a silent controller's session"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        froze.elapsed() >= Duration::from_secs(11),
        "{:?}",
        froze.elapsed()
    );
    eprintln!(
        "agents dropped a frozen server's sessions after {:?}",
        froze.elapsed()
    );
    signal(&server, libc::SIGCONT);
    cluster.eligible_hosts(2);
    b.set("running", "0");
    a.set("running", "40");
    cluster.steered_to("b", 3, Duration::from_secs(60)).await;
    a.set("running", "0");
    b.set("running", "40");
    cluster.steered_to("a", 3, Duration::from_secs(60)).await;
    assert_eq!(a.launches().len(), 1, "a's engine was not relaunched");
    assert_eq!(b.launches().len(), 1, "b's engine was not relaunched");

    for role in roles.iter_mut().filter_map(Option::take) {
        role.stop();
    }
    server.stop();
}
